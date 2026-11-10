//! `stonewave connect`: the real device code sign-in (RFC 8628) against the
//! Stonewave account.
//!
//! The server shows a code, the admin enters it at `<account>/link` while
//! signed in, compares four words and confirms. The account then hands out a
//! short token that is bound to that person and their company. This build only
//! proves the token works (`GET /v1/cli/whoami`) and drops it: it is never
//! written to disk, never printed, and nothing on this machine is changed.
//!
//! HTTP goes through the system `curl` (the install command needs it anyway),
//! so TLS and the certificate store are the system's, and the binary stays
//! free of C code and static. Secrets reach curl on stdin, never in arguments.
//!
//! Everything here is plain data in, plain data out behind the [`Http`] trait,
//! so the flow is tested against a scripted fake and against a real local
//! server (see the tests at the bottom).

use std::fmt;
use std::io::Write;
use std::process::{Command, Stdio};
use std::time::Duration;

use serde_json::{Value, json};

/// Where the account lives unless `--account` or `STONEWAVE_ACCOUNT_URL` says otherwise.
pub const DEFAULT_ACCOUNT: &str = "https://account.stonewavetech.com";
/// Where the relay of Wave lives unless `--relay` or `STONEWAVE_RELAY_URL` says otherwise. The name is chosen, but the DNS entry only
/// exists once Stonewave switches the relay on; until then `connect` ends with "no connection" and changes nothing.
pub const DEFAULT_RELAY: &str = "https://relay.stonewavetech.com";
const CLIENT_ID: &str = "stonewave-cli";
const GRANT: &str = "urn:ietf:params:oauth:grant-type:device_code";
/// Polling never goes faster than this, whatever the server says.
const MIN_INTERVAL: u64 = 1;
/// Polling never goes slower than this; a server answering `slow_down` forever must not stall the admin for hours.
const MAX_INTERVAL: u64 = 60;
/// This many network errors in a row end the wait.
pub const MAX_FAILURES: u32 = 5;

// ------------------------------------------------------------------ errors

#[derive(Debug, PartialEq, Eq)]
pub enum ConnectError {
    /// No answer: curl missing, DNS, TLS, timeout.
    Network(String),
    /// The account answered with an error we cannot work with.
    Server(String),
    /// The answer did not have the shape of the contract.
    Protocol(String),
    /// The `--account` address is not acceptable.
    BadAddress(String),
}

impl fmt::Display for ConnectError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            ConnectError::Network(m) => write!(f, "no connection to the account: {m}"),
            ConnectError::Server(m) => write!(f, "the account refused: {m}"),
            ConnectError::Protocol(m) => write!(f, "unexpected answer from the account: {m}"),
            ConnectError::BadAddress(m) => write!(f, "{m}"),
        }
    }
}

/// Text from outside, cut and cleaned before it reaches a terminal: no control or escape codes, no invisible
/// or direction-changing characters, no long lines. Letters of any language stay (a company can be "Müller & Söhne").
pub(crate) fn tame(s: &str, max: usize) -> String {
    let bad = |c: char| {
        c.is_control()
            || matches!(c, '\u{200B}'..='\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2060}'..='\u{2064}' | '\u{2066}'..='\u{2069}' | '\u{FEFF}')
    };
    s.chars()
        .map(|c| if bad(c) { '?' } else { c })
        .take(max)
        .collect()
}

// ------------------------------------------------------------------ address

/// The account's base address: `https://host[:port]`, or `http://` for this machine only (tests, local development).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Account {
    base: String,
}

impl Account {
    pub fn parse(raw: &str) -> Result<Account, ConnectError> {
        let bad = |why: &str| {
            ConnectError::BadAddress(format!("account address {why}: {}", tame(raw, 80)))
        };
        let raw = raw.trim().trim_end_matches('/');
        let (scheme, rest) = raw.split_once("://").ok_or_else(|| bad("needs https://"))?;
        if rest.is_empty()
            || rest.contains(['/', '?', '#', '@', '\\', ' '])
            || !rest
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-:[]".contains(c))
        {
            return Err(bad("must be just a host, like https://account.example.com"));
        }
        let host = if let Some(v6) = rest.strip_prefix('[') {
            v6.split(']').next().unwrap_or("")
        } else {
            rest.split(':').next().unwrap_or("")
        };
        match scheme {
            "https" => {}
            "http" if matches!(host, "127.0.0.1" | "localhost" | "::1") => {}
            "http" => {
                return Err(bad(
                    "must use https (http is only allowed for this machine)",
                ));
            }
            _ => return Err(bad("needs https://")),
        }
        Ok(Account {
            base: format!("{scheme}://{rest}"),
        })
    }

    /// `https://host[:port]`
    pub fn base(&self) -> &str {
        &self.base
    }

    pub fn url(&self, path: &str) -> String {
        format!("{}{path}", self.base)
    }

    /// The host part, without the scheme.
    pub fn host_text(&self) -> String {
        self.base
            .split_once("://")
            .map_or(self.base.as_str(), |x| x.1)
            .to_owned()
    }

    /// What the admin types in the browser (shown without the scheme, like a link).
    pub fn link_text(&self) -> String {
        format!("{}/link", self.host_text())
    }

    pub fn is_https(&self) -> bool {
        self.base.starts_with("https://")
    }
}

// ------------------------------------------------------------------ http

pub struct Reply {
    pub status: u16,
    pub body: String,
}

pub trait Http {
    /// One request. `bearer` and `body` are secrets and must not be shown to other processes.
    fn send(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Option<&str>,
    ) -> Result<Reply, ConnectError>;
}

/// The system's curl, fed through a config on stdin.
pub struct Curl {
    pub allow_plain_http: bool,
}

/// A value inside double quotes in a curl config file.
pub(crate) fn cfg_quote(s: &str) -> String {
    let mut o = String::with_capacity(s.len() + 2);
    o.push('"');
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '\n' => o.push_str("\\n"),
            '\r' => o.push_str("\\r"),
            '\t' => o.push_str("\\t"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

fn curl_config(
    method: &str,
    url: &str,
    bearer: Option<&str>,
    body: Option<&str>,
    allow_plain_http: bool,
) -> String {
    let mut c = String::new();
    c.push_str("silent\nshow-error\nmax-time = 20\nconnect-timeout = 10\nmax-filesize = 1048576\n");
    c.push_str(if allow_plain_http {
        "proto = \"=http,https\"\n"
    } else {
        "proto = \"=https\"\n"
    });
    c.push_str("write-out = \"\\n%{http_code}\"\n");
    c.push_str(&format!(
        "user-agent = {}\n",
        cfg_quote(&format!("stonewave/{}", crate::VERSION))
    ));
    c.push_str(&format!(
        "request = {}\nurl = {}\n",
        cfg_quote(method),
        cfg_quote(url)
    ));
    c.push_str("header = \"Accept: application/json\"\n");
    if let Some(b) = bearer {
        c.push_str(&format!(
            "header = {}\n",
            cfg_quote(&format!("Authorization: Bearer {b}"))
        ));
    }
    if let Some(b) = body {
        c.push_str("header = \"Content-Type: application/json\"\n");
        c.push_str(&format!("data-binary = {}\n", cfg_quote(b)));
    }
    c
}

impl Http for Curl {
    fn send(
        &self,
        method: &str,
        url: &str,
        bearer: Option<&str>,
        body: Option<&str>,
    ) -> Result<Reply, ConnectError> {
        let config = curl_config(method, url, bearer, body, self.allow_plain_http);
        let mut child = Command::new("curl")
            .args(["--config", "-"])
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .map_err(|e| {
                if e.kind() == std::io::ErrorKind::NotFound {
                    ConnectError::Network("curl is not installed (install it with your package manager and try again)".into())
                } else {
                    ConnectError::Network(tame(&e.to_string(), 120))
                }
            })?;
        // curl reads its whole config before it answers; a failed write shows up in its exit code
        let _ = child
            .stdin
            .take()
            .map(|mut s| s.write_all(config.as_bytes()));
        let out = child
            .wait_with_output()
            .map_err(|e| ConnectError::Network(tame(&e.to_string(), 120)))?;
        if !out.status.success() {
            let why = String::from_utf8_lossy(&out.stderr);
            let first = why.lines().next().unwrap_or("curl failed");
            return Err(ConnectError::Network(tame(
                first.trim_start_matches("curl: "),
                160,
            )));
        }
        let text = String::from_utf8_lossy(&out.stdout);
        let (body, code) = text
            .rsplit_once('\n')
            .ok_or_else(|| ConnectError::Protocol("no status line".into()))?;
        let status = code
            .trim()
            .parse::<u16>()
            .map_err(|_| ConnectError::Protocol("no status code".into()))?;
        Ok(Reply {
            status,
            body: body.to_owned(),
        })
    }
}

// ------------------------------------------------------------------ protocol

#[derive(Debug, PartialEq, Eq)]
pub struct Started {
    pub device_code: String,
    pub user_code: String,
    pub expires_in: u64,
    pub interval: u64,
    pub words: [String; 4],
}

/// The token never shows up in `Debug` or `Display`.
pub struct Token(String);

impl Token {
    pub(crate) fn secret(&self) -> &str {
        &self.0
    }

    #[cfg(test)]
    pub(crate) fn for_test(s: &str) -> Token {
        Token(s.to_owned())
    }
}

impl fmt::Debug for Token {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Token(..)")
    }
}

#[derive(Debug)]
pub struct Approved {
    pub token: Token,
    pub company: String,
    pub company_name: String,
}

#[derive(Debug)]
pub enum Poll {
    Pending,
    /// The account asked for a longer interval (seconds).
    SlowDown(u64),
    Approved(Approved),
    Denied,
    Expired,
}

fn json(r: &Reply) -> Result<Value, ConnectError> {
    serde_json::from_str::<Value>(&r.body)
        .map_err(|_| ConnectError::Protocol(format!("HTTP {} without JSON", r.status)))
}

fn str_field<'a>(v: &'a Value, k: &str) -> Result<&'a str, ConnectError> {
    v.get(k)
        .and_then(Value::as_str)
        .ok_or_else(|| ConnectError::Protocol(format!("field {k} missing")))
}

fn shaped(
    s: &str,
    ok: impl Fn(char) -> bool,
    min: usize,
    max: usize,
    what: &str,
) -> Result<String, ConnectError> {
    if s.len() < min || s.len() > max || !s.chars().all(ok) {
        return Err(ConnectError::Protocol(format!(
            "{what} has an unexpected form"
        )));
    }
    Ok(s.to_owned())
}

/// The error code of an RFC 8628 error answer, or of the house shape `{"error": {"code": ...}}`.
pub(crate) fn error_code(v: &Value) -> String {
    let e = v.get("error");
    let code = e
        .and_then(Value::as_str)
        .or_else(|| e.and_then(|x| x.get("code")).and_then(Value::as_str));
    tame(code.unwrap_or("unknown_error"), 40)
}

/// Starts a sign-in: the account answers with the codes and the four words.
pub fn start(
    http: &dyn Http,
    acct: &Account,
    host: &str,
    os: &str,
) -> Result<Started, ConnectError> {
    let body = json!({ "client_id": CLIENT_ID, "host": host, "os": os, "version": crate::VERSION })
        .to_string();
    let r = http.send("POST", &acct.url("/v1/device/code"), None, Some(&body))?;
    let v = json(&r)?;
    if r.status == 429 {
        return Err(ConnectError::Server(
            "too many tries from this address; wait a few minutes".into(),
        ));
    }
    if r.status != 200 {
        return Err(ConnectError::Server(error_code(&v)));
    }
    let words = v
        .get("words")
        .and_then(Value::as_array)
        .filter(|w| w.len() == 4)
        .ok_or_else(|| ConnectError::Protocol("words missing".into()))?;
    let mut w: [String; 4] = Default::default();
    for (i, x) in words.iter().enumerate() {
        w[i] = shaped(
            x.as_str().unwrap_or(""),
            |c| c.is_ascii_lowercase(),
            2,
            12,
            "word",
        )?;
    }
    let num = |k: &str| {
        v.get(k)
            .and_then(Value::as_u64)
            .ok_or_else(|| ConnectError::Protocol(format!("field {k} missing")))
    };
    Ok(Started {
        device_code: shaped(
            str_field(&v, "device_code")?,
            |c| c.is_ascii_alphanumeric() || c == '-' || c == '_',
            16,
            200,
            "device code",
        )?,
        user_code: shaped(
            str_field(&v, "user_code")?,
            |c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '-',
            4,
            24,
            "code",
        )?,
        expires_in: num("expires_in")?.clamp(30, 3600),
        interval: num("interval")?.clamp(MIN_INTERVAL, MAX_INTERVAL),
        words: w,
    })
}

/// One poll. RFC 8628 section 3.5 errors become [`Poll`] values; anything else is an error.
pub fn poll_once(http: &dyn Http, acct: &Account, device_code: &str) -> Result<Poll, ConnectError> {
    let body = json!({ "grant_type": GRANT, "device_code": device_code, "client_id": CLIENT_ID })
        .to_string();
    let r = http.send("POST", &acct.url("/v1/device/token"), None, Some(&body))?;
    if r.status >= 500 {
        // a proxy or a restart in between: not an answer of the account, so the wait loop tries again
        return Err(ConnectError::Network(format!(
            "the account answered HTTP {}",
            r.status
        )));
    }
    let v = json(&r)?;
    if r.status == 200 {
        return Ok(Poll::Approved(Approved {
            token: Token(shaped(
                str_field(&v, "access_token")?,
                |c| c.is_ascii_alphanumeric() || "-_.".contains(c),
                20,
                2048,
                "token",
            )?),
            company: shaped(
                str_field(&v, "company")?,
                |c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-',
                2,
                40,
                "company",
            )?,
            company_name: tame(
                v.get("company_name").and_then(Value::as_str).unwrap_or(""),
                120,
            ),
        }));
    }
    match error_code(&v).as_str() {
        "authorization_pending" => Ok(Poll::Pending),
        "slow_down" => Ok(Poll::SlowDown(
            v.get("interval")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .clamp(MIN_INTERVAL, MAX_INTERVAL),
        )),
        "access_denied" => Ok(Poll::Denied),
        "expired_token" | "invalid_grant" => Ok(Poll::Expired),
        "rate_limited" => Ok(Poll::SlowDown(MAX_INTERVAL.min(30))),
        other => Err(ConnectError::Server(other.to_owned())),
    }
}

/// Asks the account who the token belongs to; proves the token works. Returns the company name.
pub fn whoami(http: &dyn Http, acct: &Account, token: &Token) -> Result<String, ConnectError> {
    let r = http.send(
        "GET",
        &acct.url("/v1/cli/whoami"),
        Some(token.secret()),
        None,
    )?;
    let v = json(&r)?;
    if r.status != 200 {
        return Err(ConnectError::Server(error_code(&v)));
    }
    Ok(tame(
        v.get("company_name").and_then(Value::as_str).unwrap_or(""),
        120,
    ))
}

// ------------------------------------------------------------------ waiting

#[derive(Debug, PartialEq, Eq)]
pub enum Wait {
    Approved,
    Denied,
    Expired,
}

/// Time and screen for the wait loop. The real one draws a spinner and listens for Ctrl-C; the test one is a fake clock.
pub trait Pace {
    /// Time since the sign-in started.
    fn elapsed(&self) -> Duration;
    /// Let a moment pass (about 50 ms). `left` is the time until the code expires. May fail with Ctrl-C.
    fn tick(&mut self, left: Duration) -> Result<(), crate::term::Error>;
}

pub struct Waited {
    pub end: Wait,
    pub approved: Option<Approved>,
}

/// Polls until approved, denied or expired. Obeys `slow_down`; gives up after [`MAX_FAILURES`] network errors in a row.
pub fn wait_for_approval(
    http: &dyn Http,
    acct: &Account,
    st: &Started,
    pace: &mut dyn Pace,
) -> Result<Result<Waited, ConnectError>, crate::term::Error> {
    let expires = Duration::from_secs(st.expires_in);
    let mut interval = Duration::from_secs(st.interval);
    let mut next = interval;
    let mut failures = 0;
    loop {
        let now = pace.elapsed();
        if now >= expires {
            return Ok(Ok(Waited {
                end: Wait::Expired,
                approved: None,
            }));
        }
        if now >= next {
            match poll_once(http, acct, &st.device_code) {
                Ok(Poll::Pending) => failures = 0,
                Ok(Poll::SlowDown(s)) => {
                    failures = 0;
                    interval = Duration::from_secs(s).max(interval);
                }
                Ok(Poll::Approved(a)) => {
                    return Ok(Ok(Waited {
                        end: Wait::Approved,
                        approved: Some(a),
                    }));
                }
                Ok(Poll::Denied) => {
                    return Ok(Ok(Waited {
                        end: Wait::Denied,
                        approved: None,
                    }));
                }
                Ok(Poll::Expired) => {
                    return Ok(Ok(Waited {
                        end: Wait::Expired,
                        approved: None,
                    }));
                }
                Err(ConnectError::Network(m)) => {
                    failures += 1;
                    if failures >= MAX_FAILURES {
                        return Ok(Err(ConnectError::Network(m)));
                    }
                }
                Err(e) => return Ok(Err(e)),
            }
            next = pace.elapsed() + interval;
        }
        pace.tick(expires.saturating_sub(pace.elapsed()))?;
    }
}

// ------------------------------------------------------------------ tests

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::rc::Rc;

    fn acct() -> Account {
        Account::parse("https://account.test").unwrap()
    }

    #[test]
    fn address_rules() {
        assert_eq!(
            Account::parse("https://account.stonewavetech.com/")
                .unwrap()
                .url("/x"),
            "https://account.stonewavetech.com/x"
        );
        assert_eq!(
            Account::parse("https://a.test:8443").unwrap().link_text(),
            "a.test:8443/link"
        );
        assert!(Account::parse("http://127.0.0.1:3013").is_ok());
        assert!(Account::parse("http://localhost:1").is_ok());
        assert!(Account::parse("http://[::1]:3013").is_ok());
        for bad in [
            "http://account.stonewavetech.com",
            "account.test",
            "ftp://a.test",
            "https://",
            "https://a.test/path",
            "https://a.test?x=1",
            "https://user@a.test",
            "https://a.test#f",
            "https://a b.test",
            "http://127.0.0.1.evil.com",
            "http://localhost.evil.com",
            "https://a.test\\x",
        ] {
            assert!(Account::parse(bad).is_err(), "{bad}");
        }
    }

    #[test]
    fn curl_config_keeps_secrets_out_of_arguments_and_escapes() {
        let c = curl_config(
            "POST",
            "https://a.test/x",
            Some("tok.en"),
            Some("{\"a\":\"b\\\"\n\"}"),
            false,
        );
        assert!(c.contains("proto = \"=https\""));
        assert!(c.contains("header = \"Authorization: Bearer tok.en\""));
        assert!(
            c.contains("data-binary = \"{\\\"a\\\":\\\"b\\\\\\\"\\n\\\"}\""),
            "{c}"
        );
        assert!(!c.contains("{\n"), "no raw newline inside a value");
        assert!(
            curl_config("GET", "http://127.0.0.1:1/x", None, None, true).contains("=http,https")
        );
        // a hostile value cannot add a config line
        let evil = curl_config(
            "GET",
            "https://a.test/\"\nurl = \"https://evil.test/",
            None,
            None,
            false,
        );
        assert_eq!(evil.matches("\nurl = ").count(), 1, "{evil}");
    }

    /// One request as the fake saw it: method, url, bearer, body, virtual time.
    type Seen = (String, String, Option<String>, Option<String>, Duration);

    /// Scripted answers; remembers the requests and the virtual time of each.
    struct Fake {
        replies: RefCell<VecDeque<Result<Reply, ConnectError>>>,
        seen: RefCell<Vec<Seen>>,
        clock: Rc<Cell<Duration>>,
    }

    impl Fake {
        fn new(clock: &Rc<Cell<Duration>>, replies: Vec<Result<Reply, ConnectError>>) -> Fake {
            Fake {
                replies: RefCell::new(replies.into()),
                seen: RefCell::new(vec![]),
                clock: clock.clone(),
            }
        }
    }

    impl Http for Fake {
        fn send(
            &self,
            m: &str,
            u: &str,
            b: Option<&str>,
            body: Option<&str>,
        ) -> Result<Reply, ConnectError> {
            self.seen.borrow_mut().push((
                m.into(),
                u.into(),
                b.map(Into::into),
                body.map(Into::into),
                self.clock.get(),
            ));
            self.replies
                .borrow_mut()
                .pop_front()
                .expect("unexpected request")
        }
    }

    fn ok(body: &str) -> Result<Reply, ConnectError> {
        Ok(Reply {
            status: 200,
            body: body.into(),
        })
    }
    fn err(status: u16, code: &str) -> Result<Reply, ConnectError> {
        Ok(Reply {
            status,
            body: format!("{{\"error\":\"{code}\"}}"),
        })
    }
    const STARTED: &str = r#"{"device_code":"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG","user_code":"WAVE-7K4Q-X2MP","verification_uri":"https://account.test/link","expires_in":600,"interval":5,"words":["amber","river","cedar","fox"]}"#;
    const TOKEN: &str = r#"{"access_token":"aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbb.cccccccccccccc","token_type":"Bearer","expires_in":900,"scope":"server:register","company":"acme","company_name":"Acme GmbH"}"#;

    struct Clock(Rc<Cell<Duration>>, u32);
    impl Pace for Clock {
        fn elapsed(&self) -> Duration {
            self.0.get()
        }
        fn tick(&mut self, _: Duration) -> Result<(), crate::term::Error> {
            self.1 += 1;
            self.0.set(self.0.get() + Duration::from_millis(100));
            Ok(())
        }
    }

    #[test]
    fn start_reads_the_contract_and_sends_what_the_account_needs() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(&clock, vec![ok(STARTED)]);
        let s = start(&f, &acct(), "web-01", "Ubuntu 24.04").unwrap();
        assert_eq!(s.user_code, "WAVE-7K4Q-X2MP");
        assert_eq!(
            s.words,
            ["amber", "river", "cedar", "fox"].map(String::from)
        );
        assert_eq!((s.expires_in, s.interval), (600, 5));
        let seen = f.seen.borrow();
        assert_eq!(
            (seen[0].0.as_str(), seen[0].1.as_str()),
            ("POST", "https://account.test/v1/device/code")
        );
        let body: Value = serde_json::from_str(seen[0].3.as_ref().unwrap()).unwrap();
        assert_eq!(body["client_id"], "stonewave-cli");
        assert_eq!(body["host"], "web-01");
        assert!(seen[0].2.is_none(), "no bearer on start");
    }

    #[test]
    fn start_refuses_bad_answers() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let bad = |b: &str| start(&Fake::new(&clock, vec![ok(b)]), &acct(), "h", "o");
        assert!(matches!(bad("not json"), Err(ConnectError::Protocol(_))));
        assert!(
            matches!(
                bad(&STARTED.replace("\"fox\"", "\"\\u001b[31mfox\"")),
                Err(ConnectError::Protocol(_))
            ),
            "escape codes in a word"
        );
        assert!(
            matches!(
                bad(&STARTED.replace(",\"fox\"", "")),
                Err(ConnectError::Protocol(_))
            ),
            "three words"
        );
        assert!(matches!(
            bad(&STARTED.replace("WAVE-7K4Q-X2MP", "WAVE 7K4Q\\n")),
            Err(ConnectError::Protocol(_))
        ));
        assert!(matches!(
            bad(&STARTED.replace("\"device_code\"", "\"dc\"")),
            Err(ConnectError::Protocol(_))
        ));
        let limited = start(
            &Fake::new(&clock, vec![err(429, "rate_limited")]),
            &acct(),
            "h",
            "o",
        );
        assert!(matches!(limited, Err(ConnectError::Server(m)) if m.contains("too many")));
        let nope = start(
            &Fake::new(&clock, vec![err(400, "invalid_client")]),
            &acct(),
            "h",
            "o",
        );
        assert_eq!(
            nope.unwrap_err(),
            ConnectError::Server("invalid_client".into())
        );
    }

    #[test]
    fn tame_keeps_letters_and_drops_control_and_direction_tricks() {
        assert_eq!(tame("Müller & Söhne GmbH", 40), "Müller & Söhne GmbH");
        assert_eq!(tame("a\u{1b}[31mb\nc\u{202e}d", 40), "a?[31mb?c?d");
        assert_eq!(tame("abcdef", 3), "abc");
    }

    #[test]
    fn a_server_error_in_between_is_retried_not_fatal() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let bad = Ok(Reply {
            status: 502,
            body: "<html>Bad gateway</html>".into(),
        });
        let f = Fake::new(&clock, vec![bad, ok(TOKEN)]);
        let w = wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0))
            .unwrap()
            .unwrap();
        assert_eq!(w.end, Wait::Approved);
    }

    #[test]
    fn poll_maps_the_rfc_errors() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let p = |r| poll_once(&Fake::new(&clock, vec![r]), &acct(), "dc").unwrap();
        assert!(matches!(
            p(err(400, "authorization_pending")),
            Poll::Pending
        ));
        assert!(matches!(
            p(Ok(Reply {
                status: 400,
                body: r#"{"error":"slow_down","interval":10}"#.into()
            })),
            Poll::SlowDown(10)
        ));
        assert!(matches!(p(err(400, "access_denied")), Poll::Denied));
        assert!(matches!(p(err(400, "expired_token")), Poll::Expired));
        assert!(matches!(p(err(400, "invalid_grant")), Poll::Expired));
        assert!(
            matches!(p(ok(TOKEN)), Poll::Approved(a) if a.company == "acme" && a.company_name == "Acme GmbH")
        );
        assert!(
            poll_once(
                &Fake::new(&clock, vec![err(400, "invalid_client")]),
                &acct(),
                "dc"
            )
            .is_err()
        );
        // house-shaped errors read too
        let house = Ok(Reply {
            status: 429,
            body: r#"{"error":{"code":"rate_limited"}}"#.into(),
        });
        assert!(matches!(p(house), Poll::SlowDown(_)));
    }

    #[test]
    fn token_stays_secret_in_debug_and_travels_only_as_bearer() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let Poll::Approved(a) =
            poll_once(&Fake::new(&clock, vec![ok(TOKEN)]), &acct(), "dc").unwrap()
        else {
            panic!()
        };
        assert!(!format!("{a:?}").contains("aaaaaaaa"));
        let f = Fake::new(
            &clock,
            vec![ok(r#"{"company":"acme","company_name":"Acme GmbH"}"#)],
        );
        assert_eq!(whoami(&f, &acct(), &a.token).unwrap(), "Acme GmbH");
        let seen = f.seen.borrow();
        assert_eq!(
            (seen[0].0.as_str(), seen[0].1.as_str()),
            ("GET", "https://account.test/v1/cli/whoami")
        );
        assert_eq!(
            seen[0].2.as_deref(),
            Some("aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbb.cccccccccccccc")
        );
    }

    fn started() -> Started {
        Started {
            device_code: "d".repeat(43),
            user_code: "WAVE-AAAA-BBBB".into(),
            expires_in: 600,
            interval: 5,
            words: ["a", "b", "c", "d"].map(String::from),
        }
    }

    #[test]
    fn waits_pending_then_approved_at_the_interval() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(
            &clock,
            vec![
                err(400, "authorization_pending"),
                err(400, "authorization_pending"),
                ok(TOKEN),
            ],
        );
        let w = wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0))
            .unwrap()
            .unwrap();
        assert_eq!(w.end, Wait::Approved);
        let times: Vec<u64> = f.seen.borrow().iter().map(|s| s.4.as_secs()).collect();
        assert_eq!(times, vec![5, 10, 15], "never before the interval");
    }

    #[test]
    fn slow_down_makes_the_wait_longer_for_good() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let slow = Ok(Reply {
            status: 400,
            body: r#"{"error":"slow_down","interval":10}"#.into(),
        });
        let f = Fake::new(
            &clock,
            vec![slow, err(400, "authorization_pending"), ok(TOKEN)],
        );
        wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0))
            .unwrap()
            .unwrap();
        let times: Vec<u64> = f.seen.borrow().iter().map(|s| s.4.as_secs()).collect();
        assert_eq!(times, vec![5, 15, 25]);
    }

    #[test]
    fn denied_and_expired_end_the_wait() {
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(&clock, vec![err(400, "access_denied")]);
        assert_eq!(
            wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0))
                .unwrap()
                .unwrap()
                .end,
            Wait::Denied
        );
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(&clock, vec![err(400, "expired_token")]);
        assert_eq!(
            wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0))
                .unwrap()
                .unwrap()
                .end,
            Wait::Expired
        );
        // the local clock runs out even when the account keeps saying "pending"
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(
            &clock,
            (0..200)
                .map(|_| err(400, "authorization_pending"))
                .collect(),
        );
        let mut st = started();
        st.expires_in = 30;
        assert_eq!(
            wait_for_approval(&f, &acct(), &st, &mut Clock(clock.clone(), 0))
                .unwrap()
                .unwrap()
                .end,
            Wait::Expired
        );
        assert!(f.seen.borrow().len() <= 6);
    }

    #[test]
    fn network_errors_are_retried_a_few_times_then_reported() {
        let net = || Err(ConnectError::Network("timeout".into()));
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(
            &clock,
            vec![
                net(),
                net(),
                err(400, "authorization_pending"),
                net(),
                net(),
                ok(TOKEN),
            ],
        );
        assert_eq!(
            wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0))
                .unwrap()
                .unwrap()
                .end,
            Wait::Approved
        );
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(&clock, (0..MAX_FAILURES).map(|_| net()).collect());
        assert!(matches!(
            wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0)).unwrap(),
            Err(ConnectError::Network(_))
        ));
        // a protocol problem stops at once
        let clock = Rc::new(Cell::new(Duration::ZERO));
        let f = Fake::new(&clock, vec![err(400, "server_error")]);
        assert!(matches!(
            wait_for_approval(&f, &acct(), &started(), &mut Clock(clock.clone(), 0)).unwrap(),
            Err(ConnectError::Server(_))
        ));
    }

    // ---- against a real local server, through the real curl ----

    fn have_curl() -> bool {
        Command::new("curl")
            .arg("--version")
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    type Route = (&'static str, &'static str, u16, String);
    type Logged = Vec<(String, String, String)>;

    /// A tiny HTTP server: answers each request from `routes` (method + path -> status + body) and records (path, authorization, body).
    fn serve(routes: Vec<Route>) -> (String, std::thread::JoinHandle<Logged>) {
        use std::io::{BufRead, BufReader, Read};
        let l = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", l.local_addr().unwrap());
        let h = std::thread::spawn(move || {
            let mut log = vec![];
            for (method, path, status, body) in routes {
                let (s, _) = l.accept().unwrap();
                let mut rd = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                rd.read_line(&mut line).unwrap();
                let (mut auth, mut len) = (String::new(), 0usize);
                loop {
                    let mut h = String::new();
                    rd.read_line(&mut h).unwrap();
                    if h.trim().is_empty() {
                        break;
                    }
                    let l = h.to_ascii_lowercase();
                    if let Some(v) = l.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if l.starts_with("authorization:") {
                        auth = h.split_once(':').unwrap().1.trim().to_owned();
                    }
                }
                let mut b = vec![0; len];
                rd.read_exact(&mut b).unwrap();
                assert_eq!(line.trim(), format!("{method} {path} HTTP/1.1"));
                log.push((path.to_owned(), auth, String::from_utf8(b).unwrap()));
                let mut s = s;
                write!(s, "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n\r\n{body}", body.len()).unwrap();
            }
            log
        });
        (addr, h)
    }

    #[test]
    fn end_to_end_against_a_local_server_with_the_real_curl() {
        if !have_curl() {
            eprintln!("curl not installed: skipped");
            return;
        }
        let started = STARTED.replace("\"interval\":5", "\"interval\":1");
        let (addr, server) = serve(vec![
            ("POST", "/v1/device/code", 200, started),
            (
                "POST",
                "/v1/device/token",
                400,
                r#"{"error":"authorization_pending"}"#.into(),
            ),
            ("POST", "/v1/device/token", 200, TOKEN.into()),
            (
                "GET",
                "/v1/cli/whoami",
                200,
                r#"{"company":"acme","company_name":"Acme GmbH"}"#.into(),
            ),
        ]);
        let acct = Account::parse(&addr).unwrap();
        let http = Curl {
            allow_plain_http: !acct.is_https(),
        };
        let st = start(&http, &acct, "web-01", "Ubuntu").unwrap();
        struct Real(std::time::Instant);
        impl Pace for Real {
            fn elapsed(&self) -> Duration {
                self.0.elapsed()
            }
            fn tick(&mut self, _: Duration) -> Result<(), crate::term::Error> {
                std::thread::sleep(Duration::from_millis(50));
                Ok(())
            }
        }
        let w = wait_for_approval(&http, &acct, &st, &mut Real(std::time::Instant::now()))
            .unwrap()
            .unwrap();
        let a = w.approved.unwrap();
        assert_eq!(whoami(&http, &acct, &a.token).unwrap(), "Acme GmbH");
        let log = server.join().unwrap();
        assert!(log[0].2.contains("\"client_id\":\"stonewave-cli\""));
        assert!(log[2].2.contains(&st.device_code));
        assert_eq!(
            log[3].1,
            "Bearer aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbb.cccccccccccccc"
        );
        assert_eq!(log[0].1, "");
    }

    #[test]
    fn plain_http_is_refused_unless_allowed_and_a_dead_port_is_a_network_error() {
        if !have_curl() {
            return;
        }
        let strict = Curl {
            allow_plain_http: false,
        };
        assert!(matches!(
            strict.send("GET", "http://127.0.0.1:1/x", None, None),
            Err(ConnectError::Network(_))
        ));
        let lax = Curl {
            allow_plain_http: true,
        };
        assert!(matches!(
            lax.send("GET", "http://127.0.0.1:1/x", None, None),
            Err(ConnectError::Network(_))
        ));
    }
}
