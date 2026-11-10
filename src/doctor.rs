//! `stonewave doctor`: can this server be connected?
//!
//! Read only. It reads files under `/etc`, `/proc` and `/run`, runs a few
//! commands that only report (`uname -m`, `sshd -V`, `timedatectl show`,
//! `df`) and opens one TCP connection to the Stonewave account service on
//! port 443 (no data is sent). It changes nothing.
//!
//! Everything it reads goes through [`Probe`], so the checks can be tested
//! against made-up servers.

use std::io::Read;
use std::net::{SocketAddr, TcpStream, ToSocketAddrs};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::{Duration, Instant};

use crate::style::{ColorMode, pal};
use crate::term::{Result, Term, json_escape};
use crate::ui::{DETAIL, INDENT, pulse_dot};

/// Host the daemon will talk to; doctor only checks that port 443 answers.
pub const OUTBOUND_HOST: &str = "account.stonewavetech.com";
const COMMAND_TIMEOUT: Duration = Duration::from_secs(3);
const NETWORK_TIMEOUT: Duration = Duration::from_secs(3);
/// OpenSSH from this version has everything the agent SSH needs
/// (certificates, `ExposeAuthInfo`, principals files, KRLs).
const MIN_OPENSSH: (u32, u32) = (8, 2);
const MIN_FREE_MB: u64 = 500;
const SSHD: &str = "/usr/sbin/sshd";

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Level {
    Ok,
    /// Works, but worth a look.
    Warn,
    /// Setup would not work.
    Fail,
    /// Just so you know.
    Info,
}

impl Level {
    fn as_str(self) -> &'static str {
        match self {
            Level::Ok => "ok",
            Level::Warn => "warn",
            Level::Fail => "fail",
            Level::Info => "info",
        }
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Check {
    pub id: &'static str,
    pub label: &'static str,
    pub level: Level,
    pub detail: String,
}

/// What doctor reads from the machine.
pub trait Probe: Sync {
    fn read(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    /// Output (stdout, else stderr) of a command, or None if it could not run in time.
    fn run(&self, cmd: &str, args: &[&str]) -> Option<String>;
    /// Opens and closes a TCP connection; returns how long it took.
    fn connect(&self, host: &str, port: u16) -> std::result::Result<Duration, String>;
    fn env(&self, name: &str) -> Option<String>;
}

/// The real machine.
pub struct System;

/// The real machine, for threads that may outlive the screen (Ctrl-C).
pub static SYSTEM: System = System;

impl Probe for System {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
        let mut child = Command::new(cmd)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .ok()?;
        let started = Instant::now();
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if started.elapsed() < COMMAND_TIMEOUT => {
                    std::thread::sleep(Duration::from_millis(20));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    return None;
                }
            }
        }
        // The commands print a few lines at most, so the pipes cannot fill up
        // while we wait above.
        let mut out = String::new();
        child.stdout.take()?.read_to_string(&mut out).ok()?;
        if out.trim().is_empty() {
            child.stderr.take()?.read_to_string(&mut out).ok()?;
        }
        Some(out.trim().to_owned())
    }

    fn connect(&self, host: &str, port: u16) -> std::result::Result<Duration, String> {
        let started = Instant::now();
        // The system resolver has no timeout of its own; ask it on a side thread.
        let (tx, rx) = mpsc::channel::<Vec<SocketAddr>>();
        let name = host.to_owned();
        std::thread::spawn(move || {
            let addrs = (name.as_str(), port)
                .to_socket_addrs()
                .map(Iterator::collect)
                .unwrap_or_default();
            let _ = tx.send(addrs);
        });
        let addrs = match rx.recv_timeout(NETWORK_TIMEOUT) {
            Ok(a) if !a.is_empty() => a,
            Ok(_) => return Err("name not resolved".into()),
            Err(_) => return Err("name lookup timed out".into()),
        };
        let mut last = "no address".to_owned();
        for addr in addrs {
            let left = NETWORK_TIMEOUT
                .saturating_sub(started.elapsed())
                .max(Duration::from_millis(500));
            match TcpStream::connect_timeout(&addr, left) {
                Ok(_) => return Ok(started.elapsed()),
                Err(e) => last = e.kind().to_string(),
            }
        }
        Err(last)
    }

    fn env(&self, name: &str) -> Option<String> {
        std::env::var(name).ok().filter(|v| !v.is_empty())
    }
}

// ---------------------------------------------------------------- parsers

/// A field of an os-release or simple `KEY=value` file, quotes removed.
pub fn os_release_field(text: &str, key: &str) -> Option<String> {
    text.lines()
        .find_map(|l| l.strip_prefix(key)?.strip_prefix('='))
        .map(|v| v.trim().trim_matches('"').to_owned())
        .filter(|v| !v.is_empty())
}

/// `OpenSSH_9.6p1 Ubuntu-3ubuntu13, OpenSSL …` → (9, 6). Also finds it inside
/// the usage text that older sshd versions print for `-V`.
pub fn parse_openssh(text: &str) -> Option<(u32, u32)> {
    let rest = &text[text.find("OpenSSH_")? + "OpenSSH_".len()..];
    let mut nums = rest
        .split(|c: char| !c.is_ascii_digit())
        .filter(|s| !s.is_empty());
    let major = nums.next()?.parse().ok()?;
    let minor = nums.next()?.parse().ok()?;
    Some((major, minor))
}

/// Effective user id from `/proc/self/status` (`Uid: real effective saved fs`).
pub fn parse_euid(status: &str) -> Option<u32> {
    let line = status.lines().find(|l| l.starts_with("Uid:"))?;
    line.split_whitespace().nth(2)?.parse().ok()
}

/// Free megabytes from `df -Pk <path>` output.
pub fn parse_df_free_mb(text: &str) -> Option<u64> {
    let line = text.lines().nth(1)?;
    let kb: u64 = line.split_whitespace().nth(3)?.parse().ok()?;
    Some(kb / 1024)
}

// ---------------------------------------------------------------- checks

/// What one check found: its level and a short detail.
type Finding = (Level, String);

fn found(level: Level, detail: impl Into<String>) -> Finding {
    (level, detail.into())
}

const SUPPORTED: [(&str, &str); 5] = [
    ("ubuntu", "22.04"),
    ("ubuntu", "24.04"),
    ("ubuntu", "26.04"),
    ("debian", "12"),
    ("debian", "13"),
];

fn os(p: &dyn Probe) -> Finding {
    let Some(text) = p.read("/etc/os-release") else {
        return found(Level::Fail, "no /etc/os-release: not a Linux server");
    };
    let id = os_release_field(&text, "ID").unwrap_or_default();
    let version = os_release_field(&text, "VERSION_ID").unwrap_or_default();
    let name = os_release_field(&text, "PRETTY_NAME").unwrap_or_else(|| format!("{id} {version}"));
    if SUPPORTED.iter().any(|&(i, v)| i == id && v == version) {
        found(Level::Ok, name)
    } else {
        found(
            Level::Warn,
            format!("{name} · not tested yet (Ubuntu 22.04–26.04, Debian 12–13)"),
        )
    }
}

fn arch(p: &dyn Probe) -> Finding {
    match p.run("uname", &["-m"]).as_deref() {
        Some(a @ ("x86_64" | "aarch64")) => found(Level::Ok, a),
        Some(other) => found(Level::Fail, format!("{other} · only x86_64 and aarch64")),
        None => found(Level::Warn, "unknown (uname failed)"),
    }
}

fn root(p: &dyn Probe) -> Finding {
    match p.read("/proc/self/status").as_deref().and_then(parse_euid) {
        Some(0) => found(Level::Ok, "yes · needed once, for setup"),
        Some(_) => found(Level::Info, "no · doctor works, setup needs sudo once"),
        None => found(Level::Info, "unknown"),
    }
}

fn systemd(p: &dyn Probe) -> Finding {
    if p.exists("/run/systemd/system") {
        found(Level::Ok, "running")
    } else {
        found(Level::Fail, "not running · the daemon needs systemd")
    }
}

/// The server's sshd is what checks certificates, so its version counts, not the client's.
fn openssh(p: &dyn Probe) -> Finding {
    if !p.exists(SSHD) {
        return found(Level::Warn, "no sshd · setup would install openssh-server");
    }
    match p.run(SSHD, &["-V"]).as_deref().and_then(parse_openssh) {
        Some(v) if v < MIN_OPENSSH => found(
            Level::Fail,
            format!(
                "sshd {}.{} · needs {}.{} or newer",
                v.0, v.1, MIN_OPENSSH.0, MIN_OPENSSH.1
            ),
        ),
        Some(v) => found(
            Level::Ok,
            format!("sshd {}.{} · certificates supported", v.0, v.1),
        ),
        None => found(Level::Warn, "sshd found, version unknown"),
    }
}

fn clock(p: &dyn Probe) -> Finding {
    match p
        .run("timedatectl", &["show", "-p", "NTPSynchronized", "--value"])
        .as_deref()
    {
        Some("yes") => found(Level::Ok, "synchronized"),
        Some("no") => found(
            Level::Warn,
            "not synchronized · certificates depend on the time",
        ),
        _ => found(Level::Warn, "unknown (timedatectl not available)"),
    }
}

/// `https://host[:port]` as host and port.
fn host_port(base: &str) -> (String, u16) {
    let rest = base.split_once("://").map_or(base, |x| x.1);
    match rest.rsplit_once(':') {
        Some((h, port)) if port.parse::<u16>().is_ok() => {
            (h.to_owned(), port.parse().unwrap_or(443))
        }
        _ => (rest.to_owned(), 443),
    }
}

/// Where the daemon of this server connects: the relay it was set up with, else the default one.
fn relay_endpoint(p: &dyn Probe) -> (String, u16) {
    let path = format!("{}/{}", crate::setup::VAR_LIB, crate::relay::IDENTITY_FILE);
    let base = p
        .read(&path)
        .and_then(|t| crate::relay::Identity::parse(&t))
        .map_or_else(|| crate::connect::DEFAULT_RELAY.to_owned(), |i| i.relay);
    host_port(&base)
}

fn outbound(p: &dyn Probe) -> Finding {
    match p.connect(OUTBOUND_HOST, 443) {
        Ok(d) => {
            // The sign-in goes to the account, the daemon lives on the relay: both must be reachable. The relay is
            // only a warning, because it may not be switched on yet.
            let (host, port) = relay_endpoint(p);
            match p.connect(&host, port) {
                Ok(r) => found(
                    Level::Ok,
                    format!(
                        "account {} ms · relay {} ms (port {port})",
                        d.as_millis(),
                        r.as_millis()
                    ),
                ),
                Err(why) => found(
                    Level::Warn,
                    format!(
                        "account reachable ({} ms) · relay {host}:{port} not reachable ({why}): the daemon needs it",
                        d.as_millis()
                    ),
                ),
            }
        }
        Err(why) => {
            let proxy = ["https_proxy", "HTTPS_PROXY", "all_proxy", "ALL_PROXY"]
                .iter()
                .any(|v| p.env(v).is_some());
            if proxy {
                // Direct traffic is blocked but a proxy is configured: setup can
                // still work once the daemon speaks through proxies.
                found(
                    Level::Warn,
                    format!(
                        "no direct connection ({why}) · a proxy is set; the daemon must use it"
                    ),
                )
            } else {
                found(
                    Level::Fail,
                    format!(
                        "{OUTBOUND_HOST}:443 not reachable ({why}) · the daemon only connects outbound"
                    ),
                )
            }
        }
    }
}

fn disk(p: &dyn Probe) -> Finding {
    match p
        .run("df", &["-Pk", "/var"])
        .as_deref()
        .and_then(parse_df_free_mb)
    {
        Some(mb) if mb >= MIN_FREE_MB => found(Level::Ok, format!("{mb} MB free on /var")),
        Some(mb) => found(Level::Warn, format!("only {mb} MB free on /var")),
        None => found(Level::Warn, "unknown (df failed)"),
    }
}

/// ufw's service is active on Ubuntu even when the firewall is off, so the
/// switch in ufw.conf decides (readable without root).
fn firewall(p: &dyn Probe) -> Finding {
    match p
        .read("/etc/ufw/ufw.conf")
        .as_deref()
        .and_then(|t| os_release_field(t, "ENABLED"))
    {
        Some(v) if v.eq_ignore_ascii_case("yes") => {
            found(Level::Info, "ufw on · setup keeps it as it is")
        }
        Some(_) => found(
            Level::Info,
            "ufw installed but off · setup would propose basics, you confirm",
        ),
        None => found(
            Level::Info,
            "no ufw · setup would propose basics, you confirm",
        ),
    }
}

fn existing(p: &dyn Probe) -> Finding {
    if p.exists("/etc/stonewave") {
        found(Level::Info, "already set up · setup checks and keeps it")
    } else {
        found(Level::Info, "not set up yet")
    }
}

type CheckFn = fn(&dyn Probe) -> Finding;

/// All checks, in the order they are shown: id, label, check.
pub const CHECKS: [(&str, &str, CheckFn); 10] = [
    ("os", "System", os),
    ("arch", "CPU", arch),
    ("root", "Root", root),
    ("systemd", "systemd", systemd),
    ("openssh", "OpenSSH", openssh),
    ("clock", "Clock", clock),
    ("outbound", "Outbound", outbound),
    ("disk", "Disk", disk),
    ("firewall", "Firewall", firewall),
    ("existing", "Stonewave", existing),
];

fn to_check(id: &'static str, label: &'static str, (level, detail): Finding) -> Check {
    Check {
        id,
        label,
        level,
        detail,
    }
}

pub fn run_all(p: &dyn Probe) -> Vec<Check> {
    CHECKS
        .iter()
        .map(|&(id, label, f)| to_check(id, label, f(p)))
        .collect()
}

/// Whether setup could go ahead: no check failed.
pub fn passed(checks: &[Check]) -> bool {
    checks.iter().all(|c| c.level != Level::Fail)
}

// ---------------------------------------------------------------- output

fn icon(m: ColorMode, level: Level) -> String {
    match level {
        Level::Ok => m.fg_bold(pal::GREEN, "✓"),
        Level::Warn => m.fg_bold(pal::AMBER, "◆"),
        Level::Fail => m.fg_bold(pal::RED, "✗"),
        Level::Info => m.fg(pal::GREY, "·"),
    }
}

fn pad(label: &str, label_w: usize) -> String {
    " ".repeat(label_w.saturating_sub(label.chars().count()) + 2)
}

fn row(m: ColorMode, c: &Check, label_w: usize) -> String {
    let detail = match c.level {
        Level::Fail => m.fg(pal::RED, &c.detail),
        Level::Warn => m.fg(pal::AMBER, &c.detail),
        _ => m.fg(pal::GREY, &c.detail),
    };
    format!(
        "{DETAIL}{} {}{}{detail}",
        icon(m, c.level),
        c.label,
        pad(c.label, label_w)
    )
}

fn running_row(m: ColorMode, label: &str, label_w: usize, t: f32) -> String {
    format!(
        "{DETAIL}{} {}{}{}",
        pulse_dot(m, t),
        m.bold(label),
        pad(label, label_w),
        m.fg(pal::CYAN, "checking")
    )
}

fn waiting_row(m: ColorMode, label: &str) -> String {
    format!("{DETAIL}{} {}", m.fg(pal::DIM, "○"), m.fg(pal::DIM, label))
}

/// Machine-readable result for agents: one JSON object.
pub fn to_json(host: &str, checks: &[Check]) -> String {
    let items: Vec<String> = checks
        .iter()
        .map(|c| {
            format!(
                r#"{{"id":"{}","level":"{}","detail":"{}"}}"#,
                c.id,
                c.level.as_str(),
                json_escape(&c.detail)
            )
        })
        .collect();
    format!(
        r#"{{"host":"{}","passed":{},"checks":[{}]}}"#,
        json_escape(host),
        passed(checks),
        items.join(",")
    )
}

/// Runs the checks one after another and shows them as they finish.
///
/// Each check runs on its own detached thread, so the screen keeps moving
/// while one waits and Ctrl-C stops at once (a check still running is simply
/// left behind when the program ends).
pub fn show(term: &mut Term, host: &str, p: &'static dyn Probe) -> Result<Vec<Check>> {
    let m = term.mode();
    let label_w = CHECKS
        .iter()
        .map(|(_, l, _)| l.chars().count())
        .max()
        .unwrap_or(0);
    term.blank();
    term.line(&format!(
        "{INDENT}{} {}",
        m.fg(pal::CYAN, "●"),
        m.bold(&format!("Checking {host}"))
    ));
    term.line(&format!(
        "{DETAIL}{}",
        m.fg(pal::GREY, "Read only: nothing on this machine is changed.")
    ));
    term.blank();

    let mut done: Vec<Check> = Vec::with_capacity(CHECKS.len());
    if !term.caps.live {
        for c in run_all(p) {
            term.line(&row(m, &c, label_w));
            done.push(c);
        }
        term.flush()?;
        return Ok(done);
    }

    term.hide_cursor();
    let mark = term.mark();
    let t0 = Instant::now();
    for (i, &(id, label, f)) in CHECKS.iter().enumerate() {
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(f(p));
        });
        let finding = loop {
            let t = if term.caps.animate {
                t0.elapsed().as_secs_f32()
            } else {
                0.0
            };
            let mut lines: Vec<String> = done.iter().map(|c| row(m, c, label_w)).collect();
            lines.push(running_row(m, label, label_w, t));
            lines.extend(CHECKS[i + 1..].iter().map(|(_, l, _)| waiting_row(m, l)));
            term.repaint(mark, &lines);
            match rx.try_recv() {
                Ok(finding) => break finding,
                // A check that panicked counts as unknown instead of taking the screen down.
                Err(mpsc::TryRecvError::Disconnected) => break found(Level::Warn, "check crashed"),
                Err(mpsc::TryRecvError::Empty) => {}
            }
            term.wait(Duration::from_millis(40))?;
        };
        done.push(to_check(id, label, finding));
    }
    let lines: Vec<String> = done.iter().map(|c| row(m, c, label_w)).collect();
    term.repaint(mark, &lines);
    term.flush()?;
    Ok(done)
}

/// The closing lines: what the result means.
pub fn verdict(term: &mut Term, checks: &[Check]) -> Result<()> {
    let m = term.mode();
    let fails = checks.iter().filter(|c| c.level == Level::Fail).count();
    let warns = checks.iter().filter(|c| c.level == Level::Warn).count();
    term.blank();
    if fails == 0 {
        let extra = if warns > 0 {
            format!(" · {warns} to look at")
        } else {
            String::new()
        };
        term.line(&format!(
            "{INDENT}{} {}{}",
            m.fg_bold(pal::GREEN, "✓"),
            m.bold("Ready to connect"),
            m.fg(pal::GREY, &extra)
        ));
    } else {
        term.line(&format!(
            "{INDENT}{} {}",
            m.fg_bold(pal::RED, "✗"),
            m.bold(&format!(
                "Not ready · {fails} check{} failed",
                if fails == 1 { "" } else { "s" }
            ))
        ));
    }
    term.flush()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    /// A made-up server.
    #[derive(Default)]
    struct Fake {
        files: HashMap<&'static str, &'static str>,
        cmds: HashMap<&'static str, &'static str>,
        env: HashMap<&'static str, &'static str>,
        reachable: bool,
    }

    impl Probe for Fake {
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).map(|s| (*s).to_owned())
        }
        fn exists(&self, path: &str) -> bool {
            self.files.contains_key(path)
        }
        fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
            let key = format!("{cmd} {}", args.join(" "));
            self.cmds.get(key.as_str()).map(|s| (*s).to_owned())
        }
        fn connect(&self, _: &str, _: u16) -> std::result::Result<Duration, String> {
            if self.reachable {
                Ok(Duration::from_millis(23))
            } else {
                Err("timed out".into())
            }
        }
        fn env(&self, name: &str) -> Option<String> {
            self.env.get(name).map(|s| (*s).to_owned())
        }
    }

    fn good_server() -> Fake {
        Fake {
            files: HashMap::from([
                (
                    "/etc/os-release",
                    "PRETTY_NAME=\"Ubuntu 24.04.1 LTS\"\nID=ubuntu\nVERSION_ID=\"24.04\"\n",
                ),
                ("/proc/self/status", "Name:\tstonewave\nUid:\t0\t0\t0\t0\n"),
                ("/run/systemd/system", ""),
                ("/usr/sbin/sshd", ""),
                (
                    "/etc/ufw/ufw.conf",
                    "# comment\nENABLED=yes\nLOGLEVEL=low\n",
                ),
            ]),
            cmds: HashMap::from([
                ("uname -m", "x86_64"),
                (
                    "/usr/sbin/sshd -V",
                    "OpenSSH_9.6p1 Ubuntu-3ubuntu13.5, OpenSSL 3.0.13 30 Jan 2024",
                ),
                ("timedatectl show -p NTPSynchronized --value", "yes"),
                (
                    "df -Pk /var",
                    "Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 40000000 10000000 30000000 25% /\n",
                ),
            ]),
            env: HashMap::new(),
            reachable: true,
        }
    }

    fn get<'a>(checks: &'a [Check], id: &str) -> &'a Check {
        checks.iter().find(|c| c.id == id).unwrap()
    }

    fn level(checks: &[Check], id: &str) -> Level {
        get(checks, id).level
    }

    #[test]
    fn a_good_server_passes() {
        let checks = run_all(&good_server());
        assert!(passed(&checks), "{checks:#?}");
        for id in [
            "os", "arch", "root", "systemd", "openssh", "clock", "outbound", "disk",
        ] {
            assert_eq!(level(&checks, id), Level::Ok, "{id}");
        }
        assert!(get(&checks, "firewall").detail.starts_with("ufw on"));
    }

    #[test]
    fn failures_are_named() {
        let mut s = good_server();
        s.files.remove("/run/systemd/system");
        s.reachable = false;
        // Old sshd: -V is unknown, the version is in the usage text.
        s.cmds.insert(
            "/usr/sbin/sshd -V",
            "unknown option -- V\nOpenSSH_7.4p1, OpenSSL 1.0.2k-fips\nusage: sshd [-46DdeiqTt]",
        );
        let checks = run_all(&s);
        assert!(!passed(&checks));
        assert_eq!(level(&checks, "systemd"), Level::Fail);
        assert_eq!(level(&checks, "outbound"), Level::Fail);
        assert_eq!(level(&checks, "openssh"), Level::Fail);
    }

    #[test]
    fn soft_problems_only_warn() {
        let mut s = good_server();
        s.files.insert(
            "/etc/os-release",
            "ID=fedora\nVERSION_ID=41\nPRETTY_NAME=\"Fedora 41\"\n",
        );
        s.files
            .insert("/proc/self/status", "Uid:\t1000\t1000\t1000\t1000\n");
        s.files.remove("/usr/sbin/sshd");
        s.cmds
            .insert("timedatectl show -p NTPSynchronized --value", "no");
        s.cmds.remove("df -Pk /var");
        let checks = run_all(&s);
        assert!(passed(&checks), "{checks:#?}");
        assert_eq!(level(&checks, "os"), Level::Warn);
        assert_eq!(level(&checks, "root"), Level::Info);
        assert_eq!(level(&checks, "openssh"), Level::Warn);
        assert_eq!(level(&checks, "clock"), Level::Warn);
        assert_eq!(level(&checks, "disk"), Level::Warn);
    }

    #[test]
    fn ufw_service_alone_does_not_mean_on() {
        let mut s = good_server();
        s.files.insert("/etc/ufw/ufw.conf", "ENABLED=no\n");
        assert!(
            get(&run_all(&s), "firewall")
                .detail
                .starts_with("ufw installed but off")
        );
        s.files.remove("/etc/ufw/ufw.conf");
        assert!(get(&run_all(&s), "firewall").detail.starts_with("no ufw"));
    }

    #[test]
    fn a_proxy_turns_blocked_outbound_into_a_warning() {
        let mut s = good_server();
        s.reachable = false;
        s.env.insert("https_proxy", "http://proxy.example:3128");
        let checks = run_all(&s);
        assert_eq!(level(&checks, "outbound"), Level::Warn);
        assert!(passed(&checks));
    }

    #[test]
    fn not_linux_fails_cleanly() {
        let checks = run_all(&Fake::default());
        assert!(!passed(&checks));
        assert_eq!(level(&checks, "os"), Level::Fail);
        assert_eq!(checks.len(), CHECKS.len());
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_openssh("OpenSSH_10.2p1 Ubuntu-2ubuntu3.8, OpenSSL 3.5.5"),
            Some((10, 2))
        );
        assert_eq!(parse_openssh("OpenSSH_8.2p1"), Some((8, 2)));
        assert_eq!(parse_openssh("ssh: command not found"), None);
        assert_eq!(parse_euid("Uid:\t1000\t0\t0\t0\n"), Some(0));
        assert_eq!(parse_euid("Name: x\n"), None);
        assert_eq!(parse_df_free_mb("h\n/dev/x 100 50 2048 50% /\n"), Some(2));
        assert_eq!(parse_df_free_mb("h\n"), None);
    }

    #[test]
    fn json_is_one_object() {
        let checks = run_all(&good_server());
        let j = to_json("srv \"1\"", &checks);
        assert!(
            j.starts_with(r#"{"host":"srv \"1\"","passed":true,"checks":[{"id":"os","level":"ok""#),
            "{j}"
        );
        assert_eq!(j.matches(r#""id":"#).count(), CHECKS.len());
    }

    #[test]
    fn host_and_port_of_the_relay_address() {
        assert_eq!(
            host_port("https://relay.example"),
            ("relay.example".to_owned(), 443)
        );
        assert_eq!(
            host_port("http://127.0.0.1:18095"),
            ("127.0.0.1".to_owned(), 18095)
        );
        assert_eq!(
            host_port("https://relay.example:8443"),
            ("relay.example".to_owned(), 8443)
        );
    }
}
