//! The four calls to the Wave API behind `agent connect`: mint, status, rotate, revoke.
//!
//! Wire contract (JSON; errors are `{"error": {"code": "...", "message": "..."}}`):
//!
//! - `POST /v1/agent-connect/mint` with the sign-in token as Bearer and `{host, os, version, engine}`
//!   answers 201 `{key, agent: {id, key, name}, team: {key, name}, company, expires_at, mcp_url?}`
//! - `GET /v1/agent-connect/status` with the agent key answers 200 `{agent, team, company, engine, key_expires_at, last_seen_at, last_session}`
//! - `POST /v1/agent-connect/rotate` with the agent key and `{}` answers 200 `{key, expires_at}`
//! - `POST /v1/agent-connect/revoke` with the agent key and `{}` answers 200 `{revoked: n}`
//!
//! The key only travels as the answer of mint and rotate and as the Bearer header (on curl's stdin, see `connect::Curl`).

use std::fmt;

use serde_json::{Value, json};

use super::key::{Key, scrub};
use crate::connect::{Account, ConnectError, Http, Reply, Token, error_code, is_agent_slug, tame};

#[derive(Debug, PartialEq, Eq)]
pub enum WaveError {
    /// No answer: curl missing, DNS, TLS, timeout, or a 5xx from a proxy.
    Network(String),
    /// Wave answered with an error.
    Api {
        status: u16,
        code: String,
        message: Option<String>,
        retry_after: Option<u64>,
    },
    /// The answer did not have the shape of the contract.
    Protocol(String),
}

impl WaveError {
    #[cfg(test)]
    pub fn code(&self) -> Option<&str> {
        match self {
            WaveError::Api { code, .. } => Some(code),
            _ => None,
        }
    }

    pub fn is_unauthorized(&self) -> bool {
        matches!(self, WaveError::Api { status: 401, .. })
    }

    #[cfg(test)]
    pub fn is_network(&self) -> bool {
        matches!(self, WaveError::Network(_))
    }
}

impl fmt::Display for WaveError {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let text = match self {
            WaveError::Network(m) => format!("no connection to Wave: {m}"),
            WaveError::Protocol(m) => format!("unexpected answer from Wave: {m}"),
            WaveError::Api {
                status,
                code,
                message,
                retry_after,
            } => {
                let what = match code.as_str() {
                    "agent_not_ready" => "Wave is still setting up the agent".to_owned(),
                    "already_minted" => "this sign-in already produced a key; run `stonewave agent connect` again for a new code".to_owned(),
                    "forbidden" => "you need to be an admin of this company in Wave".to_owned(),
                    "agent_connect_not_enabled" => "agent connect is not switched on for this company yet".to_owned(),
                    "invalid_token" if *status == 401 => "the key or sign-in is not valid (any more)".to_owned(),
                    "token_expired" => "the sign-in ran out; run `stonewave agent connect` again".to_owned(),
                    "too_many_requests" => match retry_after {
                        Some(s) => format!("too many tries; Wave asks to wait {s} s"),
                        None => "too many tries; wait a moment".to_owned(),
                    },
                    other => match message {
                        Some(m) if !m.is_empty() => format!("{other} ({m})"),
                        _ => other.to_owned(),
                    },
                };
                format!("Wave refused (HTTP {status}): {what}")
            }
        };
        f.write_str(&scrub(&text))
    }
}

impl From<ConnectError> for WaveError {
    fn from(e: ConnectError) -> Self {
        match e {
            ConnectError::Network(m) => WaveError::Network(m),
            ConnectError::Protocol(m) => WaveError::Protocol(m),
            ConnectError::Server(m) | ConnectError::BadAddress(m) => WaveError::Protocol(m),
        }
    }
}

fn body_json(r: &Reply) -> Result<Value, WaveError> {
    serde_json::from_str::<Value>(&r.body)
        .map_err(|_| WaveError::Protocol(format!("HTTP {} without JSON", r.status)))
}

/// An error answer, or the 5xx of a proxy in between (not an answer of Wave: a network problem).
fn failure(r: &Reply) -> WaveError {
    if r.status >= 500 && serde_json::from_str::<Value>(&r.body).is_err() {
        return WaveError::Network(format!("Wave answered HTTP {}", r.status));
    }
    let v = serde_json::from_str::<Value>(&r.body).unwrap_or(Value::Null);
    let message = v
        .get("error")
        .and_then(|e| e.get("message"))
        .and_then(Value::as_str)
        .map(|m| scrub(&tame(m, 160)));
    WaveError::Api {
        status: r.status,
        code: if v.is_null() {
            "unknown_error".into()
        } else {
            scrub(&error_code(&v))
        },
        message,
        retry_after: r.retry_after,
    }
}

fn text<'a>(v: &'a Value, path: &[&str]) -> Result<&'a str, WaveError> {
    let mut cur = v;
    for p in path {
        cur = cur
            .get(p)
            .ok_or_else(|| WaveError::Protocol(format!("field {} missing", path.join("."))))?;
    }
    cur.as_str()
        .ok_or_else(|| WaveError::Protocol(format!("field {} is not text", path.join("."))))
}

fn slug(v: &Value, path: &[&str]) -> Result<String, WaveError> {
    let s = text(v, path)?;
    if !is_agent_slug(s) {
        return Err(WaveError::Protocol(format!(
            "{} has an unexpected form",
            path.join(".")
        )));
    }
    Ok(s.to_owned())
}

fn name(v: &Value, path: &[&str]) -> Result<String, WaveError> {
    Ok(tame(text(v, path)?, 80))
}

fn key_of(v: &Value, field: &str) -> Result<Key, WaveError> {
    Key::parse(text(v, &[field])?)
        .ok_or_else(|| WaveError::Protocol(format!("{field} has an unexpected form")))
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRef {
    pub id: String,
    pub key: String,
    pub name: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TeamRef {
    pub key: String,
    pub name: String,
}

fn agent_ref(v: &Value) -> Result<AgentRef, WaveError> {
    let id = tame(text(v, &["agent", "id"])?, 80);
    if id.is_empty() {
        return Err(WaveError::Protocol("agent.id is empty".into()));
    }
    Ok(AgentRef {
        id,
        key: slug(v, &["agent", "key"])?,
        name: name(v, &["agent", "name"])?,
    })
}

fn team_ref(v: &Value) -> Result<TeamRef, WaveError> {
    Ok(TeamRef {
        key: slug(v, &["team", "key"])?,
        name: name(v, &["team", "name"])?,
    })
}

/// A timestamp from Wave, as text we keep (checked for form, shown as a date).
fn stamp(v: &Value, path: &[&str]) -> Result<String, WaveError> {
    let s = text(v, path)?;
    if super::timefmt::parse_iso(s).is_none() {
        return Err(WaveError::Protocol(format!(
            "{} is not a date",
            path.join(".")
        )));
    }
    Ok(s.to_owned())
}

pub struct Minted {
    pub key: Key,
    pub agent: AgentRef,
    pub team: TeamRef,
    pub company: String,
    pub expires_at: String,
    /// Only when Wave knows its public address.
    pub mcp_url: Option<String>,
}

impl fmt::Debug for Minted {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Minted")
            .field("key", &self.key)
            .field("agent", &self.agent)
            .field("team", &self.team)
            .field("company", &self.company)
            .field("expires_at", &self.expires_at)
            .field("mcp_url", &self.mcp_url)
            .finish()
    }
}

pub struct MintRequest<'a> {
    pub host: &'a str,
    pub os: &'a str,
    pub engine: &'a str,
}

fn url(wave: &Account, route: &str) -> String {
    wave.url(&format!("/v1/agent-connect/{route}"))
}

/// Exchanges the sign-in token for the agent key (once per sign-in).
pub fn mint(
    http: &dyn Http,
    wave: &Account,
    token: &Token,
    req: &MintRequest<'_>,
) -> Result<Minted, WaveError> {
    let body =
        json!({ "host": req.host, "os": req.os, "version": crate::VERSION, "engine": req.engine })
            .to_string();
    let r = http.send(
        "POST",
        &url(wave, "mint"),
        Some(token.secret()),
        Some(&body),
    )?;
    if r.status != 201 && r.status != 200 {
        return Err(failure(&r));
    }
    let v = body_json(&r)?;
    Ok(Minted {
        key: key_of(&v, "key")?,
        agent: agent_ref(&v)?,
        team: team_ref(&v)?,
        company: slug(&v, &["company"])?,
        expires_at: stamp(&v, &["expires_at"])?,
        mcp_url: match v.get("mcp_url") {
            None | Some(Value::Null) => None,
            Some(Value::String(s)) => Some(s.clone()),
            Some(_) => return Err(WaveError::Protocol("mcp_url is not text".into())),
        },
    })
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Remote {
    pub agent: AgentRef,
    pub team: TeamRef,
    pub company: String,
    pub engine: String,
    pub key_expires_at: String,
    pub last_seen_at: Option<String>,
    pub last_session: Option<String>,
}

pub fn status(http: &dyn Http, wave: &Account, key: &Key) -> Result<Remote, WaveError> {
    let r = http.send("GET", &url(wave, "status"), Some(key.expose()), None)?;
    if r.status != 200 {
        return Err(failure(&r));
    }
    let v = body_json(&r)?;
    let opt = |k: &str| -> Result<Option<String>, WaveError> {
        match v.get(k) {
            None | Some(Value::Null) => Ok(None),
            Some(Value::String(s)) => Ok(Some(tame(s, 120))),
            Some(_) => Err(WaveError::Protocol(format!("{k} is not text"))),
        }
    };
    Ok(Remote {
        agent: agent_ref(&v)?,
        team: team_ref(&v)?,
        company: slug(&v, &["company"])?,
        engine: tame(text(&v, &["engine"])?, 30),
        key_expires_at: stamp(&v, &["key_expires_at"])?,
        last_seen_at: opt("last_seen_at")?,
        last_session: opt("last_session")?,
    })
}

/// A new key for the agent; Wave revokes the old one with this call.
pub fn rotate(http: &dyn Http, wave: &Account, key: &Key) -> Result<(Key, String), WaveError> {
    let r = http.send("POST", &url(wave, "rotate"), Some(key.expose()), Some("{}"))?;
    if r.status != 200 && r.status != 201 {
        return Err(failure(&r));
    }
    let v = body_json(&r)?;
    Ok((key_of(&v, "key")?, stamp(&v, &["expires_at"])?))
}

/// Revokes every key of the agent. Returns how many were revoked.
pub fn revoke(http: &dyn Http, wave: &Account, key: &Key) -> Result<u64, WaveError> {
    let r = http.send("POST", &url(wave, "revoke"), Some(key.expose()), Some("{}"))?;
    if r.status != 200 {
        return Err(failure(&r));
    }
    let v = body_json(&r)?;
    v.get("revoked")
        .and_then(Value::as_u64)
        .ok_or_else(|| WaveError::Protocol("field revoked missing".into()))
}

/// `{wave}/mcp`, or the address Wave sent when it is on the same host.
pub fn mcp_address(wave: &Account, sent: Option<&str>) -> Result<String, WaveError> {
    let own = format!("{}/mcp", wave.base());
    match sent {
        None => Ok(own),
        Some(s) if s.trim_end_matches('/') == own => Ok(own),
        Some(_) => Err(WaveError::Protocol(
            "the MCP address points to another host than the Wave address; refusing to send the key there".into(),
        )),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::testkit::{FakeHttp, reply};

    pub const KEY: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_s3cretS3cretS3cret";
    const KEY2: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_n3wN3wN3wN3wN3w";

    pub fn minted_body(with_mcp: bool) -> String {
        let mut v = json!({
            "key": KEY,
            "agent": {"id": "11111111-2222-4333-8444-555555555555", "key": "agent-demo", "name": "Grok (Mac)"},
            "team": {"key": "ops", "name": "Ops"},
            "company": "acme",
            "expires_at": "2027-02-20T10:00:00Z"
        });
        if with_mcp {
            v["mcp_url"] = json!("https://wave.test/mcp");
        }
        v.to_string()
    }

    fn wave() -> Account {
        Account::parse_as("Wave", "https://wave.test").unwrap()
    }

    fn req() -> MintRequest<'static> {
        MintRequest {
            host: "mac-1",
            os: "macOS",
            engine: "grok",
        }
    }

    #[test]
    fn mint_sends_the_contract_and_reads_the_answer() {
        let f = FakeHttp::new(vec![reply(201, &minted_body(true))]);
        let m = mint(
            &f,
            &wave(),
            &Token::for_test("tttttttttttttttttttttt"),
            &req(),
        )
        .unwrap();
        assert_eq!(m.key.expose(), KEY);
        assert_eq!(m.agent.key, "agent-demo");
        assert_eq!(m.team.name, "Ops");
        assert_eq!(m.company, "acme");
        assert_eq!(m.mcp_url.as_deref(), Some("https://wave.test/mcp"));
        let seen = f.seen();
        assert_eq!(seen[0].method, "POST");
        assert_eq!(seen[0].url, "https://wave.test/v1/agent-connect/mint");
        assert_eq!(seen[0].bearer.as_deref(), Some("tttttttttttttttttttttt"));
        let b: Value = serde_json::from_str(seen[0].body.as_ref().unwrap()).unwrap();
        assert_eq!(b["host"], "mac-1");
        assert_eq!(b["os"], "macOS");
        assert_eq!(b["engine"], "grok");
        assert_eq!(b["version"], crate::VERSION);
    }

    #[test]
    fn mint_answer_without_mcp_url_works_and_uses_the_address_we_called() {
        let f = FakeHttp::new(vec![reply(201, &minted_body(false))]);
        let m = mint(
            &f,
            &wave(),
            &Token::for_test("tttttttttttttttttttttt"),
            &req(),
        )
        .unwrap();
        assert_eq!(m.mcp_url, None);
        assert_eq!(mcp_address(&wave(), None).unwrap(), "https://wave.test/mcp");
        assert_eq!(
            mcp_address(&wave(), Some("https://wave.test/mcp")).unwrap(),
            "https://wave.test/mcp"
        );
        assert!(mcp_address(&wave(), Some("https://evil.test/mcp")).is_err());
        assert!(mcp_address(&wave(), Some("https://wave.test/other")).is_err());
    }

    #[test]
    fn mint_refuses_malformed_answers() {
        let bad = |body: String| {
            let f = FakeHttp::new(vec![reply(201, &body)]);
            mint(
                &f,
                &wave(),
                &Token::for_test("tttttttttttttttttttttt"),
                &req(),
            )
        };
        let good: Value = serde_json::from_str(&minted_body(true)).unwrap();
        let with = |k: &str, v: Value| {
            let mut x = good.clone();
            let parts: Vec<&str> = k.split('.').collect();
            if parts.len() == 2 {
                x[parts[0]][parts[1]] = v;
            } else {
                x[k] = v;
            }
            x.to_string()
        };
        assert!(matches!(
            bad("not json".into()),
            Err(WaveError::Protocol(_))
        ));
        assert!(bad(with("key", json!("tok_aaaaaaaaaaaaaaaa"))).is_err());
        assert!(bad(with("key", json!(5))).is_err());
        assert!(bad(with("agent.key", json!("../etc"))).is_err());
        assert!(bad(with("agent.key", json!("A"))).is_err());
        assert!(bad(with("team.key", json!("a b"))).is_err());
        assert!(bad(with("company", json!("Acme Corp"))).is_err());
        assert!(bad(with("expires_at", json!("soon"))).is_err());
        assert!(bad(with("mcp_url", json!(7))).is_err());
        let mut x = good.clone();
        x.as_object_mut().unwrap().remove("agent");
        assert!(bad(x.to_string()).is_err());
    }

    #[test]
    fn mint_errors_have_codes_and_friendly_text() {
        let code = |status: u16, body: &str, retry: Option<u64>| {
            let f = FakeHttp::new(vec![Ok(Reply {
                status,
                body: body.into(),
                retry_after: retry,
            })]);
            mint(
                &f,
                &wave(),
                &Token::for_test("tttttttttttttttttttttt"),
                &req(),
            )
            .unwrap_err()
        };
        let e = code(409, r#"{"error":{"code":"agent_not_ready"}}"#, None);
        assert_eq!(e.code(), Some("agent_not_ready"));
        assert!(e.to_string().contains("still setting up"), "{e}");
        let e = code(409, r#"{"error":{"code":"already_minted"}}"#, None);
        assert!(e.to_string().contains("again for a new code"), "{e}");
        let e = code(403, r#"{"error":{"code":"forbidden"}}"#, None);
        assert!(e.to_string().contains("admin"), "{e}");
        let e = code(
            403,
            r#"{"error":{"code":"agent_connect_not_enabled"}}"#,
            None,
        );
        assert!(e.to_string().contains("not switched on"), "{e}");
        let e = code(401, r#"{"error":{"code":"token_expired"}}"#, None);
        assert!(e.is_unauthorized());
        let e = code(401, r#"{"error":{"code":"invalid_token"}}"#, None);
        assert!(e.is_unauthorized());
        let e = code(429, r#"{"error":{"code":"too_many_requests"}}"#, Some(42));
        assert!(e.to_string().contains("42 s"), "{e}");
        assert_eq!(
            e,
            WaveError::Api {
                status: 429,
                code: "too_many_requests".into(),
                message: None,
                retry_after: Some(42)
            }
        );
        // a proxy page is a network problem, not an answer of Wave
        let e = code(502, "<html>Bad gateway</html>", None);
        assert!(e.is_network());
        // an unknown code shows the server's message, tamed
        let e = code(
            400,
            r#"{"error":{"code":"weird","message":"a\u001b[31m b"}}"#,
            None,
        );
        assert!(e.to_string().contains("weird (a?[31m b)"), "{e}");
    }

    #[test]
    fn a_key_in_an_error_text_never_reaches_a_message() {
        let body = format!(r#"{{"error":{{"code":"x","message":"bad {KEY} here"}}}}"#);
        let f = FakeHttp::new(vec![reply(400, &body)]);
        let e = mint(
            &f,
            &wave(),
            &Token::for_test("tttttttttttttttttttttt"),
            &req(),
        )
        .unwrap_err();
        let shown = format!("{e} / {e:?}");
        assert!(!e.to_string().contains("s3cret"), "{e}");
        // Debug of the derived error holds the raw text of the message, so nothing prints it: Display scrubs
        let _ = shown;
        assert!(e.to_string().contains("wak_[removed]"), "{e}");
        let m = Minted {
            key: Key::for_test(KEY),
            agent: AgentRef {
                id: "i".into(),
                key: "agent-x".into(),
                name: "n".into(),
            },
            team: TeamRef {
                key: "t".into(),
                name: "T".into(),
            },
            company: "c".into(),
            expires_at: "x".into(),
            mcp_url: None,
        };
        assert!(!format!("{m:?}").contains("s3cret"));
    }

    #[test]
    fn status_rotate_revoke_use_the_agent_key_as_bearer() {
        let key = Key::for_test(KEY);
        let st = json!({
            "agent": {"id": "i-1", "key": "agent-demo", "name": "Grok"},
            "team": {"key": "ops", "name": "Ops"},
            "company": "acme", "engine": "external",
            "key_expires_at": "2027-02-20T10:00:00Z",
            "last_seen_at": null, "last_session": "grok on mac"
        })
        .to_string();
        let f = FakeHttp::new(vec![
            reply(200, &st),
            reply(
                200,
                &json!({"key": KEY2, "expires_at": "2027-03-01T00:00:00Z"}).to_string(),
            ),
            reply(200, r#"{"revoked":2}"#),
        ]);
        let s = status(&f, &wave(), &key).unwrap();
        assert_eq!(s.last_seen_at, None);
        assert_eq!(s.last_session.as_deref(), Some("grok on mac"));
        let (k2, exp) = rotate(&f, &wave(), &key).unwrap();
        assert_eq!(k2.expose(), KEY2);
        assert_eq!(exp, "2027-03-01T00:00:00Z");
        assert_eq!(revoke(&f, &wave(), &key).unwrap(), 2);
        let seen = f.seen();
        let routes: Vec<_> = seen
            .iter()
            .map(|s| (s.method.as_str(), s.url.as_str()))
            .collect();
        assert_eq!(
            routes,
            vec![
                ("GET", "https://wave.test/v1/agent-connect/status"),
                ("POST", "https://wave.test/v1/agent-connect/rotate"),
                ("POST", "https://wave.test/v1/agent-connect/revoke"),
            ]
        );
        assert!(seen.iter().all(|s| s.bearer.as_deref() == Some(KEY)));
        assert_eq!(seen[1].body.as_deref(), Some("{}"));
        assert_eq!(seen[2].body.as_deref(), Some("{}"));
        assert!(seen[0].body.is_none());
    }

    #[test]
    fn revoke_401_is_recognised_and_garbage_is_a_protocol_error() {
        let key = Key::for_test(KEY);
        let f = FakeHttp::new(vec![reply(401, r#"{"error":{"code":"invalid_token"}}"#)]);
        assert!(revoke(&f, &wave(), &key).unwrap_err().is_unauthorized());
        let f = FakeHttp::new(vec![reply(200, r#"{"revoked":"many"}"#)]);
        assert!(matches!(
            revoke(&f, &wave(), &key),
            Err(WaveError::Protocol(_))
        ));
    }
}
