//! The tunnel: `stonewaved` holds one outbound connection to the relay and does exactly four things
//! with it (the design of the server daemon):
//!
//! 1. keep the line (reconnect with a growing pause),
//! 2. hand byte streams to the agent sshd on `127.0.0.1:2222`,
//! 3. report health once a minute,
//! 4. obey "bye" (a revoked server stops for good).
//!
//! It cannot run a command. The only thing it connects to is the agent sshd address in [`Settings`],
//! and a frame from Wave cannot name another one: `open` carries a channel number and nothing else.
//! What the agent then does is decided by that sshd, with certificates.
//!
//! The wire is two HTTPS requests that the system's curl makes: a long `GET /v1/relay/stream` whose
//! answer never ends (Wave to server) and short `POST /v1/relay/up` requests (server to Wave). Both are
//! signed with the server key. [`Transport`] is the seam for tests.

use std::collections::HashMap;
use std::io::{BufRead, Read, Write};
use std::net::{Shutdown, SocketAddr, TcpStream};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::mpsc::{self, Receiver, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

use serde_json::Value;

use crate::connect::cfg_quote as quote;
use crate::relay::{Down, MAX_DATA, ServerKey, Up, now_ms, parse_down, signed_headers};

pub const AGENT_ADDR: ([u8; 4], u16) = ([127, 0, 0, 1], crate::setup::AGENT_PORT);
/// systemd must not restart a daemon whose server was revoked (`RestartPreventExitStatus=78`).
pub const EXIT_REVOKED: u8 = 78;

#[derive(Debug, Clone)]
pub struct Settings {
    pub agent_addr: SocketAddr,
    pub health_every: Duration,
    pub max_channels: usize,
    pub connect_timeout: Duration,
    /// Frames that come close together are sent in one request.
    pub batch_window: Duration,
    pub backoff_start: Duration,
    pub backoff_max: Duration,
    /// Wave's "revoked" is believed only when a fresh connection says it again this much later (see `run_forever`).
    pub revoke_confirm: Duration,
}

impl Default for Settings {
    fn default() -> Settings {
        Settings {
            agent_addr: SocketAddr::from(AGENT_ADDR),
            health_every: Duration::from_secs(60),
            max_channels: 4,
            connect_timeout: Duration::from_secs(2),
            batch_window: Duration::from_millis(40),
            backoff_start: Duration::from_secs(1),
            backoff_max: Duration::from_secs(30),
            revoke_confirm: Duration::from_secs(20),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum TunnelError {
    /// No answer: curl missing, DNS, TLS, timeout, reset.
    Network(String),
    /// Wave answered with an error code.
    Refused(String),
}

/// How the daemon reaches the relay.
pub trait Transport: Send + Sync + 'static {
    /// Opens the stream. The reader yields the lines of the answer; dropping it ends the request.
    fn open_stream(&self) -> Result<Box<dyn BufRead + Send>, TunnelError>;
    /// Sends frames (NDJSON) for this session.
    fn post(&self, session: &str, body: &str) -> Result<(), TunnelError>;
}

#[derive(Debug, PartialEq, Eq)]
pub enum End {
    Retry(String),
    /// Wave cut this server off. Nothing to retry.
    Revoked,
}

type Chans = Arc<Mutex<HashMap<u32, Chan>>>;
struct Chan {
    sock: TcpStream,
    to_sshd: SyncSender<Vec<u8>>,
}

fn close_chan(c: &Chan) {
    let _ = c.sock.shutdown(Shutdown::Both);
}

fn read_line(r: &mut dyn BufRead) -> Result<Option<String>, String> {
    let mut buf = String::new();
    // A line from the relay is a frame of at most ~25 KiB: more than this is not the relay.
    let n = r
        .take(64 * 1024)
        .read_line(&mut buf)
        .map_err(|e| e.to_string())?;
    if n == 0 {
        return Ok(None);
    }
    if !buf.ends_with('\n') && n >= 64 * 1024 {
        return Err("line too long".into());
    }
    Ok(Some(buf.trim_end().to_owned()))
}

/// The error code in a Wave error answer, if the text is one.
pub fn error_code(text: &str) -> Option<String> {
    let v: Value = serde_json::from_str(text.trim()).ok()?;
    v.get("error")?;
    Some(crate::connect::error_code(&v))
}

/// One life of the line: connect, hello, serve frames until it ends.
pub fn run_session<T: Transport>(
    t: &Arc<T>,
    s: &Settings,
    health: &Arc<dyn Fn() -> String + Send + Sync>,
    on_connected: &dyn Fn(),
) -> End {
    let mut reader = match t.open_stream() {
        Ok(r) => r,
        Err(TunnelError::Refused(c)) if c == "server_revoked" => return End::Revoked,
        Err(TunnelError::Refused(c)) => return End::Retry(format!("refused: {c}")),
        Err(TunnelError::Network(m)) => return End::Retry(m),
    };
    let first = match read_line(&mut *reader) {
        Ok(Some(l)) => l,
        Ok(None) => return End::Retry("no answer from the relay (is it reachable?)".into()),
        Err(e) => return End::Retry(e),
    };
    let session = match parse_down(&first) {
        Some(Down::Hello { session, .. }) => session,
        // Not a hello: with `fail-with-body` this is the error answer.
        _ => {
            return match error_code(&first) {
                Some(c) if c == "server_revoked" => End::Revoked,
                Some(c) => End::Retry(format!("refused: {c}")),
                None => End::Retry("the answer was not from the relay".into()),
            };
        }
    };
    on_connected();

    let done = Arc::new(AtomicBool::new(false));
    let abort: Arc<Mutex<Option<End>>> = Arc::new(Mutex::new(None));
    let (tx, rx) = mpsc::sync_channel::<Up>(256);
    let chans: Chans = Arc::new(Mutex::new(HashMap::new()));
    let opening = Arc::new(AtomicUsize::new(0));

    let uploader = {
        let (t, abort, done, sess, window) = (
            t.clone(),
            abort.clone(),
            done.clone(),
            session.clone(),
            s.batch_window,
        );
        thread::spawn(move || upload_loop(&*t, &sess, rx, &abort, &done, window))
    };
    let reporter = {
        let (tx, done, health, every) = (tx.clone(), done.clone(), health.clone(), s.health_every);
        thread::spawn(move || {
            let mut next = Instant::now();
            while !done.load(Ordering::Relaxed) {
                if Instant::now() >= next {
                    if tx.try_send(Up::Health(health())).is_err() && done.load(Ordering::Relaxed) {
                        break;
                    }
                    next = Instant::now() + every;
                }
                thread::sleep(Duration::from_millis(100));
            }
        })
    };

    let end = loop {
        if let Some(e) = abort.lock().unwrap().take() {
            break e;
        }
        let line = match read_line(&mut *reader) {
            Ok(Some(l)) => l,
            Ok(None) => break End::Retry("the line ended".into()),
            Err(e) => break End::Retry(e),
        };
        if line.is_empty() {
            continue;
        }
        match parse_down(&line) {
            Some(Down::Open { ch }) => open_channel(s, &chans, &opening, &tx, ch),
            Some(Down::Data { ch, data }) => {
                let target = chans.lock().unwrap().get(&ch).map(|c| c.to_sshd.clone());
                if let Some(q) = target {
                    // A writer that cannot keep up must not stall the line: that channel ends.
                    if q.try_send(data).is_err() {
                        end_channel(&chans, &tx, ch);
                    }
                }
            }
            Some(Down::Close { ch }) => {
                if let Some(c) = chans.lock().unwrap().remove(&ch) {
                    close_chan(&c);
                }
            }
            Some(Down::Bye { reason }) => {
                break if reason == "revoked" {
                    End::Revoked
                } else {
                    End::Retry(format!("bye: {reason}"))
                };
            }
            Some(Down::Hello { .. } | Down::Ping | Down::Unknown) | None => {}
        }
    };

    done.store(true, Ordering::Relaxed);
    for (_, c) in chans.lock().unwrap().drain() {
        close_chan(&c);
    }
    drop(tx);
    drop(reader);
    let _ = reporter.join();
    let _ = uploader.join();
    end
}

/// Ends a channel from our side and tells Wave, once.
fn end_channel(chans: &Chans, tx: &SyncSender<Up>, ch: u32) {
    let removed = chans.lock().unwrap().remove(&ch);
    if let Some(c) = removed {
        close_chan(&c);
        // The close must reach Wave even when the queue is full (a big transfer): wait a little for room instead of dropping it.
        let mut frame = Up::Close(ch);
        for _ in 0..100 {
            match tx.try_send(frame) {
                Ok(()) | Err(mpsc::TrySendError::Disconnected(_)) => break,
                Err(mpsc::TrySendError::Full(f)) => {
                    frame = f;
                    thread::sleep(Duration::from_millis(20));
                }
            }
        }
    }
}

/// `open`: connect to the agent sshd (the fixed address) on a side thread, then answer.
fn open_channel(
    s: &Settings,
    chans: &Chans,
    opening: &Arc<AtomicUsize>,
    tx: &SyncSender<Up>,
    ch: u32,
) {
    // Channels that are still connecting count too, so a burst of `open` frames cannot go past the limit.
    if chans.lock().unwrap().len() + opening.fetch_add(1, Ordering::SeqCst) >= s.max_channels {
        opening.fetch_sub(1, Ordering::SeqCst);
        let _ = tx.try_send(Up::OpenFailed(ch));
        return;
    }
    let s_max = s.max_channels;
    let (addr, timeout, chans, tx, opening) = (
        s.agent_addr,
        s.connect_timeout,
        chans.clone(),
        tx.clone(),
        opening.clone(),
    );
    thread::spawn(move || {
        let connected = TcpStream::connect_timeout(&addr, timeout);
        opening.fetch_sub(1, Ordering::SeqCst);
        let Ok(sock) = connected else {
            let _ = tx.send(Up::OpenFailed(ch));
            return;
        };
        let _ = sock.set_nodelay(true);
        let (Ok(mut for_reader), Ok(mut for_writer), Ok(for_map)) =
            (sock.try_clone(), sock.try_clone(), sock.try_clone())
        else {
            let _ = tx.send(Up::OpenFailed(ch));
            return;
        };
        let (to_sshd, from_wave) = mpsc::sync_channel::<Vec<u8>>(64);
        {
            let mut map = chans.lock().unwrap();
            if map.contains_key(&ch) || map.len() >= s_max {
                drop(map);
                let _ = sock.shutdown(Shutdown::Both);
                let _ = tx.send(Up::OpenFailed(ch));
                return;
            }
            map.insert(
                ch,
                Chan {
                    sock: for_map,
                    to_sshd,
                },
            );
        }
        let _ = tx.send(Up::Opened(ch));
        // Wave to sshd
        let (chans_w, tx_w) = (chans.clone(), tx.clone());
        thread::spawn(move || {
            for chunk in from_wave {
                if for_writer.write_all(&chunk).is_err() {
                    break;
                }
            }
            end_channel(&chans_w, &tx_w, ch);
        });
        // sshd to Wave
        // A short read timeout, so the reader notices that the channel was ended from our side even where
        // `shutdown` does not wake a blocked read (Windows).
        let _ = for_reader.set_read_timeout(Some(Duration::from_millis(250)));
        let mut buf = vec![0u8; MAX_DATA];
        loop {
            match for_reader.read(&mut buf) {
                Ok(0) => break,
                Ok(n) => {
                    if tx.send(Up::Data(ch, buf[..n].to_vec())).is_err() {
                        break;
                    }
                }
                Err(e)
                    if matches!(
                        e.kind(),
                        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                    ) =>
                {
                    if !chans.lock().unwrap().contains_key(&ch) {
                        break;
                    }
                }
                Err(_) => break,
            }
        }
        end_channel(&chans, &tx, ch);
    });
}

fn upload_loop<T: Transport>(
    t: &T,
    session: &str,
    rx: Receiver<Up>,
    abort: &Mutex<Option<End>>,
    done: &AtomicBool,
    window: Duration,
) {
    const MAX_TEXT: usize = 48 * 1024;
    while let Ok(first) = rx.recv() {
        let mut only_health = matches!(first, Up::Health(_));
        let mut body = first.line() + "\n";
        let until = Instant::now() + window;
        while body.len() < MAX_TEXT {
            match rx.recv_timeout(until.saturating_duration_since(Instant::now())) {
                Ok(f) => {
                    only_health &= matches!(f, Up::Health(_));
                    body.push_str(&f.line());
                    body.push('\n');
                }
                Err(RecvTimeoutError::Timeout | RecvTimeoutError::Disconnected) => break,
            }
        }
        let mut last = None;
        for attempt in 0..3 {
            match t.post(session, &body) {
                Ok(()) => {
                    last = None;
                    break;
                }
                Err(e @ TunnelError::Refused(_)) => {
                    last = Some(e);
                    break; // asking again does not change an answer
                }
                Err(e) => {
                    last = Some(e);
                    thread::sleep(Duration::from_millis(300 * (attempt + 1)));
                }
            }
        }
        if let Some(e) = last {
            if done.load(Ordering::Relaxed) {
                return;
            }
            // A lost health report is replaced by the next one; lost bytes break the channel, so the line is rebuilt.
            if !only_health {
                *abort.lock().unwrap() = Some(match e {
                    TunnelError::Refused(c) if c == "server_revoked" => End::Revoked,
                    TunnelError::Refused(c) => End::Retry(format!("refused: {c}")),
                    TunnelError::Network(m) => End::Retry(m),
                });
                return;
            }
            if let TunnelError::Refused(c) = &e
                && c == "server_revoked"
            {
                *abort.lock().unwrap() = Some(End::Revoked);
                return;
            }
        }
    }
}

/// Pause before the next try: doubles up to the maximum, with up to a quarter of random extra.
pub fn backoff(s: &Settings, failures: u32) -> Duration {
    let base = s
        .backoff_start
        .saturating_mul(1u32 << failures.min(10))
        .min(s.backoff_max);
    let jitter = u64::from(crate::relay::random_bytes::<1>()[0]) * base.as_millis() as u64 / 1024;
    base + Duration::from_millis(jitter)
}

/// Keeps the line for ever. Returns only when Wave revoked this server (or `stop` is set by a test).
pub fn run_forever<T: Transport>(
    t: Arc<T>,
    s: &Settings,
    health: Arc<dyn Fn() -> String + Send + Sync>,
    status: &dyn Fn(&str, &str),
    stop: &AtomicBool,
) -> End {
    let mut failures = 0u32;
    // What Wave sends down is protected by TLS only (no signature of Wave's own yet, see the debts). One "revoked" that
    // arrives on a line, however it got there, must not end the daemon for good, because systemd never restarts it
    // after that. So the first claim only closes the line; the daemon waits and asks on a new connection, and a line that
    // comes up healthy in between cancels the claim. A real revoke is refused again by the relay at once.
    let claims = AtomicUsize::new(0);
    while !stop.load(Ordering::Relaxed) {
        status("connecting", "");
        let started = Instant::now();
        let end = run_session(&t, s, &health, &|| {
            claims.store(0, Ordering::Relaxed);
            status("connected", "");
        });
        match end {
            End::Revoked => {
                if claims.fetch_add(1, Ordering::Relaxed) + 1 >= 2 {
                    status("revoked", "Wave cut this server off");
                    return End::Revoked;
                }
                status(
                    "checking",
                    "Wave says it cut this server off; asking again on a new connection",
                );
                let mut left = s.revoke_confirm;
                while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
                    let step = left.min(Duration::from_millis(200));
                    thread::sleep(step);
                    left -= step;
                }
            }
            End::Retry(why) => {
                // A line that held for a while was healthy: start the pauses over.
                failures = if started.elapsed() > Duration::from_secs(30) {
                    0
                } else {
                    failures + 1
                };
                status("reconnecting", &why);
                let mut left = backoff(s, failures);
                while left > Duration::ZERO && !stop.load(Ordering::Relaxed) {
                    let step = left.min(Duration::from_millis(200));
                    thread::sleep(step);
                    left -= step;
                }
            }
        }
    }
    End::Retry("stopped".into())
}

// ------------------------------------------------------------------ curl

pub struct CurlTransport {
    pub base: String,
    pub server_ref: String,
    pub key: ServerKey,
}

/// The curl config of one signed request. Headers carry a signature that is useless a minute later; the
/// config goes to curl on stdin anyway, so nothing shows up in a process list.
pub fn curl_config(
    base: &str,
    method: &str,
    target: &str,
    headers: &[(String, String)],
    body: Option<&str>,
    streaming: bool,
) -> String {
    let mut c = String::from("silent\nshow-error\nfail-with-body\nconnect-timeout = 15\n");
    if streaming {
        // never buffered; ends after 11 minutes (Wave ends it after 10); dead if nothing arrives for 90 s
        c.push_str("no-buffer\nmax-time = 660\nspeed-limit = 1\nspeed-time = 90\n");
    } else {
        c.push_str("max-time = 30\nmax-filesize = 1048576\nwrite-out = \"\\n%{http_code}\"\n");
    }
    c.push_str(if base.starts_with("https://") {
        "proto = \"=https\"\n"
    } else {
        "proto = \"=http,https\"\n"
    });
    c.push_str(&format!(
        "user-agent = {}\n",
        quote(&format!("stonewave/{}", crate::VERSION))
    ));
    c.push_str(&format!(
        "request = {}\nurl = {}\n",
        quote(method),
        quote(&format!("{base}{target}"))
    ));
    for (k, v) in headers {
        c.push_str(&format!("header = {}\n", quote(&format!("{k}: {v}"))));
    }
    if let Some(b) = body {
        c.push_str("header = \"Content-Type: application/x-ndjson\"\n");
        c.push_str(&format!("data-binary = {}\n", quote(b)));
    }
    c
}

fn spawn_curl(config: &str, stdout: Stdio) -> Result<Child, TunnelError> {
    let mut child = Command::new("curl")
        // `-q` first: curl then ignores ~/.curlrc, which could add a proxy, headers or a trusted certificate.
        .args(["-q", "--config", "-"])
        .stdin(Stdio::piped())
        .stdout(stdout)
        .stderr(Stdio::null())
        .spawn()
        .map_err(|e| TunnelError::Network(format!("curl: {e}")))?;
    if let Some(mut s) = child.stdin.take() {
        let _ = s.write_all(config.as_bytes());
    }
    Ok(child)
}

/// The answer of the stream request. Dropping it stops curl.
struct CurlStream {
    child: Child,
    out: std::io::BufReader<std::process::ChildStdout>,
}

impl Read for CurlStream {
    fn read(&mut self, buf: &mut [u8]) -> std::io::Result<usize> {
        self.out.read(buf)
    }
}
impl BufRead for CurlStream {
    fn fill_buf(&mut self) -> std::io::Result<&[u8]> {
        self.out.fill_buf()
    }
    fn consume(&mut self, amt: usize) {
        self.out.consume(amt);
    }
}
impl Drop for CurlStream {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

impl Transport for CurlTransport {
    fn open_stream(&self) -> Result<Box<dyn BufRead + Send>, TunnelError> {
        let target = "/v1/relay/stream";
        let headers = signed_headers(
            &self.key,
            &self.server_ref,
            "GET",
            target,
            b"",
            None,
            now_ms(),
        );
        let config = curl_config(&self.base, "GET", target, &headers, None, true);
        let mut child = spawn_curl(&config, Stdio::piped())?;
        let out = child.stdout.take().expect("piped");
        Ok(Box::new(CurlStream {
            child,
            out: std::io::BufReader::new(out),
        }))
    }

    fn post(&self, session: &str, body: &str) -> Result<(), TunnelError> {
        let target = "/v1/relay/up";
        let headers = signed_headers(
            &self.key,
            &self.server_ref,
            "POST",
            target,
            body.as_bytes(),
            Some(session),
            now_ms(),
        );
        let config = curl_config(&self.base, "POST", target, &headers, Some(body), false);
        let child = spawn_curl(&config, Stdio::piped())?;
        let out = child
            .wait_with_output()
            .map_err(|e| TunnelError::Network(e.to_string()))?;
        let text = String::from_utf8_lossy(&out.stdout);
        // with `fail-with-body` and `write-out` the answer is: body, newline, status code
        let (body, code) = text.rsplit_once('\n').unwrap_or(("", ""));
        match code.trim().parse::<u16>() {
            Ok(200) => Ok(()),
            // curl prints 000 when there was no HTTP answer at all
            Ok(0) => Err(TunnelError::Network("no answer".into())),
            // a proxy or a restart in between is not an answer of Wave: try again
            Ok(c) if c >= 500 => Err(TunnelError::Network(format!("HTTP {c}"))),
            Ok(_) => Err(TunnelError::Refused(
                error_code(body).unwrap_or_else(|| "unknown_error".into()),
            )),
            Err(_) => Err(TunnelError::Network(if out.status.success() {
                "no status code".into()
            } else {
                "no answer".into()
            })),
        }
    }
}

#[cfg(test)]
mod tests;
