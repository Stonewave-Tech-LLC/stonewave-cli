//! The full-screen menu on a real pseudo-terminal: the real binary, a real terminal driver in between, keys typed as bytes. Unix only.
//!
//! One test walks the whole wizard for an agent whose key is in an environment variable (the way a server with a secret store has it): rooms
//! from a small Wave on the loopback address, the dry-run preview, a real `--once` test of the station with a stand-in for Claude Code, and
//! checks that no key is written anywhere and that the terminal is given back cleanly.
#![cfg(unix)]

use std::io::{BufRead, BufReader, Read, Write};
use std::net::{TcpListener, TcpStream};
use std::os::fd::FromRawFd;
use std::os::unix::fs::PermissionsExt;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::thread;
use std::time::{Duration, Instant};

const KEY: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_ptyTestKeyPtyTestKey";

struct Dir(PathBuf);

impl Dir {
    fn new(label: &str) -> Dir {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "swpty-{label}-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        let _ = std::fs::remove_dir_all(&p);
        std::fs::create_dir_all(&p).unwrap();
        Dir(p)
    }
}

impl Drop for Dir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

// ------------------------------------------------------------------ a small Wave

type Seen = Arc<Mutex<Vec<(String, String)>>>;

fn serve() -> (String, Seen) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let port = listener.local_addr().unwrap().port();
    let seen: Seen = Arc::new(Mutex::new(Vec::new()));
    let s2 = seen.clone();
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let s = s2.clone();
            thread::spawn(move || handle(stream, s));
        }
    });
    (format!("http://127.0.0.1:{port}"), seen)
}

fn handle(mut stream: TcpStream, seen: Seen) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let (mut auth, mut len, mut line) = (String::new(), 0usize, String::new());
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        let l = line.to_lowercase();
        if let Some(v) = l.strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
        if l.starts_with("authorization:") {
            auth = line.split_once(':').unwrap().1.trim().to_owned();
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok();
    let b: serde_json::Value = serde_json::from_slice(&body).unwrap_or_default();
    let tool = b["params"]["name"].as_str().unwrap_or("").to_owned();
    seen.lock().unwrap().push((tool.clone(), auth));
    let data = match tool.as_str() {
        "inbox" => serde_json::json!({ "items": [], "next_seq": 1, "tasks": [], "stuck": [] }),
        "rooms" => {
            serde_json::json!({ "items": [{ "id": "room-1", "name": "Team room" }, { "id": "room-2", "name": "Sales" }] })
        }
        _ => serde_json::json!({ "ok": true }),
    };
    let out = serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": data.to_string() }] } }).to_string();
    let resp = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

fn fake_claude(dir: &Path) {
    let p = dir.join("claude");
    // answers --version, takes `mcp add` and `mcp remove` (what connecting a Claude Code does), refuses everything else
    std::fs::write(&p, "#!/bin/sh\nif [ \"$1\" = \"--version\" ]; then echo '2.1.321 (Claude Code)'; exit 0; fi\nif [ \"$1\" = \"mcp\" ]; then exit 0; fi\nexit 1\n").unwrap();
    std::fs::set_permissions(&p, std::fs::Permissions::from_mode(0o755)).unwrap();
    // a stand-in for the system's opener: writes the link it was given into a file next to it
    let o = dir.join("xdg-open");
    std::fs::write(
        &o,
        format!(
            "#!/bin/sh\nprintf '%s' \"$1\" > '{}/opened.txt'\n",
            dir.display()
        ),
    )
    .unwrap();
    std::fs::set_permissions(&o, std::fs::Permissions::from_mode(0o755)).unwrap();
}

// ------------------------------------------------------------------ a terminal

struct Pty {
    master: std::fs::File,
    child: Child,
    seen: Vec<u8>,
    mark: usize,
}

const UP: &str = "\x1b[A";
const DOWN: &str = "\x1b[B";
const ENTER: &str = "\r";
const ESC: &str = "\x1b";
const CTRL_U: &str = "\x15";
const CTRL_C: &str = "\x03";

fn is_root() -> bool {
    // SAFETY: geteuid has no arguments and cannot fail.
    unsafe { libc::geteuid() == 0 }
}

impl Pty {
    fn spawn(mut cmd: Command, rows: u16, cols: u16) -> Pty {
        let (mut master, mut slave) = (0, 0);
        let ws = libc::winsize {
            ws_row: rows,
            ws_col: cols,
            ws_xpixel: 0,
            ws_ypixel: 0,
        };
        // SAFETY: plain libc calls on integers we own; the file descriptors are wrapped right away.
        let (master, slave) = unsafe {
            assert_eq!(
                libc::openpty(
                    &mut master,
                    &mut slave,
                    std::ptr::null_mut(),
                    std::ptr::null(),
                    &ws
                ),
                0
            );
            (
                std::fs::File::from_raw_fd(master),
                std::fs::File::from_raw_fd(slave),
            )
        };
        cmd.stdin(Stdio::from(slave.try_clone().unwrap()))
            .stdout(Stdio::from(slave.try_clone().unwrap()))
            .stderr(Stdio::from(slave));
        // SAFETY: only async-signal-safe calls between fork and exec; the terminal becomes the child's controlling terminal.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                libc::ioctl(0, libc::TIOCSCTTY as _, 0);
                Ok(())
            });
        }
        let child = cmd.spawn().unwrap();
        Pty {
            master,
            child,
            seen: Vec::new(),
            mark: 0,
        }
    }

    fn pump(&mut self, ms: i32) -> bool {
        use std::os::fd::AsRawFd;
        let mut p = libc::pollfd {
            fd: self.master.as_raw_fd(),
            events: libc::POLLIN,
            revents: 0,
        };
        // SAFETY: one valid pollfd.
        let n = unsafe { libc::poll(&mut p, 1, ms) };
        if n <= 0 {
            return false;
        }
        let mut buf = [0u8; 8192];
        match self.master.read(&mut buf) {
            Ok(0) | Err(_) => false,
            Ok(n) => {
                self.seen.extend_from_slice(&buf[..n]);
                true
            }
        }
    }

    /// Waits until `needle` shows up in what the terminal received after the last match.
    fn expect(&mut self, needle: &str) {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            let text = String::from_utf8_lossy(&self.seen[self.mark..]).into_owned();
            if let Some(i) = text.find(needle) {
                self.mark += i + needle.len();
                return;
            }
            if Instant::now() > deadline {
                panic!(
                    "did not see {needle:?}; the terminal got:\n{}",
                    String::from_utf8_lossy(&self.seen[self.mark..])
                );
            }
            self.pump(100);
        }
    }

    fn send(&mut self, keys: &str) {
        self.master.write_all(keys.as_bytes()).unwrap();
        self.master.flush().unwrap();
        // a lone Escape must reach the program as its own key
        thread::sleep(Duration::from_millis(120));
    }

    /// Reads what is left, then waits for the end. Returns the exit code.
    fn finish(&mut self) -> Option<i32> {
        let deadline = Instant::now() + Duration::from_secs(20);
        loop {
            while self.pump(20) {}
            if let Some(s) = self.child.try_wait().unwrap() {
                while self.pump(50) {}
                return s.code();
            }
            if Instant::now() > deadline {
                let _ = self.child.kill();
                panic!(
                    "the menu did not end; the terminal got:\n{}",
                    String::from_utf8_lossy(&self.seen[self.mark..])
                );
            }
        }
    }

    fn everything(&self) -> String {
        String::from_utf8_lossy(&self.seen).into_owned()
    }
}

fn menu(home: &Dir, bin: &Dir, wave: Option<&str>) -> Command {
    let mut c = Command::new(env!("CARGO_BIN_EXE_stonewave"));
    c.env_clear()
        .env(
            "PATH",
            // only the system's own folders after ours: no coding agent of whoever runs the tests is found by accident
            format!("{}:/usr/local/bin:/usr/bin:/bin", bin.0.display()),
        )
        .env("HOME", &home.0)
        .env("TERM", "xterm-256color")
        .env("NO_COLOR", "1")
        .env("DEMO_KEY", KEY);
    if let Some(w) = wave {
        c.env("STONEWAVE_WAVE_URL", w);
    }
    c
}

/// A menu with nothing on this machine yet and Claude Code installed.
fn rig() -> (Dir, Dir) {
    let home = Dir::new("home");
    let bin = Dir::new("bin");
    fake_claude(&bin.0);
    (home, bin)
}

// ------------------------------------------------------------------ the tests

#[test]
fn the_menu_opens_on_a_terminal_and_gives_it_back_on_q_and_on_ctrl_c() {
    if is_root() {
        eprintln!("skipped: the menu refuses to connect agents as root");
        return;
    }
    for quit in ["q", CTRL_C] {
        let (home, bin) = rig();
        let mut t = Pty::spawn(menu(&home, &bin, None), 40, 100);
        t.expect("Agent verbinden");
        t.send(DOWN);
        t.send(DOWN); // "Agent hinzufügen", "Agent verbinden", then Status
        t.send(ENTER);
        t.expect("Status");
        t.expect("Claude Code"); // the status screen lists the engines
        t.send(ESC);
        t.expect("Beenden");
        t.send(quit);
        assert_eq!(t.finish(), Some(0), "{quit:?}");
        let all = t.everything();
        assert!(all.contains("\x1b[?1049h"), "the full screen was taken");
        assert!(all.contains("\x1b[?1049l"), "and given back");
        assert!(all.contains("\x1b[?25h"), "with the cursor shown again");
    }
}

#[test]
fn plain_output_never_meets_a_menu() {
    let (home, bin) = rig();
    // not on a terminal: today's plain screen, one pass, nothing asked
    let out = menu(&home, &bin, None)
        .arg("agents")
        .stdin(Stdio::null())
        .output()
        .unwrap();
    let text = String::from_utf8_lossy(&out.stdout);
    assert!(
        out.status.success() && text.contains("coding agents on this computer"),
        "{text}"
    );
    assert!(!text.contains("\x1b[?1049h"));
    // on a terminal with --plain: the same
    let mut c = menu(&home, &bin, None);
    c.arg("--plain");
    let mut t = Pty::spawn(c, 40, 100);
    assert_eq!(t.finish(), Some(0));
    let all = t.everything();
    assert!(
        all.contains("coding agents on this computer") && !all.contains("\x1b[?1049h"),
        "{all}"
    );
    // a terminal that cannot do it: the plain menu answers q
    let mut c = menu(&home, &bin, None);
    c.env("TERM", "dumb");
    let mut t = Pty::spawn(c, 40, 100);
    t.expect("coding agents on this computer");
    t.send("q\n");
    assert_eq!(t.finish(), Some(0));
    assert!(!t.everything().contains("\x1b[?1049h"));
}

#[test]
fn a_small_terminal_gets_the_plain_menu_instead_of_a_broken_screen() {
    let (home, bin) = rig();
    let mut t = Pty::spawn(menu(&home, &bin, None), 10, 40);
    t.expect("coding agents on this computer");
    t.send("q\n");
    assert_eq!(t.finish(), Some(0));
    assert!(!t.everything().contains("\x1b[?1049h"));
}

#[test]
fn the_whole_wizard_for_an_agent_whose_key_lives_in_the_environment() {
    if is_root() {
        eprintln!("skipped: the menu refuses to connect agents as root");
        return;
    }
    let (home, bin) = rig();
    let (wave, seen) = serve();
    let mut t = Pty::spawn(menu(&home, &bin, Some(&wave)), 40, 110);
    t.expect("Agent verbinden");
    t.send(DOWN); // past "Agent hinzufügen" to "Agent verbinden"
    t.send(ENTER);
    t.expect("Wie kommt der Agent zu Wave?");
    t.send(DOWN);
    t.send(ENTER); // Mehr …
    t.expect("Auf einem Server mit Secret-Store");
    t.send(ENTER);
    t.expect("Welches Programm soll der Agent sein?");
    t.expect("gefunden (2.1.321");
    t.send(ENTER);
    t.expect("Wie heißt der Agent?");
    t.send(CTRL_U);
    t.send("demo");
    t.send(ENTER);
    t.expect("Wo liegt Wave?");
    t.send(ENTER); // the address from STONEWAVE_WAVE_URL
    t.expect("Woher kommt der Schlüssel?");
    t.send(CTRL_U);
    t.send("DEMO_KEY");
    t.expect("ist in dieser Shell gesetzt");
    t.send(ENTER);
    t.expect("Räumen soll der Agent geweckt werden");
    t.expect("Team room");
    t.send(" ");
    t.send(ENTER);
    t.expect("Der Arbeitsordner des Agenten");
    t.send(ENTER);
    t.expect("Die Grenzen jedes Weckrufs");
    t.send(ENTER);
    t.expect("Vorschau: was ein Weckruf starten würde");
    t.expect("environment variable DEMO_KEY of this process");
    t.send(ENTER);
    t.expect("Test: einmal in den Posteingang schauen");
    // the real test run: the terminal goes to the station for a moment and comes back
    t.send(ENTER);
    t.expect("Enter drücken, um zum Menü zurückzukehren");
    t.send(ENTER);
    t.expect("Dauerhaft starten?");
    t.expect("Start-Befehl");
    t.send(ENTER);
    t.expect("demo ist eingerichtet");
    t.send(ENTER);
    t.expect("Agenten auf diesem Computer");
    t.send("q");
    assert_eq!(t.finish(), Some(0));

    // what it left: the profile, no key anywhere, and Wave was asked with the key from the environment
    let profile = home.0.join(".config/wave/demo.profile.json");
    let text = std::fs::read_to_string(&profile).expect("the profile was saved");
    assert!(
        text.contains("\"key_env\": \"DEMO_KEY\"")
            && text.contains("room-1")
            && !text.contains("room-2"),
        "{text}"
    );
    assert!(!tree_contains(&home.0, KEY), "the key is written nowhere");
    assert!(!t.everything().contains(KEY), "and never shown");
    let s = seen.lock().unwrap();
    let tools: Vec<&str> = s.iter().map(|(t, _)| t.as_str()).collect();
    assert!(
        tools.contains(&"rooms") && tools.contains(&"inbox"),
        "{tools:?}"
    );
    assert!(
        s.iter().all(|(_, a)| *a == format!("Bearer {KEY}")),
        "{s:?}"
    );
}

fn tree_contains(dir: &Path, needle: &str) -> bool {
    let Ok(rd) = std::fs::read_dir(dir) else {
        return false;
    };
    rd.flatten().any(|e| {
        let p = e.path();
        if p.is_dir() {
            tree_contains(&p, needle)
        } else {
            std::fs::read(&p).is_ok_and(|b| String::from_utf8_lossy(&b).contains(needle))
        }
    })
}

#[test]
fn the_main_screen_lists_an_agent_the_wizard_made_and_its_page_offers_what_fits() {
    if is_root() {
        return;
    }
    let (home, bin) = rig();
    // an agent that exists only as a profile, as the wizard leaves it
    let cfg = home.0.join(".config/wave");
    std::fs::create_dir_all(&cfg).unwrap();
    std::fs::write(
        cfg.join("demo.profile.json"),
        r#"{"version":1,"agent":"demo","engine":"claude-code","wave_url":"https://wave.example","key_env":"DEMO_KEY","launcher":[],"workdir":"/tmp/demo-work","rooms":["room-1"],"room_names":["Team room"]}"#,
    )
    .unwrap();
    let mut t = Pty::spawn(menu(&home, &bin, None), 40, 110);
    t.expect("demo");
    t.expect("Umgebung DEMO_KEY");
    t.send(ENTER);
    t.expect("Status der Station");
    t.expect("Station jetzt hier starten");
    t.expect("Dauerhaft starten");
    t.send(UP);
    t.send(ESC);
    t.send("q");
    assert_eq!(t.finish(), Some(0));
}

#[test]
fn a_term_or_hup_signal_gives_the_terminal_back() {
    if is_root() {
        return;
    }
    for sig in [libc::SIGTERM, libc::SIGHUP] {
        let (home, bin) = rig();
        let mut t = Pty::spawn(menu(&home, &bin, None), 40, 100);
        t.expect("Agent verbinden");
        // SAFETY: a plain kill of the child this test started.
        unsafe {
            libc::kill(t.child.id() as i32, sig);
        }
        assert_eq!(t.finish(), Some(0), "signal {sig}");
        let all = t.everything();
        assert!(
            all.contains("\x1b[?1049l") && all.contains("\x1b[?25h"),
            "signal {sig}: the screen was not given back"
        );
    }
}

#[test]
fn the_wave_address_is_offered_when_nothing_is_known_and_the_first_agent_can_go_on() {
    if is_root() {
        return;
    }
    let (home, bin) = rig();
    let mut t = Pty::spawn(menu(&home, &bin, None), 40, 110);
    t.expect("Agent verbinden");
    t.send(DOWN); // past "Agent hinzufügen" to "Agent verbinden"
    t.send(ENTER);
    t.expect("Wie kommt der Agent zu Wave?");
    t.send(DOWN);
    t.send(ENTER); // Mehr …
    t.expect("Auf einem Server mit Secret-Store");
    t.send(ENTER);
    t.expect("Welches Programm soll der Agent sein?");
    t.send(ENTER);
    t.expect("Wie heißt der Agent?");
    t.send(ENTER);
    t.expect("Wo liegt Wave?");
    t.expect("https://wave-dev.preview.stonewavetech.com");
    t.send(CTRL_C);
    assert_eq!(t.finish(), Some(0));
}

// ------------------------------------------------------------------ the simple way, with a small account and a small Wave on one address

type Bodies = Arc<Mutex<Vec<(String, String)>>>;

fn serve_account() -> (String, Bodies) {
    let listener = TcpListener::bind("127.0.0.1:0").unwrap();
    let url = format!("http://127.0.0.1:{}", listener.local_addr().unwrap().port());
    let bodies: Bodies = Arc::new(Mutex::new(Vec::new()));
    let (u2, b2) = (url.clone(), bodies.clone());
    let polls = Arc::new(Mutex::new(0u32));
    thread::spawn(move || {
        for stream in listener.incoming().flatten() {
            let (u, b, p) = (u2.clone(), b2.clone(), polls.clone());
            thread::spawn(move || handle_account(stream, &u, b, p));
        }
    });
    (url, bodies)
}

fn handle_account(mut stream: TcpStream, url: &str, bodies: Bodies, polls: Arc<Mutex<u32>>) {
    let mut reader = BufReader::new(stream.try_clone().unwrap());
    let (mut len, mut line, mut first) = (0usize, String::new(), String::new());
    loop {
        line.clear();
        if reader.read_line(&mut line).unwrap_or(0) == 0 || line == "\r\n" {
            break;
        }
        if first.is_empty() {
            first = line.clone();
        }
        if let Some(v) = line.to_lowercase().strip_prefix("content-length:") {
            len = v.trim().parse().unwrap_or(0);
        }
    }
    let mut body = vec![0u8; len];
    reader.read_exact(&mut body).ok();
    let body = String::from_utf8_lossy(&body).into_owned();
    let path = first.split_whitespace().nth(1).unwrap_or("").to_owned();
    bodies.lock().unwrap().push((path.clone(), body.clone()));
    let (status, out) = match path.as_str() {
        "/v1/device/code" => (
            200,
            format!(
                r#"{{"device_code":"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG","user_code":"WAVE-7K4Q-X2MP","verification_uri":"{url}/link","expires_in":600,"interval":1,"words":["amber","river","cedar","fox"]}}"#
            ),
        ),
        "/v1/device/token" => {
            let mut n = polls.lock().unwrap();
            *n += 1;
            if *n < 2 {
                (400, r#"{"error":"authorization_pending"}"#.to_owned())
            } else {
                (
                    200,
                    format!(
                        r#"{{"access_token":"aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbb.cccccccccccccc","token_type":"Bearer","expires_in":900,"scope":"agent:connect","company":"acme","company_name":"Acme GmbH","team":"ops","team_name":"Ops","agent":"ops-bot","agent_name":"Ops Bot","engine":"claude-code","wave_url":"{url}"}}"#
                    ),
                )
            }
        }
        "/v1/agent-connect/mint" => (
            201,
            format!(
                r#"{{"key":"{KEY}","agent":{{"id":"11111111-2222-4333-8444-555555555555","key":"ops-bot","name":"Ops Bot"}},"team":{{"key":"ops","name":"Ops"}},"company":"acme","expires_at":"2099-02-20T10:00:00Z"}}"#
            ),
        ),
        _ => {
            // Wave's MCP: the rooms
            let b: serde_json::Value = serde_json::from_str(&body).unwrap_or_default();
            let data = match b["params"]["name"].as_str().unwrap_or("") {
                "rooms" => {
                    serde_json::json!({ "items": [{ "id": "room-1", "name": "Team room" }, { "id": "room-2", "name": "Sales" }] })
                }
                "inbox" => {
                    serde_json::json!({ "items": [], "next_seq": 1, "tasks": [], "stuck": [] })
                }
                _ => serde_json::json!({ "ok": true }),
            };
            (
                200,
                serde_json::json!({ "jsonrpc": "2.0", "id": 1, "result": { "content": [{ "type": "text", "text": data.to_string() }] } }).to_string(),
            )
        }
    };
    let resp = format!(
        "HTTP/1.1 {status} X\r\nContent-Type: application/json\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{out}",
        out.len()
    );
    let _ = stream.write_all(resp.as_bytes());
}

#[test]
fn the_simple_way_opens_the_browser_and_connects_an_agent_without_asking_for_a_name() {
    if is_root() {
        return;
    }
    let (home, bin) = rig();
    let (url, bodies) = serve_account();
    let mut c = menu(&home, &bin, None);
    c.env("DISPLAY", ":0").env("STONEWAVE_ACCOUNT_URL", &url);
    let mut t = Pty::spawn(c, 40, 110);
    t.expect("Agent verbinden");
    t.send(DOWN); // past "Agent hinzufügen" to "Agent verbinden"
    t.send(ENTER);
    t.expect("Wie kommt der Agent zu Wave?");
    t.expect("Neuen Agenten anmelden");
    t.send(ENTER);
    t.expect("Agent mit Wave verbinden");
    t.expect("Claude Code (2.1.321");
    t.send(ENTER);
    // the real sign-in runs in the terminal: German, the code and the four words stay as the safeguard, the browser is opened
    t.expect("Im Browser anmelden");
    t.expect("WAVE-7K4Q-X2MP");
    t.expect("amber · river · cedar · fox");
    t.expect("Dein Browser ist offen. Gib dort den Code ein.");
    t.expect("Key saved");
    t.expect("Enter drücken, um zum Menü zurückzukehren");
    t.send(ENTER);
    // the name is the one Wave made, read back; "Später" is the default
    t.expect("Verbunden: ops-bot");
    t.send(ENTER);
    t.expect("ops-bot ist verbunden");
    t.send(ENTER);
    t.expect("Agenten auf diesem Computer");
    t.expect("ops-bot");
    t.send("q");
    assert_eq!(t.finish(), Some(0));

    assert_eq!(
        std::fs::read_to_string(bin.0.join("opened.txt")).unwrap(),
        format!("{url}/link"),
        "the browser was sent to the account's own page"
    );
    let b = bodies.lock().unwrap();
    let start = b.iter().find(|(p, _)| p == "/v1/device/code").unwrap();
    assert!(start.1.contains(r#""engine":"claude-code""#), "{}", start.1);
    assert!(
        !start.1.contains("agent_name"),
        "no name was suggested or asked: {}",
        start.1
    );
    let cfg = home.0.join(".config/wave");
    assert!(cfg.join("ops-bot.key").exists() && cfg.join("ops-bot.json").exists());
    assert!(!t.everything().contains(KEY), "the key is never shown");
}
