//! The real `curl` against a local stub that speaks the account's device code protocol and Wave's agent-connect contract:
//! connect, status, rotate, status, disconnect. Files go into a temporary folder through injected base folders (the process
//! environment is not touched, the real home folder is not read or written), engines and service managers are recorded fakes.

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use serde_json::{Value, json};

use super::connect_flow::{ConnectOpts, connect};
use super::dirs::{Dirs, Os};
use super::engine::Engine;
use super::exec::fake::{Fake, ok_out};
use super::key::testdir::TempDir;
use super::manage::{self, DisconnectOpts};
use super::state::State;
use super::testkit::TestUi;
use super::ui::Kind;
use super::{Ctx, Timing};
use crate::connect::{Account, Curl, Http};
use crate::flow::Outcome;

const KEY1: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_firstFirstFirst1";
const KEY2: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_secondSecond22";
const TOKEN: &str = "tttttttttttttttttttt.uuuuuuuuuuuuuuuu.vvvvvvvvvvvv";

#[derive(Debug, Clone)]
struct Req {
    method: String,
    path: String,
    auth: String,
    body: String,
}

/// Account and Wave in one server. Keeps the current key like Wave would.
struct Stub {
    addr: String,
    log: Arc<Mutex<Vec<Req>>>,
    stop: Arc<AtomicBool>,
    handle: Option<std::thread::JoinHandle<()>>,
}

fn respond(s: &mut TcpStream, status: u16, extra_headers: &str, body: &str) {
    write!(
        s,
        "HTTP/1.1 {status} X\r\ncontent-type: application/json\r\ncontent-length: {}\r\nconnection: close\r\n{extra_headers}\r\n{body}",
        body.len()
    )
    .unwrap();
}

impl Stub {
    fn start() -> Stub {
        let l = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = format!("http://{}", l.local_addr().unwrap());
        let log = Arc::new(Mutex::new(Vec::<Req>::new()));
        let stop = Arc::new(AtomicBool::new(false));
        let (log2, stop2, addr2) = (log.clone(), stop.clone(), addr.clone());
        let handle = std::thread::spawn(move || {
            let current_key = Mutex::new(None::<String>);
            let token_polls = AtomicUsize::new(0);
            let mint_calls = AtomicUsize::new(0);
            for conn in l.incoming() {
                if stop2.load(Ordering::SeqCst) {
                    break;
                }
                let Ok(mut s) = conn else { continue };
                let mut rd = BufReader::new(s.try_clone().unwrap());
                let mut line = String::new();
                if rd.read_line(&mut line).is_err() {
                    continue;
                }
                let (mut auth, mut len) = (String::new(), 0usize);
                loop {
                    let mut h = String::new();
                    if rd.read_line(&mut h).is_err() || h.trim().is_empty() {
                        break;
                    }
                    let low = h.to_ascii_lowercase();
                    if let Some(v) = low.strip_prefix("content-length:") {
                        len = v.trim().parse().unwrap();
                    }
                    if low.starts_with("authorization:") {
                        auth = h.split_once(':').unwrap().1.trim().to_owned();
                    }
                }
                let mut b = vec![0; len];
                rd.read_exact(&mut b).unwrap();
                let mut parts = line.split_whitespace();
                let (method, path) = (parts.next().unwrap_or(""), parts.next().unwrap_or(""));
                let req = Req {
                    method: method.to_owned(),
                    path: path.to_owned(),
                    auth: auth.clone(),
                    body: String::from_utf8(b).unwrap(),
                };
                log2.lock().unwrap().push(req);
                let bearer = auth.strip_prefix("Bearer ").unwrap_or("");
                let err = |code: &str| json!({"error": {"code": code}}).to_string();
                match (method, path) {
                    ("POST", "/v1/device/code") => respond(
                        &mut s,
                        200,
                        "",
                        r#"{"device_code":"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG","user_code":"WAVE-7K4Q-X2MP","expires_in":600,"interval":1,"words":["amber","river","cedar","fox"]}"#,
                    ),
                    ("POST", "/v1/device/token") => {
                        if token_polls.fetch_add(1, Ordering::SeqCst) == 0 {
                            respond(&mut s, 400, "", r#"{"error":"authorization_pending"}"#);
                        } else {
                            let v = json!({
                                "access_token": TOKEN, "token_type": "Bearer", "expires_in": 900,
                                "scope": "agent:connect", "company": "acme", "company_name": "Acme GmbH",
                                "team": "ops", "team_name": "Ops",
                                "agent": "agent-demo", "agent_name": "Grok (Mac)", "engine": "grok",
                                "wave_url": addr2
                            });
                            respond(&mut s, 200, "", &v.to_string());
                        }
                    }
                    ("POST", "/v1/agent-connect/mint") => {
                        if bearer != TOKEN {
                            respond(&mut s, 401, "", &err("invalid_token"));
                            continue;
                        }
                        match mint_calls.fetch_add(1, Ordering::SeqCst) {
                            0 => respond(
                                &mut s,
                                429,
                                "retry-after: 2\r\n",
                                &err("too_many_requests"),
                            ),
                            1 => respond(&mut s, 409, "", &err("agent_not_ready")),
                            _ => {
                                *current_key.lock().unwrap() = Some(KEY1.to_owned());
                                // no mcp_url: the CLI uses {wave}/mcp
                                let v = json!({
                                    "key": KEY1,
                                    "agent": {"id": "11111111-2222-4333-8444-555555555555", "key": "agent-demo", "name": "Grok (Mac)"},
                                    "team": {"key": "ops", "name": "Ops"},
                                    "company": "acme", "expires_at": "2099-02-20T10:00:00Z"
                                });
                                respond(&mut s, 201, "", &v.to_string());
                            }
                        }
                    }
                    ("GET" | "POST", p) if p.starts_with("/v1/agent-connect/") => {
                        let mut cur = current_key.lock().unwrap();
                        if cur.as_deref() != Some(bearer) {
                            respond(&mut s, 401, "", &err("invalid_token"));
                            continue;
                        }
                        match p {
                            "/v1/agent-connect/status" => {
                                let v = json!({
                                    "agent": {"id": "11111111-2222-4333-8444-555555555555", "key": "agent-demo", "name": "Grok (Mac)"},
                                    "team": {"key": "ops", "name": "Ops"}, "company": "acme", "engine": "grok",
                                    "key_expires_at": "2099-02-20T10:00:00Z", "last_seen_at": null, "last_session": null
                                });
                                respond(&mut s, 200, "", &v.to_string());
                            }
                            "/v1/agent-connect/rotate" => {
                                *cur = Some(KEY2.to_owned());
                                respond(
                                    &mut s,
                                    200,
                                    "",
                                    &json!({"key": KEY2, "expires_at": "2099-02-20T10:00:00Z"})
                                        .to_string(),
                                );
                            }
                            "/v1/agent-connect/revoke" => {
                                *cur = None;
                                respond(&mut s, 200, "", r#"{"revoked":1}"#);
                            }
                            _ => respond(&mut s, 404, "", &err("not_found")),
                        }
                    }
                    _ => respond(&mut s, 404, "", &err("not_found")),
                }
            }
        });
        Stub {
            addr,
            log,
            stop,
            handle: Some(handle),
        }
    }

    fn log(&self) -> Vec<Req> {
        self.log.lock().unwrap().clone()
    }
}

impl Drop for Stub {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = TcpStream::connect(self.addr.trim_start_matches("http://"));
        if let Some(h) = self.handle.take() {
            let _ = h.join();
        }
    }
}

fn have_curl() -> bool {
    std::process::Command::new("curl")
        .arg("--version")
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .map(|s| s.success())
        .unwrap_or(false)
}

fn real(a: &Account) -> Box<dyn Http> {
    Box::new(Curl {
        allow_plain_http: !a.is_https(),
    })
}

#[test]
fn connect_status_rotate_disconnect_with_the_real_curl_against_a_stub() {
    if !have_curl() {
        eprintln!("curl not installed: skipped");
        return;
    }
    let stub = Stub::start();
    let t = TempDir::new("e2e");
    let mut dirs = Dirs::under(t.path());
    dirs.os = Os::Linux;
    let exec =
        Fake::new(&["grok", "node"]).with(|_, a| (a == ["--version"]).then(|| ok_out("v22.0.0\n")));
    let make: &dyn Fn(&Account) -> Box<dyn Http> = &real;
    let ctx = Ctx {
        dirs: &dirs,
        exec: &exec,
        http_for: make,
        host: "mac-1",
        os: "macOS",
        timing: Timing::default(),
    };
    let account = Account::parse(&stub.addr).unwrap();

    // ---- connect: 429 with Retry-After (read from the real header), then agent_not_ready, then the key
    let mut ui = TestUi::new(&[true]);
    let out = connect(
        &ctx,
        &mut ui,
        &ConnectOpts {
            engine: Some(Engine::Grok),
            company: Some("acme".into()),
            name: Some("Grok (Mac)".into()),
            no_station: false,
            yes: false,
            account,
            wave: None,
        },
    )
    .unwrap();
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    let pauses: Vec<(u64, String)> = ui
        .pauses
        .iter()
        .map(|(d, m)| (d.as_secs(), m.clone()))
        .collect();
    assert_eq!(pauses.len(), 2, "{pauses:?}");
    assert_eq!(pauses[0].0, 2, "Retry-After: 2 was honoured");
    assert_eq!(pauses[1].0, 5);
    assert!(pauses[1].1.contains("setting up the agent"));

    let key_file = dirs.key_file("agent-demo");
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), KEY1);
    let all = ui.text();
    assert!(
        !all.contains("firstFirst") && !all.contains("wak_0a1b"),
        "{all}"
    );
    let st = State::load(&dirs.state_file("agent-demo")).unwrap();
    assert_eq!(
        st.mcp_url,
        format!("{}/mcp", stub.addr),
        "no mcp_url in the answer: {{wave}}/mcp"
    );
    assert_eq!(st.wave_url, stub.addr);

    // ---- what the stub saw
    let seen = stub.log();
    let start: Value = serde_json::from_str(&seen[0].body).unwrap();
    assert_eq!(start["purpose"], "agent");
    assert_eq!(
        start["want"],
        json!({"engine": "grok", "agent_name": "Grok (Mac)", "company": "acme"})
    );
    let mints: Vec<&Req> = seen
        .iter()
        .filter(|r| r.path == "/v1/agent-connect/mint")
        .collect();
    assert_eq!(mints.len(), 3);
    assert!(mints.iter().all(|m| m.auth == format!("Bearer {TOKEN}")));
    let grok_add = exec
        .lines()
        .into_iter()
        .find(|l| l.contains("mcp add"))
        .unwrap();
    assert!(grok_add.contains("Bearer ${WAVE_AGENT_KEY}") && !grok_add.contains("wak_"));

    // ---- status through the real transport
    let before = stub.log().len();
    let text = manage::status_json(&ctx, None, None);
    assert!(!text.contains("wak_"));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["remote"]["reachable"], true, "{text}");
    assert_eq!(stub.log()[before].auth, format!("Bearer {KEY1}"));
    assert!(
        stub.log()
            .iter()
            .any(|r| r.method == "GET" && r.path.ends_with("/status"))
    );

    // ---- rotate
    let mut ui = TestUi::new(&[]);
    let out = manage::rotate(&ctx, &mut ui, None, None).unwrap();
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    assert_eq!(std::fs::read_to_string(&key_file).unwrap(), KEY2);
    // the expiry is the one of the binding, and rotate says so
    assert!(ui.has(Kind::Ok, "same expiry 2099-02-20"), "{}", ui.text());
    assert!(ui.has(Kind::Note, "does not extend"));
    assert_eq!(
        State::load(&dirs.state_file("agent-demo"))
            .unwrap()
            .key_expires_at,
        "2099-02-20T10:00:00Z"
    );
    let rot = stub
        .log()
        .into_iter()
        .find(|r| r.path.ends_with("/rotate"))
        .unwrap();
    assert_eq!(rot.auth, format!("Bearer {KEY1}"));
    assert_eq!(rot.body, "{}");
    let v: Value = serde_json::from_str(&manage::status_json(&ctx, None, None)).unwrap();
    assert_eq!(v["remote"]["reachable"], true, "the new key works");

    // ---- disconnect
    let mut ui = TestUi::new(&[]);
    let out = manage::disconnect(
        &ctx,
        &mut ui,
        &DisconnectOpts {
            agent: None,
            wave: None,
            yes: true,
            keep_remote: false,
        },
    )
    .unwrap();
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    let rev = stub
        .log()
        .into_iter()
        .find(|r| r.path.ends_with("/revoke"))
        .unwrap();
    assert_eq!(rev.auth, format!("Bearer {KEY2}"));
    assert!(!key_file.exists());
    assert!(!dirs.state_file("agent-demo").exists());
    assert!(
        exec.lines()
            .iter()
            .any(|l| l == "/fake/bin/grok mcp remove wave")
    );
    // the old key is dead at the stub, too: status of a fresh file with it answers 401
    std::fs::create_dir_all(&dirs.config).unwrap();
    let all = ui.text();
    assert!(!all.contains("secondSecond"), "{all}");
    drop(stub);
}

#[test]
fn nothing_listens_is_a_network_error_not_a_hang() {
    if !have_curl() {
        return;
    }
    let t = TempDir::new("e2e-down");
    let dirs = Dirs::under(t.path());
    let exec = Fake::new(&["grok"]);
    let make: &dyn Fn(&Account) -> Box<dyn Http> = &real;
    let ctx = Ctx {
        dirs: &dirs,
        exec: &exec,
        http_for: make,
        host: "h",
        os: "o",
        timing: Timing::default(),
    };
    let mut ui = TestUi::new(&[]);
    let out = connect(
        &ctx,
        &mut ui,
        &ConnectOpts {
            engine: Some(Engine::Grok),
            company: None,
            name: None,
            no_station: true,
            yes: false,
            account: Account::parse("http://127.0.0.1:1").unwrap(),
            wave: None,
        },
    )
    .unwrap();
    assert!(matches!(out, Outcome::Failed));
    assert!(
        ui.text().contains("Could not start the sign-in"),
        "{}",
        ui.text()
    );
    assert!(!dirs.config.exists());
}
