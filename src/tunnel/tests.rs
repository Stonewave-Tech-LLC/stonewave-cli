use std::io::{BufReader, Read, Write};
use std::net::TcpListener;
use std::sync::atomic::AtomicBool;
use std::sync::mpsc::{Receiver, Sender, channel};

use super::*;
use crate::b64;
use crate::relay::signing_string;

// ---------------------------------------------------------------- a made-up relay

/// A reader that yields what the test sends it, and ends when the sender is dropped.
struct PipeRead {
    rx: Receiver<Vec<u8>>,
    left: Vec<u8>,
}
impl Read for PipeRead {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        if self.left.is_empty() {
            match self.rx.recv() {
                Ok(v) => self.left = v,
                Err(_) => return Ok(0),
            }
        }
        let n = self.left.len().min(buf.len());
        buf[..n].copy_from_slice(&self.left[..n]);
        self.left.drain(..n);
        Ok(n)
    }
}

struct Fake {
    stream: Mutex<Option<Receiver<Vec<u8>>>>,
    posts: Mutex<Vec<String>>,
    post_error: Mutex<Option<TunnelError>>,
    open_error: Mutex<Option<TunnelError>>,
}

impl Transport for Fake {
    fn open_stream(&self) -> Result<Box<dyn BufRead + Send>, TunnelError> {
        if let Some(e) = self.open_error.lock().unwrap().take() {
            return Err(e);
        }
        let rx = self
            .stream
            .lock()
            .unwrap()
            .take()
            .expect("one stream per test");
        Ok(Box::new(BufReader::new(PipeRead {
            rx,
            left: Vec::new(),
        })))
    }
    fn post(&self, _session: &str, body: &str) -> Result<(), TunnelError> {
        if let Some(e) = self.post_error.lock().unwrap().take() {
            return Err(e);
        }
        self.posts.lock().unwrap().push(body.to_owned());
        Ok(())
    }
}

const SESSION: &str = "SESSIONSESSIONSESSION";
fn hello() -> String {
    format!(
        "{{\"t\":\"hello\",\"session\":\"{SESSION}\",\"server_id\":\"x\",\"now\":1,\"heartbeat_s\":20}}\n"
    )
}

struct Rig {
    fake: Arc<Fake>,
    feed: Sender<Vec<u8>>,
    settings: Settings,
}

fn rig(sshd: SocketAddr) -> Rig {
    let (feed, rx) = channel();
    let fake = Arc::new(Fake {
        stream: Mutex::new(Some(rx)),
        posts: Mutex::new(Vec::new()),
        post_error: Mutex::new(None),
        open_error: Mutex::new(None),
    });
    let settings = Settings {
        agent_addr: sshd,
        health_every: Duration::from_millis(150),
        batch_window: Duration::from_millis(10),
        ..Settings::default()
    };
    Rig {
        fake,
        feed,
        settings,
    }
}

impl Rig {
    fn send(&self, line: &str) {
        // the session may be over already (then nobody reads): not an error of the test
        let _ = self.feed.send(
            format!(
                "{line}
"
            )
            .into_bytes(),
        );
    }
    fn run(&self) -> thread::JoinHandle<End> {
        let (fake, settings) = (self.fake.clone(), self.settings.clone());
        let health: Arc<dyn Fn() -> String + Send + Sync> =
            Arc::new(|| r#"{"ts":1,"version":"t","checks":[],"metrics":{},"units":{}}"#.to_owned());
        thread::spawn(move || run_session(&fake, &settings, &health, &|| {}))
    }
    /// All frames posted so far, as parsed JSON.
    fn frames(&self) -> Vec<Value> {
        self.fake
            .posts
            .lock()
            .unwrap()
            .iter()
            .flat_map(|b| {
                b.lines()
                    .map(|l| serde_json::from_str::<Value>(l).unwrap())
                    .collect::<Vec<_>>()
            })
            .collect()
    }
    fn wait_for(&self, what: &str, pred: impl Fn(&[Value]) -> bool) -> Vec<Value> {
        let end = Instant::now() + Duration::from_secs(5);
        loop {
            let f = self.frames();
            if pred(&f) {
                return f;
            }
            assert!(
                Instant::now() < end,
                "timeout waiting for {what}; frames: {f:?}"
            );
            thread::sleep(Duration::from_millis(10));
        }
    }
}

fn fake_sshd() -> (TcpListener, SocketAddr) {
    let l = TcpListener::bind("127.0.0.1:0").unwrap();
    let a = l.local_addr().unwrap();
    (l, a)
}

fn data_frame(ch: u32, bytes: &[u8]) -> String {
    format!(
        r#"{{"t":"data","ch":{ch},"d":"{}"}}"#,
        b64::encode_url(bytes)
    )
}

// ---------------------------------------------------------------- the line

#[test]
fn bytes_travel_both_ways_through_a_channel_to_the_agent_sshd() {
    let (sshd, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    r.send(r#"{"t":"open","ch":1}"#);
    let (mut conn, _) = sshd.accept().unwrap();
    r.wait_for("opened", |f| {
        f.iter().any(|x| x["t"] == "opened" && x["ch"] == 1)
    });
    // sshd speaks first (its banner)
    conn.write_all(b"SSH-2.0-OpenSSH_9.6\r\n").unwrap();
    let f = r.wait_for("banner", |f| f.iter().any(|x| x["t"] == "data"));
    let d = f.iter().find(|x| x["t"] == "data").unwrap();
    assert_eq!(
        b64::decode_url(d["d"].as_str().unwrap()).unwrap(),
        b"SSH-2.0-OpenSSH_9.6\r\n"
    );
    // Wave speaks
    r.send(&data_frame(1, b"SSH-2.0-wave\r\n"));
    let mut got = [0u8; 14];
    conn.set_read_timeout(Some(Duration::from_secs(5))).unwrap();
    conn.read_exact(&mut got).unwrap();
    assert_eq!(&got, b"SSH-2.0-wave\r\n");
    // Wave closes: sshd sees the end
    r.send(r#"{"t":"close","ch":1}"#);
    let mut rest = Vec::new();
    conn.read_to_end(&mut rest).unwrap();
    assert!(rest.is_empty());
    r.send(r#"{"t":"bye","reason":"max_age"}"#);
    assert_eq!(h.join().unwrap(), End::Retry("bye: max_age".into()));
}

#[test]
fn a_frame_cannot_name_another_target_and_a_dead_agent_sshd_is_reported() {
    let (sshd, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    // extra fields are ignored: the daemon only ever connects to its fixed address
    r.send(r#"{"t":"open","ch":1,"host":"10.9.9.9","port":22,"addr":"1.2.3.4:25"}"#);
    let (_conn, peer) = sshd.accept().unwrap();
    assert!(peer.ip().is_loopback());
    r.wait_for("opened", |f| f.iter().any(|x| x["t"] == "opened"));
    drop(sshd);
    // now nothing listens there any more
    r.send(r#"{"t":"open","ch":2}"#);
    r.wait_for("open_failed", |f| {
        f.iter().any(|x| x["t"] == "open_failed" && x["ch"] == 2)
    });
    r.send(r#"{"t":"bye","reason":"x"}"#);
    h.join().unwrap();
}

#[test]
fn at_most_four_channels_and_a_channel_number_is_not_used_twice() {
    let (sshd, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    let mut conns = Vec::new();
    for ch in 1..=4 {
        r.send(&format!(r#"{{"t":"open","ch":{ch}}}"#));
        conns.push(sshd.accept().unwrap());
    }
    r.wait_for("four opened", |f| {
        f.iter().filter(|x| x["t"] == "opened").count() == 4
    });
    r.send(r#"{"t":"open","ch":5}"#);
    r.wait_for("fifth refused", |f| {
        f.iter().any(|x| x["t"] == "open_failed" && x["ch"] == 5)
    });
    r.send(r#"{"t":"close","ch":1}"#);
    // ch 2 again while it is still open
    r.send(r#"{"t":"open","ch":2}"#);
    r.wait_for("duplicate refused", |f| {
        f.iter().any(|x| x["t"] == "open_failed" && x["ch"] == 2)
    });
    r.send(r#"{"t":"bye","reason":"x"}"#);
    h.join().unwrap();
}

#[test]
fn a_burst_of_open_frames_cannot_go_past_the_limit() {
    let (sshd, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    // eight opens at once: the ones still connecting count too
    for ch in 1..=8 {
        r.send(&format!(r#"{{"t":"open","ch":{ch}}}"#));
    }
    let mut conns = Vec::new();
    for _ in 0..4 {
        conns.push(sshd.accept().unwrap());
    }
    let f = r.wait_for("all answered", |f| {
        f.iter()
            .filter(|x| x["t"] == "opened" || x["t"] == "open_failed")
            .count()
            == 8
    });
    assert_eq!(f.iter().filter(|x| x["t"] == "opened").count(), 4, "{f:?}");
    assert_eq!(f.iter().filter(|x| x["t"] == "open_failed").count(), 4);
    r.send(r#"{"t":"bye","reason":"x"}"#);
    h.join().unwrap();
}

#[test]
fn when_sshd_hangs_up_wave_is_told_once() {
    let (sshd, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    r.send(r#"{"t":"open","ch":1}"#);
    let (conn, _) = sshd.accept().unwrap();
    r.wait_for("opened", |f| f.iter().any(|x| x["t"] == "opened"));
    drop(conn);
    r.wait_for("close", |f| {
        f.iter().any(|x| x["t"] == "close" && x["ch"] == 1)
    });
    r.send(r#"{"t":"bye","reason":"x"}"#);
    h.join().unwrap();
    assert_eq!(r.frames().iter().filter(|x| x["t"] == "close").count(), 1);
}

#[test]
fn health_is_sent_at_once_and_then_regularly() {
    let (_sshd, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    r.wait_for("two reports", |f| {
        f.iter().filter(|x| x["t"] == "health").count() >= 2
    });
    r.send(r#"{"t":"bye","reason":"x"}"#);
    h.join().unwrap();
}

#[test]
fn bye_revoked_and_an_error_answer_decide_whether_to_come_back() {
    let (_s, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    r.send(r#"{"t":"bye","reason":"revoked"}"#);
    assert_eq!(h.join().unwrap(), End::Revoked);

    // the answer is an error, not a stream (fail-with-body)
    let r = rig(addr);
    r.send(r#"{"error":{"code":"server_revoked","message_key":"error.server_revoked"}}"#);
    assert_eq!(r.run().join().unwrap(), End::Revoked);
    let r = rig(addr);
    r.send(r#"{"error":{"code":"invalid_signature"}}"#);
    assert_eq!(
        r.run().join().unwrap(),
        End::Retry("refused: invalid_signature".into()),
        "a wrong clock must not end the daemon for good"
    );
    let r = rig(addr);
    r.send("<html>502 Bad Gateway</html>");
    assert!(matches!(r.run().join().unwrap(), End::Retry(_)));
    let r = rig(addr);
    *r.fake.open_error.lock().unwrap() = Some(TunnelError::Refused("server_revoked".into()));
    assert_eq!(r.run().join().unwrap(), End::Revoked);
    let r = rig(addr);
    *r.fake.open_error.lock().unwrap() = Some(TunnelError::Network("no route".into()));
    assert_eq!(r.run().join().unwrap(), End::Retry("no route".into()));
    let r = rig(addr);
    drop(r.feed.clone());
    let Rig {
        fake,
        feed,
        settings,
    } = r;
    drop(feed);
    let h: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(String::new);
    assert!(
        matches!(run_session(&fake, &settings, &h, &|| {}), End::Retry(_)),
        "the line closes before a hello"
    );
}

#[test]
fn a_refused_post_ends_the_session_and_a_revoked_one_ends_it_for_good() {
    let (_s, addr) = fake_sshd();
    let r = rig(addr);
    *r.fake.post_error.lock().unwrap() = Some(TunnelError::Refused("server_revoked".into()));
    r.send(hello().trim());
    let h = r.run();
    // the next frame from Wave wakes the loop, which then sees the abort
    for _ in 0..100 {
        r.send(r#"{"t":"ping"}"#);
        if h.is_finished() {
            break;
        }
        thread::sleep(Duration::from_millis(20));
    }
    assert_eq!(h.join().unwrap(), End::Revoked);
}

#[test]
fn unknown_and_broken_frames_are_ignored() {
    let (_s, addr) = fake_sshd();
    let r = rig(addr);
    r.send(hello().trim());
    let h = r.run();
    for junk in [
        r#"{"t":"teleport","ch":1}"#,
        "not json",
        r#"{"t":"open","ch":0}"#,
        r#"{"t":"data","ch":99,"d":"AAAA"}"#,
        "",
        r#"{"t":"hello","session":"x"}"#,
    ] {
        r.send(junk);
    }
    r.send(r#"{"t":"bye","reason":"x"}"#);
    assert_eq!(
        h.join().unwrap(),
        End::Retry("bye: x".into()),
        "still alive after the junk"
    );
}

#[test]
fn pauses_grow_and_stop_at_the_maximum() {
    let s = Settings::default();
    let p = |n| backoff(&s, n);
    assert!(p(0) >= Duration::from_secs(1) && p(0) < Duration::from_millis(1300));
    assert!(p(2) >= Duration::from_secs(4));
    assert!(p(10) >= Duration::from_secs(30) && p(10) < Duration::from_secs(38));
    assert!(p(30) < Duration::from_secs(38), "no overflow");
}

#[test]
fn run_forever_keeps_trying_and_stops_when_revoked() {
    struct Flaky {
        n: Mutex<u32>,
    }
    impl Transport for Flaky {
        fn open_stream(&self) -> Result<Box<dyn BufRead + Send>, TunnelError> {
            let mut n = self.n.lock().unwrap();
            *n += 1;
            if *n < 3 {
                Err(TunnelError::Network("down".into()))
            } else {
                Err(TunnelError::Refused("server_revoked".into()))
            }
        }
        fn post(&self, _: &str, _: &str) -> Result<(), TunnelError> {
            Ok(())
        }
    }
    let s = Settings {
        backoff_start: Duration::from_millis(5),
        backoff_max: Duration::from_millis(10),
        ..Settings::default()
    };
    let states = Mutex::new(Vec::<String>::new());
    let h: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(String::new);
    let t = Arc::new(Flaky { n: Mutex::new(0) });
    let end = run_forever(
        t.clone(),
        &s,
        h,
        &|st, _| states.lock().unwrap().push(st.to_owned()),
        &AtomicBool::new(false),
    );
    assert_eq!(end, End::Revoked);
    assert_eq!(*t.n.lock().unwrap(), 3);
    let st = states.lock().unwrap().clone();
    assert!(
        st.contains(&"reconnecting".to_owned()) && st.last().unwrap() == "revoked",
        "{st:?}"
    );
}

// ---------------------------------------------------------------- curl

#[test]
fn curl_config_is_quoted_and_https_only_for_https() {
    let h = vec![("X-A".to_owned(), "va\"l\nue".to_owned())];
    let c = curl_config(
        "https://relay.example",
        "POST",
        "/v1/relay/up",
        &h,
        Some("{\"t\":\"x\"}\n"),
        false,
    );
    assert!(c.contains("proto = \"=https\""));
    assert!(c.contains("url = \"https://relay.example/v1/relay/up\""));
    assert!(
        c.contains("header = \"X-A: va\\\"l\\nue\""),
        "no raw newline can start a new setting: {c}"
    );
    assert_eq!(c.lines().filter(|l| l.starts_with("url")).count(), 1);
    let local = curl_config(
        "http://127.0.0.1:9",
        "GET",
        "/v1/relay/stream",
        &[],
        None,
        true,
    );
    assert!(
        local.contains("proto = \"=http,https\"")
            && local.contains("no-buffer")
            && local.contains("speed-time")
    );
    assert!(!c.contains("no-buffer"));
}

/// A minimal relay on a local port, speaking real HTTP to the real curl: it checks the signature of every
/// request the way the Node relay does, serves a stream (hello, then bye revoked once a health report came in)
/// and answers posts.
#[test]
fn the_real_curl_transport_signs_streams_and_posts() {
    use ed25519_dalek::{Signature, Verifier, VerifyingKey};
    let key = ServerKey::from_seed([9; 32]);
    let vk = VerifyingKey::from_bytes(
        &b64::decode_url(&key.public_b64u())
            .unwrap()
            .try_into()
            .unwrap(),
    )
    .unwrap();
    let server_ref = "11111111-1111-4111-8111-111111111111.22222222-2222-4222-8222-222222222222";
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Arc<Mutex<Vec<(String, bool)>>> = Arc::new(Mutex::new(Vec::new()));
    let got_health = Arc::new(AtomicBool::new(false));

    let (seen2, got_health2) = (seen.clone(), got_health.clone());
    thread::spawn(move || {
        for conn in listener.incoming() {
            let Ok(mut c) = conn else { break };
            let (seen, got_health) = (seen2.clone(), got_health2.clone());
            thread::spawn(move || {
                // request head
                let mut head = Vec::new();
                let mut b = [0u8; 1];
                while !head.ends_with(b"\r\n\r\n") {
                    if c.read(&mut b).unwrap_or(0) == 0 {
                        return;
                    }
                    head.push(b[0]);
                }
                let text = String::from_utf8_lossy(&head).to_string();
                let mut lines = text.split("\r\n");
                let mut first = lines.next().unwrap().split(' ');
                let (method, target) = (
                    first.next().unwrap().to_owned(),
                    first.next().unwrap().to_owned(),
                );
                let hdr = |name: &str| {
                    text.lines().find_map(|l| {
                        l.to_ascii_lowercase()
                            .strip_prefix(&format!("{name}: "))
                            .map(|_| l[name.len() + 2..].trim().to_owned())
                    })
                };
                let len: usize = hdr("content-length")
                    .and_then(|v| v.parse().ok())
                    .unwrap_or(0);
                let mut body = vec![0u8; len];
                c.read_exact(&mut body).unwrap();
                // the signature, checked like the relay does
                let session = hdr("x-sw-session");
                let signed = signing_string(
                    &method,
                    &target,
                    &hdr("x-sw-time").unwrap(),
                    &hdr("x-sw-nonce").unwrap(),
                    &body,
                    server_ref,
                    session.as_deref(),
                );
                let sig: [u8; 64] = b64::decode_url(&hdr("x-sw-sig").unwrap())
                    .unwrap()
                    .try_into()
                    .unwrap();
                let ok = vk
                    .verify(signed.as_bytes(), &Signature::from_bytes(&sig))
                    .is_ok()
                    && hdr("authorization").as_deref()
                        == Some(&format!("Stonewave-Server {server_ref}"));
                seen.lock()
                    .unwrap()
                    .push((format!("{method} {target}"), ok));
                if !ok {
                    let _ = c.write_all(b"HTTP/1.1 401 Unauthorized\r\nConnection: close\r\nContent-Length: 0\r\n\r\n");
                    return;
                }
                if target == "/v1/relay/stream" {
                    let _ = c.write_all(format!("HTTP/1.1 200 OK\r\nContent-Type: application/x-ndjson\r\nConnection: close\r\n\r\n{}", hello()).as_bytes());
                    while !got_health.load(Ordering::Relaxed) {
                        thread::sleep(Duration::from_millis(20));
                    }
                    let _ = c.write_all(b"{\"t\":\"bye\",\"reason\":\"revoked\"}\n");
                } else {
                    if String::from_utf8_lossy(&body).contains("\"t\":\"health\"") {
                        got_health.store(true, Ordering::Relaxed);
                    }
                    let _ = c.write_all(b"HTTP/1.1 200 OK\r\nConnection: close\r\nContent-Type: application/json\r\nContent-Length: 11\r\n\r\n{\"ok\":true}");
                }
            });
        }
    });

    let t = Arc::new(CurlTransport {
        base: format!("http://127.0.0.1:{port}"),
        server_ref: server_ref.to_owned(),
        key,
    });
    let s = Settings {
        backoff_start: Duration::from_millis(10),
        health_every: Duration::from_secs(60),
        ..Settings::default()
    };
    let h: Arc<dyn Fn() -> String + Send + Sync> =
        Arc::new(|| r#"{"ts":1,"version":"t","checks":[],"metrics":{},"units":{}}"#.to_owned());
    let end = run_session(&t, &s, &h, &|| {});
    assert_eq!(end, End::Revoked);
    let seen = seen.lock().unwrap().clone();
    assert!(
        seen.iter()
            .any(|(r, ok)| r == "GET /v1/relay/stream" && *ok),
        "{seen:?}"
    );
    assert!(
        seen.iter().any(|(r, ok)| r == "POST /v1/relay/up" && *ok),
        "{seen:?}"
    );
    assert!(
        seen.iter().all(|(_, ok)| *ok),
        "every request carried a valid signature: {seen:?}"
    );
}

#[test]
fn an_unreachable_relay_is_a_network_error_not_a_refusal() {
    let t = CurlTransport {
        base: "http://127.0.0.1:1".into(),
        server_ref: "x".into(),
        key: ServerKey::from_seed([1; 32]),
    };
    match t.post("S", "{}\n") {
        Err(TunnelError::Network(_)) => {}
        other => panic!("{other:?}"),
    }
}

#[test]
fn one_request_never_carries_more_than_64_frames() {
    // The relay answers `400 bad_frame` to a request with more; a burst of small frames must be split.
    let fake = Fake {
        stream: Mutex::new(None),
        posts: Mutex::new(Vec::new()),
        post_error: Mutex::new(None),
        open_error: Mutex::new(None),
    };
    let (tx, rx) = mpsc::sync_channel::<Up>(512);
    for ch in 1..=300u32 {
        tx.send(Up::Close(ch)).unwrap();
    }
    drop(tx);
    upload_loop(
        &fake,
        SESSION,
        rx,
        &Mutex::new(None),
        &AtomicBool::new(false),
        Duration::from_millis(40),
    );
    let posts = fake.posts.lock().unwrap();
    let counts: Vec<usize> = posts.iter().map(|b| b.lines().count()).collect();
    assert!(counts.iter().all(|n| (1..=64).contains(n)), "{counts:?}");
    assert_eq!(counts.iter().sum::<usize>(), 300, "no frame is lost");
}
