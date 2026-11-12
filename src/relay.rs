//! The server's identity and the wire format of the relay (see `docs/relay-protocol.md`).
//!
//! The server has its own Ed25519 key. The private half is made on the server and never leaves it; Wave
//! only learns the public half when the server enrols with a one-time sign-in token. Every later request
//! to the relay is signed with that key, so the relay can tell the server from anybody else, and a request
//! cannot be changed or played again.

use std::path::{Path, PathBuf};

use ed25519_dalek::{Signer, SigningKey};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

use crate::b64;
use crate::connect::{Account, ConnectError, Http, Token, tame};

pub const SEED_FILE: &str = "server.key";
pub const IDENTITY_FILE: &str = "server.json";

/// Most bytes in one data frame (before base64). The relay refuses more.
pub const MAX_DATA: usize = 16 * 1024;

pub fn sha256_hex(data: &[u8]) -> String {
    Sha256::digest(data)
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
}

pub fn random_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    getrandom::getrandom(&mut b).expect("the system has a random source");
    b
}

// ------------------------------------------------------------------ the key

/// The server's signing key. `Debug` never shows it.
pub struct ServerKey(SigningKey);

impl std::fmt::Debug for ServerKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("ServerKey(..)")
    }
}

impl ServerKey {
    pub fn generate() -> ServerKey {
        ServerKey(SigningKey::from_bytes(&random_bytes::<32>()))
    }

    pub fn from_seed(seed: [u8; 32]) -> ServerKey {
        ServerKey(SigningKey::from_bytes(&seed))
    }

    /// The text kept in `server.key`: the 32 byte seed, base64url.
    pub fn seed_text(&self) -> String {
        b64::encode_url(self.0.as_bytes())
    }

    pub fn from_seed_text(text: &str) -> Option<ServerKey> {
        let seed: [u8; 32] = b64::decode_url(text.trim())?.try_into().ok()?;
        Some(ServerKey::from_seed(seed))
    }

    pub fn public_b64u(&self) -> String {
        b64::encode_url(self.0.verifying_key().as_bytes())
    }

    pub fn sign(&self, text: &str) -> String {
        b64::encode_url(&self.0.sign(text.as_bytes()).to_bytes())
    }
}

/// What the server signs at enrolment: it proves the holder of the key asked, for exactly this sign-in token.
pub fn enrol_string(token: &str, public_key_b64u: &str) -> String {
    format!(
        "sw-enrol1\n{}\n{public_key_b64u}",
        sha256_hex(token.as_bytes())
    )
}

/// What the server signs for one request. Everything that could be changed on the way is in it.
pub fn signing_string(
    method: &str,
    target: &str,
    time: &str,
    nonce: &str,
    body: &[u8],
    server_ref: &str,
    session: Option<&str>,
) -> String {
    [
        "SW1",
        &method.to_ascii_uppercase(),
        target,
        time,
        nonce,
        &sha256_hex(body),
        server_ref,
        session.unwrap_or("-"),
    ]
    .join("\n")
}

/// The headers of one signed request.
pub fn signed_headers(
    key: &ServerKey,
    server_ref: &str,
    method: &str,
    target: &str,
    body: &[u8],
    session: Option<&str>,
    now_ms: u64,
) -> Vec<(String, String)> {
    let time = now_ms.to_string();
    let nonce = b64::encode_url(&random_bytes::<16>());
    let sig = key.sign(&signing_string(
        method, target, &time, &nonce, body, server_ref, session,
    ));
    let mut h = vec![
        (
            "Authorization".to_owned(),
            format!("Stonewave-Server {server_ref}"),
        ),
        ("X-SW-Time".to_owned(), time),
        ("X-SW-Nonce".to_owned(), nonce),
        ("X-SW-Sig".to_owned(), sig),
    ];
    if let Some(s) = session {
        h.push(("X-SW-Session".to_owned(), s.to_owned()));
    }
    h
}

pub fn now_ms() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_millis() as u64)
}

// ------------------------------------------------------------------ identity on disk

/// What the daemon needs to find its way back: where the relay is and who it is there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Identity {
    /// `<company id>.<server id>`; not a secret.
    pub server_ref: String,
    pub relay: String,
    pub company: String,
    pub name: String,
}

pub struct Paths {
    pub dir: PathBuf,
}

impl Paths {
    pub fn system() -> Paths {
        Paths {
            dir: PathBuf::from(crate::setup::VAR_LIB),
        }
    }
    pub fn seed(&self) -> PathBuf {
        self.dir.join(SEED_FILE)
    }
    pub fn identity(&self) -> PathBuf {
        self.dir.join(IDENTITY_FILE)
    }
}

impl Identity {
    pub fn to_json(&self) -> String {
        json!({
            "server_ref": self.server_ref, "relay": self.relay,
            "company": self.company, "name": self.name,
        })
        .to_string()
            + "\n"
    }

    pub fn parse(text: &str) -> Option<Identity> {
        let v: Value = serde_json::from_str(text).ok()?;
        let s = |k: &str| v.get(k)?.as_str().map(str::to_owned);
        let id = Identity {
            server_ref: s("server_ref")?,
            relay: s("relay")?,
            company: s("company")?,
            name: s("name")?,
        };
        // the same shape the relay checks: two UUIDs
        let parts: Vec<&str> = id.server_ref.split('.').collect();
        let uuid = |u: &str| {
            u.len() == 36
                && u.chars().enumerate().all(|(i, c)| {
                    if [8, 13, 18, 23].contains(&i) {
                        c == '-'
                    } else {
                        c.is_ascii_hexdigit()
                    }
                })
        };
        if parts.len() != 2 || !parts.iter().all(|p| uuid(p)) {
            return None;
        }
        Some(id)
    }
}

/// Writes a file readable by its owner only (`0600`); never visible with looser rights.
pub fn write_private(path: &Path, text: &str) -> std::io::Result<()> {
    use std::io::Write;
    let tmp = path.with_extension("tmp");
    let _ = std::fs::remove_file(&tmp);
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt;
        opts.mode(0o600);
    }
    let mut f = opts.open(&tmp)?;
    f.write_all(text.as_bytes())?;
    f.sync_all()?;
    drop(f);
    std::fs::rename(&tmp, path)
}

pub fn save(paths: &Paths, key: &ServerKey, id: &Identity) -> std::io::Result<()> {
    std::fs::create_dir_all(&paths.dir)?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        // Only tighten a folder we just made or own; never loosen one that exists.
        let m = std::fs::metadata(&paths.dir)?.permissions().mode() & 0o777;
        if m & 0o027 != 0 {
            std::fs::set_permissions(&paths.dir, std::fs::Permissions::from_mode(0o750))?;
        }
    }
    write_private(&paths.seed(), &(key.seed_text() + "\n"))?;
    write_private(&paths.identity(), &id.to_json())
}

pub fn load(paths: &Paths) -> Option<(ServerKey, Identity)> {
    let key = ServerKey::from_seed_text(&std::fs::read_to_string(paths.seed()).ok()?)?;
    let id = Identity::parse(&std::fs::read_to_string(paths.identity()).ok()?)?;
    Some((key, id))
}

// ------------------------------------------------------------------ enrolment

#[derive(Debug, PartialEq, Eq)]
pub struct Enrolled {
    pub identity: Identity,
    pub status: String,
}

/// Registers this server's key with Wave, using the token of the sign-in that was just approved.
pub fn enrol(
    http: &dyn Http,
    relay: &Account,
    token: &Token,
    key: &ServerKey,
    host: &str,
    os: &str,
) -> Result<Enrolled, ConnectError> {
    let public = key.public_b64u();
    let body = json!({
        "public_key": public,
        "proof": key.sign(&enrol_string(token.secret(), &public)),
        "host": host, "os": os, "version": crate::VERSION,
    })
    .to_string();
    let r = http.send(
        "POST",
        &relay.url("/v1/relay/enrol"),
        Some(token.secret()),
        Some(&body),
    )?;
    let v: Value = serde_json::from_str(&r.body)
        .map_err(|_| ConnectError::Protocol(format!("HTTP {} without JSON", r.status)))?;
    if r.status != 200 && r.status != 201 {
        let code = v
            .pointer("/error/code")
            .and_then(Value::as_str)
            .unwrap_or("unknown_error");
        return Err(ConnectError::Server(tame(code, 40)));
    }
    let s = |k: &str| {
        v.get(k)
            .and_then(Value::as_str)
            .map(|x| tame(x, 120))
            .ok_or_else(|| ConnectError::Protocol(format!("field {k} missing")))
    };
    let identity = Identity {
        server_ref: s("server_ref")?,
        relay: relay.base().to_owned(),
        company: s("company")?,
        name: s("name")?,
    };
    // Nothing is saved from an answer that does not have the shape the relay promised.
    if Identity::parse(&identity.to_json()).is_none() {
        return Err(ConnectError::Protocol(
            "server_ref has an unexpected form".into(),
        ));
    }
    Ok(Enrolled {
        identity,
        status: s("status")?,
    })
}

// ------------------------------------------------------------------ frames

/// What Wave sends down the stream.
#[derive(Debug, PartialEq, Eq)]
pub enum Down {
    Hello {
        session: String,
        heartbeat_s: u64,
    },
    Ping,
    Open {
        ch: u32,
    },
    Data {
        ch: u32,
        data: Vec<u8>,
    },
    Close {
        ch: u32,
    },
    Bye {
        reason: String,
    },
    /// A frame type this build does not know: ignored, so Wave can grow without breaking old servers.
    Unknown,
}

pub fn parse_down(line: &str) -> Option<Down> {
    let v: Value = serde_json::from_str(line).ok()?;
    let ch = || {
        v.get("ch")
            .and_then(Value::as_u64)
            .filter(|c| (1..=1_000_000).contains(c))
            .map(|c| c as u32)
    };
    Some(match v.get("t")?.as_str()? {
        "hello" => {
            let session = v.get("session")?.as_str()?.to_owned();
            if !(16..=32).contains(&session.len())
                || !session
                    .chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
            {
                return None;
            }
            Down::Hello {
                session,
                heartbeat_s: v
                    .get("heartbeat_s")
                    .and_then(Value::as_u64)
                    .unwrap_or(20)
                    .clamp(5, 120),
            }
        }
        "ping" => Down::Ping,
        "open" => Down::Open { ch: ch()? },
        "close" => Down::Close { ch: ch()? },
        "data" => {
            let data = b64::decode_url(v.get("d")?.as_str()?)?;
            if data.len() > MAX_DATA {
                return None;
            }
            Down::Data { ch: ch()?, data }
        }
        "bye" => Down::Bye {
            reason: tame(v.get("reason").and_then(Value::as_str).unwrap_or(""), 40),
        },
        _ => Down::Unknown,
    })
}

/// What the server sends up.
#[derive(Debug, PartialEq, Eq)]
pub enum Up {
    Opened(u32),
    OpenFailed(u32),
    Data(u32, Vec<u8>),
    Close(u32),
    /// A ready `v` object of a health report (already JSON text).
    Health(String),
}

impl Up {
    pub fn line(&self) -> String {
        match self {
            Up::Opened(ch) => format!(r#"{{"t":"opened","ch":{ch}}}"#),
            Up::OpenFailed(ch) => format!(r#"{{"t":"open_failed","ch":{ch}}}"#),
            Up::Close(ch) => format!(r#"{{"t":"close","ch":{ch}}}"#),
            Up::Data(ch, d) => format!(r#"{{"t":"data","ch":{ch},"d":"{}"}}"#, b64::encode_url(d)),
            Up::Health(v) => format!(r#"{{"t":"health","v":{v}}}"#),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // A fixed test vector for the signing format; the relay checks the very same values.
    const VECTOR_PUB: &str = "6kpsY-KcUgq-9VB7Ey7F-ZVHdq6-vnuSQh7qaRRG0iw";
    const VECTOR_SIG: &str =
        "n1JH0qdPYNy9tybmEmWmRu5fc1-WOLKLskJXkEjGVEC5TBFRdWxqLkNeWGxOOKMRGX9Sl_NkZrMNiBWmTfujAg";
    const REF: &str = "11111111-1111-4111-8111-111111111111.22222222-2222-4222-8222-222222222222";

    #[test]
    fn signs_exactly_like_the_relay_expects() {
        let key = ServerKey::from_seed([7; 32]);
        assert_eq!(key.public_b64u(), VECTOR_PUB);
        let text = signing_string(
            "post",
            "/v1/relay/up",
            "1700000000000",
            "AAAAAAAAAAAAAAAAAAAAAA",
            b"{\"t\":\"close\",\"ch\":1}\n",
            REF,
            Some("SESSIONSESSIONSESSION"),
        );
        assert_eq!(
            text,
            format!(
                "SW1\nPOST\n/v1/relay/up\n1700000000000\nAAAAAAAAAAAAAAAAAAAAAA\n142f3bf3876fa1a68084fa72175381884ae7b3e107ee4ab475bb2ebe893ac4d2\n{REF}\nSESSIONSESSIONSESSION"
            )
        );
        assert_eq!(key.sign(&text), VECTOR_SIG);
    }

    #[test]
    fn the_stream_request_has_no_session_and_no_body() {
        let t = signing_string("GET", "/v1/relay/stream", "1", "n", b"", REF, None);
        assert!(t.ends_with("\n-"), "{t}");
        assert!(t.contains(&sha256_hex(b"")));
    }

    #[test]
    fn every_request_gets_a_fresh_nonce_and_the_right_headers() {
        let key = ServerKey::generate();
        let a = signed_headers(&key, REF, "GET", "/v1/relay/stream", b"", None, 5);
        let b = signed_headers(&key, REF, "GET", "/v1/relay/stream", b"", None, 5);
        let get =
            |h: &[(String, String)], k: &str| h.iter().find(|x| x.0 == k).map(|x| x.1.clone());
        assert_ne!(get(&a, "X-SW-Nonce"), get(&b, "X-SW-Nonce"));
        assert_eq!(
            get(&a, "Authorization").unwrap(),
            format!("Stonewave-Server {REF}")
        );
        assert!(get(&a, "X-SW-Session").is_none());
        let c = signed_headers(&key, REF, "POST", "/v1/relay/up", b"x", Some("S"), 5);
        assert_eq!(get(&c, "X-SW-Session").unwrap(), "S");
    }

    #[test]
    fn the_seed_round_trips_and_is_never_printed() {
        let key = ServerKey::generate();
        let again = ServerKey::from_seed_text(&key.seed_text()).unwrap();
        assert_eq!(key.public_b64u(), again.public_b64u());
        assert_eq!(format!("{key:?}"), "ServerKey(..)");
        assert!(ServerKey::from_seed_text("too short").is_none());
        assert!(ServerKey::from_seed_text(&b64::encode_url(&[1u8; 31])).is_none());
    }

    #[test]
    fn identity_is_checked_when_read() {
        let id = Identity {
            server_ref: REF.into(),
            relay: "https://r.example".into(),
            company: "acme".into(),
            name: "web-01".into(),
        };
        assert_eq!(Identity::parse(&id.to_json()), Some(id.clone()));
        for bad in [
            "{}",
            "not json",
            &id.to_json().replace('.', "x"),
            &id.to_json().replace("1111-4111", "zzzz-4111"),
        ] {
            assert!(Identity::parse(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn key_and_identity_are_saved_privately_and_loaded_again() {
        let dir =
            std::env::temp_dir().join(format!("sw-test-{}", b64::encode_url(&random_bytes::<6>())));
        let paths = Paths { dir: dir.clone() };
        let key = ServerKey::generate();
        let id = Identity {
            server_ref: REF.into(),
            relay: "https://r.example".into(),
            company: "acme".into(),
            name: "web-01".into(),
        };
        save(&paths, &key, &id).unwrap();
        let (k2, i2) = load(&paths).unwrap();
        assert_eq!(k2.public_b64u(), key.public_b64u());
        assert_eq!(i2, id);
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(paths.seed())
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600, "the private key is for its owner only");
        }
        // saving again replaces the files (a new connect)
        let key2 = ServerKey::generate();
        save(&paths, &key2, &id).unwrap();
        assert_eq!(load(&paths).unwrap().0.public_b64u(), key2.public_b64u());
        let _ = std::fs::remove_dir_all(dir);
    }

    #[test]
    fn frames_from_wave_are_parsed_strictly() {
        assert_eq!(
            parse_down(
                r#"{"t":"hello","session":"abcdefghijklmnopqrstuv","server_id":"x","now":1,"heartbeat_s":20}"#
            ),
            Some(Down::Hello {
                session: "abcdefghijklmnopqrstuv".into(),
                heartbeat_s: 20
            })
        );
        assert_eq!(
            parse_down(r#"{"t":"open","ch":3}"#),
            Some(Down::Open { ch: 3 })
        );
        assert_eq!(
            parse_down(r#"{"t":"close","ch":3}"#),
            Some(Down::Close { ch: 3 })
        );
        assert_eq!(
            parse_down(r#"{"t":"data","ch":3,"d":"aGk"}"#),
            Some(Down::Data {
                ch: 3,
                data: b"hi".to_vec()
            })
        );
        assert_eq!(
            parse_down(r#"{"t":"bye","reason":"revoked"}"#),
            Some(Down::Bye {
                reason: "revoked".into()
            })
        );
        assert_eq!(parse_down(r#"{"t":"something-new"}"#), Some(Down::Unknown));
        for bad in [
            r#"{"t":"open","ch":0}"#,
            r#"{"t":"open","ch":-1}"#,
            r#"{"t":"open"}"#,
            r#"{"t":"hello","session":"short"}"#,
            r#"{"t":"hello","session":"has spaces in it, not allowed"}"#,
            r#"{"t":"data","ch":1,"d":"!!"}"#,
            "not json",
            "[]",
        ] {
            assert_eq!(parse_down(bad), None, "{bad}");
        }
        let big = b64::encode_url(&vec![0u8; MAX_DATA + 1]);
        assert_eq!(
            parse_down(&format!(r#"{{"t":"data","ch":1,"d":"{big}"}}"#)),
            None
        );
    }

    #[test]
    fn frames_to_wave_have_the_shape_the_relay_checks() {
        assert_eq!(Up::Opened(2).line(), r#"{"t":"opened","ch":2}"#);
        assert_eq!(Up::OpenFailed(2).line(), r#"{"t":"open_failed","ch":2}"#);
        assert_eq!(Up::Close(2).line(), r#"{"t":"close","ch":2}"#);
        assert_eq!(
            Up::Data(2, b"SSH-2.0".to_vec()).line(),
            r#"{"t":"data","ch":2,"d":"U1NILTIuMA"}"#
        );
        assert_eq!(
            Up::Health(r#"{"ts":1}"#.into()).line(),
            r#"{"t":"health","v":{"ts":1}}"#
        );
    }

    use crate::connect::Reply;
    use std::sync::Mutex;

    type Seen = (String, String, Option<String>, Option<String>);

    struct FakeHttp {
        answer: (u16, String),
        seen: Mutex<Vec<Seen>>,
    }
    impl Http for FakeHttp {
        fn send(
            &self,
            method: &str,
            url: &str,
            bearer: Option<&str>,
            body: Option<&str>,
        ) -> Result<Reply, ConnectError> {
            self.seen.lock().unwrap().push((
                method.into(),
                url.into(),
                bearer.map(Into::into),
                body.map(Into::into),
            ));
            Ok(Reply {
                status: self.answer.0,
                body: self.answer.1.clone(),
            })
        }
    }

    fn answer(status: u16, body: &str) -> FakeHttp {
        FakeHttp {
            answer: (status, body.to_owned()),
            seen: Mutex::new(Vec::new()),
        }
    }

    #[test]
    fn enrolment_sends_the_key_with_a_proof_for_this_token_only() {
        let relay = Account::parse("https://relay.example").unwrap();
        let key = ServerKey::from_seed([7; 32]);
        let token = Token::for_test("the.one.time.token");
        let http = answer(
            201,
            &format!(
                r#"{{"server_ref":"{REF}","server_id":"x","name":"web-01","status":"new","company":"acme"}}"#
            ),
        );
        let got = enrol(&http, &relay, &token, &key, "web-01", "Ubuntu 24.04").unwrap();
        assert_eq!(got.identity.server_ref, REF);
        assert_eq!(got.identity.relay, "https://relay.example");
        assert_eq!(got.status, "new");
        let seen = http.seen.lock().unwrap();
        let (method, url, bearer, body) = &seen[0];
        assert_eq!(
            (method.as_str(), url.as_str()),
            ("POST", "https://relay.example/v1/relay/enrol")
        );
        assert_eq!(bearer.as_deref(), Some("the.one.time.token"));
        let v: Value = serde_json::from_str(body.as_ref().unwrap()).unwrap();
        assert_eq!(v["public_key"], VECTOR_PUB);
        assert_eq!(
            v["proof"],
            key.sign(&enrol_string("the.one.time.token", VECTOR_PUB))
        );
        assert!(
            !body.as_ref().unwrap().contains("the.one.time.token"),
            "the token travels in the header only"
        );
        assert!(
            !body.as_ref().unwrap().contains(&key.seed_text()),
            "the private key never leaves"
        );
    }

    #[test]
    fn enrolment_errors_are_readable_and_a_strange_answer_saves_nothing() {
        let relay = Account::parse("https://relay.example").unwrap();
        let key = ServerKey::generate();
        let token = Token::for_test("t");
        let refused = answer(403, r#"{"error":{"code":"relay_not_enabled"}}"#);
        assert_eq!(
            enrol(&refused, &relay, &token, &key, "h", "o").unwrap_err(),
            ConnectError::Server("relay_not_enabled".into())
        );
        let odd = answer(
            201,
            r#"{"server_ref":"not-a-ref","name":"x","status":"new","company":"acme"}"#,
        );
        assert!(matches!(
            enrol(&odd, &relay, &token, &key, "h", "o"),
            Err(ConnectError::Protocol(_))
        ));
        let html = answer(502, "<html>bad gateway</html>");
        assert!(matches!(
            enrol(&html, &relay, &token, &key, "h", "o"),
            Err(ConnectError::Protocol(_))
        ));
    }
}
