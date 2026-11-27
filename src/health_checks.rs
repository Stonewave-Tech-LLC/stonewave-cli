//! `stonewave doctor --health`: how is this server doing? Deterministic, read only, no model.
//!
//! The plain `doctor` asks "can this server be connected?". This group asks "is it healthy?": failed
//! systemd units, a pending reboot, pending security updates, automatic updates, disk and inode use,
//! read-only mounts, software RAID, SMART, listening ports, uptime and kernel.
//!
//! It is the input of the nightly server check (the Hausmeister) and of the server alarm, so the output
//! is a stable schema ([`SCHEMA`]) and follows the rules the Hausmeister already uses:
//!
//! * five severities that map one to one: `critical`=kritisch, `high`=hoch, `medium`=mittel, `low`=niedrig,
//!   `info`=info (every finding also carries `severity_de`);
//! * **source duty**: a finding names the command or file it comes from and the line numbers of that
//!   output, plus the lines themselves (`evidence`, at most five, cleaned);
//! * what could not be read is never silently "fine": it is listed under `not_collected` with the reason
//!   and what is needed (`needs`), and the check is `partial` or `not_collected`;
//! * the severity rules are the Hausmeister's (RAID `[_U]` or `(F)` is always `critical`; a file system is
//!   `high` from 85 % and `critical` from 95 %; security updates are `high`; and so on).
//!
//! Everything it reads goes through [`Probe`], so every rule is tested against made-up servers. It never
//! runs a command with input taken from the machine, except SMART device names, which must be
//! `/dev/<letters and digits>` from `smartctl --scan` (see `smart`). It never reads process environments,
//! file contents of services or log lines: `pm2 jlist`, `docker inspect` and the like are not used here.

use std::collections::BTreeMap;

use serde_json::{Value, json};

use crate::doctor::{Probe, parse_euid};

/// The schema name of the JSON. Changes that break readers get a new number.
pub const SCHEMA: &str = "stonewave.health/1";

const GIB: u64 = 1024 * 1024 * 1024;
/// At most this many evidence lines per finding.
const MAX_EVIDENCE: usize = 5;
/// Evidence and text taken from the machine are cut to this many characters.
const MAX_TEXT: usize = 200;
/// SMART is asked for at most this many devices.
const MAX_SMART_DEVICES: usize = 8;
/// Package lists older than this many days make the update counts doubtful.
const STALE_LISTS_DAYS: u64 = 7;

// ---------------------------------------------------------------- vocabulary

/// How bad a finding is. Ordered: `Info` is the lowest.
#[derive(Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Debug)]
pub enum Severity {
    Info,
    Low,
    Medium,
    High,
    Critical,
}

impl Severity {
    pub fn as_str(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "low",
            Severity::Medium => "medium",
            Severity::High => "high",
            Severity::Critical => "critical",
        }
    }

    /// The word the Hausmeister uses for the same severity.
    pub fn hausmeister(self) -> &'static str {
        match self {
            Severity::Info => "info",
            Severity::Low => "niedrig",
            Severity::Medium => "mittel",
            Severity::High => "hoch",
            Severity::Critical => "kritisch",
        }
    }
}

/// How sure a finding is (the Hausmeister's `vertrauen`; `extern` is not used here, nothing is fetched).
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Trust {
    /// It stands in the output as it is (`gemessen`).
    Measured,
    /// A conclusion from several values or from something missing (`abgeleitet`).
    Derived,
}

impl Trust {
    fn as_str(self) -> &'static str {
        match self {
            Trust::Measured => "measured",
            Trust::Derived => "derived",
        }
    }
}

/// How much of a check could be read.
#[derive(Clone, Copy, PartialEq, Eq, Debug)]
pub enum Collection {
    Collected,
    Partial,
    NotCollected,
}

impl Collection {
    pub fn as_str(self) -> &'static str {
        match self {
            Collection::Collected => "collected",
            Collection::Partial => "partial",
            Collection::NotCollected => "not_collected",
        }
    }
}

/// Where a finding comes from: a command or a file, and the lines of its output.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Source {
    /// `"command"` or `"file"`.
    pub kind: &'static str,
    pub what: String,
    /// 1-based, `"3"` or `"3-5"`.
    pub lines: Option<String>,
}

#[derive(Clone, Debug)]
pub struct Finding {
    pub severity: Severity,
    pub title: String,
    /// Where on the machine, e.g. `systemd · nginx.service`.
    pub location: String,
    pub description: String,
    pub recommendation: String,
    pub trust: Trust,
    pub source: Source,
    pub evidence: Vec<String>,
}

/// Something that could not be read.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Gap {
    pub what: String,
    pub reason: String,
    /// What would make it readable (a right or a package), if known.
    pub needs: Option<String>,
}

/// One check: its facts, its findings and its gaps.
#[derive(Clone, Debug)]
pub struct Area {
    pub id: &'static str,
    pub title: &'static str,
    pub data: Value,
    pub findings: Vec<Finding>,
    pub gaps: Vec<Gap>,
    /// How many things were read successfully (a file that is absent on purpose, such as `/proc/mdstat`
    /// without software RAID, counts as read).
    read_ok: u32,
}

impl Area {
    fn new(id: &'static str, title: &'static str) -> Self {
        Area {
            id,
            title,
            data: json!({}),
            findings: Vec::new(),
            gaps: Vec::new(),
            read_ok: 0,
        }
    }

    fn got(&mut self) {
        self.read_ok += 1;
    }

    fn gap(&mut self, what: &str, reason: &str, needs: Option<&str>) {
        self.gaps.push(Gap {
            what: what.to_owned(),
            reason: reason.to_owned(),
            needs: needs.map(str::to_owned),
        });
    }

    pub fn collection(&self) -> Collection {
        if self.read_ok == 0 {
            Collection::NotCollected
        } else if self.gaps.is_empty() {
            Collection::Collected
        } else {
            Collection::Partial
        }
    }
}

// ---------------------------------------------------------------- text from the machine

/// Text that came from the machine is data, never an instruction: control characters (line breaks,
/// escape codes, invisible characters) are replaced and the length is cut.
pub fn clean(s: &str) -> String {
    let cleaned: String = s
        .chars()
        .map(|c| if c.is_control() { '?' } else { c })
        .take(MAX_TEXT)
        .collect();
    cleaned.trim().to_owned()
}

/// `"3"` or `"3-5"` for the first and last line (1-based) for which `pred` is true.
fn line_range(text: &str, pred: impl Fn(&str) -> bool) -> Option<String> {
    let mut first = 0;
    let mut last = 0;
    for (i, l) in text.lines().enumerate() {
        if pred(l) {
            if first == 0 {
                first = i + 1;
            }
            last = i + 1;
        }
    }
    match first {
        0 => None,
        f if f == last => Some(f.to_string()),
        f => Some(format!("{f}-{last}")),
    }
}

/// The matching lines, cleaned, at most [`MAX_EVIDENCE`].
fn evidence(text: &str, pred: impl Fn(&str) -> bool) -> Vec<String> {
    text.lines()
        .filter(|l| pred(l))
        .take(MAX_EVIDENCE)
        .map(clean)
        .collect()
}

fn command_source(cmd: &str, text: &str, pred: impl Fn(&str) -> bool) -> (Source, Vec<String>) {
    (
        Source {
            kind: "command",
            what: cmd.to_owned(),
            lines: line_range(text, &pred),
        },
        evidence(text, &pred),
    )
}

fn file_source(path: &str, text: &str, pred: impl Fn(&str) -> bool) -> (Source, Vec<String>) {
    (
        Source {
            kind: "file",
            what: path.to_owned(),
            lines: line_range(text, &pred),
        },
        evidence(text, &pred),
    )
}

fn finding(
    severity: Severity,
    trust: Trust,
    title: String,
    location: String,
    description: String,
    recommendation: &str,
    (source, evidence): (Source, Vec<String>),
) -> Finding {
    Finding {
        severity,
        title,
        location,
        description,
        recommendation: recommendation.to_owned(),
        trust,
        source,
        evidence,
    }
}

// ---------------------------------------------------------------- parsers

/// Whether `name` (for example `ro`) is among the comma separated mount options.
fn has_option(options: &str, name: &str) -> bool {
    options.split(',').any(|o| o == name)
}

/// File system types that hold data (the rest is virtual).
const PERSISTENT_FS: [&str; 8] = [
    "ext2", "ext3", "ext4", "xfs", "btrfs", "f2fs", "vfat", "zfs",
];

#[derive(Clone, Debug, PartialEq)]
pub struct FsRow {
    pub device: String,
    pub fstype: String,
    pub total: u64,
    pub used: u64,
    pub free: u64,
    pub percent: u64,
    pub mount: String,
    /// Line of `df` in which it stands (1-based).
    pub line: usize,
    pub inode_percent: Option<u64>,
    pub inodes_total: Option<u64>,
    pub inodes_used: Option<u64>,
    pub inode_line: Option<usize>,
}

/// `df -PT -B1` rows: `Filesystem Type 1-blocks Used Available Capacity Mounted on`.
pub fn parse_df_bytes(text: &str) -> Vec<FsRow> {
    text.lines()
        .enumerate()
        .skip(1)
        .filter_map(|(i, l)| {
            let c: Vec<&str> = l.split_whitespace().collect();
            if c.len() < 7 {
                return None;
            }
            Some(FsRow {
                device: clean(c[0]),
                fstype: clean(c[1]),
                total: c[2].parse().ok()?,
                used: c[3].parse().ok()?,
                free: c[4].parse().ok()?,
                percent: c[5].trim_end_matches('%').parse().ok()?,
                mount: clean(&c[6..].join(" ")),
                line: i + 1,
                inode_percent: None,
                inodes_total: None,
                inodes_used: None,
                inode_line: None,
            })
        })
        .collect()
}

/// Adds the inode columns of `df -PTi` to the rows with the same mount point.
pub fn merge_inodes(rows: &mut [FsRow], text: &str) {
    for (i, l) in text.lines().enumerate().skip(1) {
        let c: Vec<&str> = l.split_whitespace().collect();
        if c.len() < 7 {
            continue;
        }
        let mount = clean(&c[6..].join(" "));
        if let Some(r) = rows.iter_mut().find(|r| r.mount == mount) {
            r.inodes_total = c[2].parse().ok();
            r.inodes_used = c[3].parse().ok();
            r.inode_percent = c[5].trim_end_matches('%').parse().ok();
            r.inode_line = Some(i + 1);
        }
    }
}

/// The Hausmeister's rule: from 95 % `critical`, from 85 % `high`. For file systems of 50 GiB and more
/// also by free space: under 5 GiB `critical`, under 20 GiB `high` (a percentage alone misleads on big disks).
pub fn disk_severity(percent: u64, free: u64, total: u64) -> Option<Severity> {
    let by_percent = match percent {
        95.. => Some(Severity::Critical),
        85.. => Some(Severity::High),
        _ => None,
    };
    let big = total >= 50 * GIB;
    let by_free = if big && free < 5 * GIB {
        Some(Severity::Critical)
    } else if big && free < 20 * GIB {
        Some(Severity::High)
    } else {
        None
    };
    by_percent.max(by_free)
}

pub fn inode_severity(percent: u64) -> Option<Severity> {
    match percent {
        95.. => Some(Severity::Critical),
        85.. => Some(Severity::High),
        _ => None,
    }
}

fn human(bytes: u64) -> String {
    if bytes >= GIB {
        format!("{:.1} GiB", bytes as f64 / GIB as f64)
    } else {
        format!("{} MiB", bytes / (1024 * 1024))
    }
}

#[derive(Clone, Debug, PartialEq)]
pub struct MdArray {
    pub name: String,
    pub level: String,
    pub members: Vec<String>,
    pub failed: Vec<String>,
    /// `UU` or `U_`.
    pub pattern: String,
    pub degraded: bool,
    /// Lines (1-based) of the array line and its status line.
    pub first_line: usize,
    pub last_line: usize,
}

/// The arrays in `/proc/mdstat`. Degraded means `[U_]`-like patterns or a member marked `(F)`.
pub fn parse_mdstat(text: &str) -> Vec<MdArray> {
    let lines: Vec<&str> = text.lines().collect();
    let mut out = Vec::new();
    for (i, l) in lines.iter().enumerate() {
        let Some((name, rest)) = l.split_once(" : ") else {
            continue;
        };
        let is_md = name.len() > 2
            && name.starts_with("md")
            && name[2..].chars().all(|c| c.is_ascii_digit());
        if !is_md {
            continue;
        }
        let mut tokens = rest.split_whitespace();
        let _state = tokens.next();
        // inactive arrays have no level
        let level = tokens.next().unwrap_or("").to_owned();
        let mut members = Vec::new();
        let mut failed = Vec::new();
        for t in tokens {
            if let Some(dev) = t
                .split('[')
                .next()
                .filter(|d| !d.is_empty() && t.contains('['))
            {
                members.push(clean(dev));
                if t.ends_with("(F)") {
                    failed.push(clean(dev));
                }
            }
        }
        // the status line is the next line that holds a `[UU]`-like token
        let mut pattern = String::new();
        let mut last = i + 1;
        for (j, next) in lines.iter().enumerate().skip(i + 1).take(3) {
            if next.contains(" : ") && next.starts_with("md") {
                break;
            }
            if let Some(p) = next.split_whitespace().find(|t| {
                t.len() > 2
                    && t.starts_with('[')
                    && t.ends_with(']')
                    && t[1..t.len() - 1].chars().all(|c| c == 'U' || c == '_')
            }) {
                pattern = p[1..p.len() - 1].to_owned();
                last = j + 1;
                break;
            }
        }
        let degraded = pattern.contains('_') || !failed.is_empty();
        out.push(MdArray {
            name: name.to_owned(),
            level: clean(&level),
            members,
            failed,
            pattern,
            degraded,
            first_line: i + 1,
            last_line: last,
        });
    }
    out
}

/// The ports that listen, from `ss -lntupH` (one row per socket).
#[derive(Clone, Debug, PartialEq)]
pub struct Listener {
    pub proto: String,
    pub addr: String,
    pub port: u16,
    pub process: Option<String>,
    pub line: usize,
}

pub fn parse_ss(text: &str) -> Vec<Listener> {
    text.lines()
        .enumerate()
        .filter_map(|(i, l)| {
            let c: Vec<&str> = l.split_whitespace().collect();
            let proto = *c.first()?;
            if proto != "tcp" && proto != "udp" {
                return None;
            }
            let local = *c.get(4)?;
            let (addr, port) = local.rsplit_once(':')?;
            let process = l.find("users:((\"").map(|at| {
                let rest = &l[at + "users:((\"".len()..];
                clean(rest.split('"').next().unwrap_or(""))
            });
            Some(Listener {
                proto: proto.to_owned(),
                addr: clean(addr),
                port: port.parse().ok()?,
                process: process.filter(|p| !p.is_empty()),
                line: i + 1,
            })
        })
        .collect()
}

/// Loopback, private, link-local: not reachable from the internet by itself. (Docker bridges are
/// `172.16/12`, so they count as private, as in the Hausmeister.) Wildcards are public.
pub fn is_private(addr: &str) -> bool {
    let a = addr.trim_matches(|c| c == '[' || c == ']');
    let a = a.split('%').next().unwrap_or(a);
    let a = a.strip_prefix("::ffff:").unwrap_or(a);
    if a == "::1" || a.starts_with("127.") || a.starts_with("10.") || a.starts_with("192.168.") {
        return true;
    }
    if a.starts_with("169.254.") || a.to_ascii_lowercase().starts_with("fe80") {
        return true;
    }
    if let Some(rest) = a.strip_prefix("172.") {
        if let Some(second) = rest.split('.').next().and_then(|s| s.parse::<u8>().ok()) {
            return (16..=31).contains(&second);
        }
    }
    false
}

/// Ports that should not listen on a public address (the Hausmeister's list plus a few more).
pub fn sensitive_port(port: u16) -> Option<&'static str> {
    Some(match port {
        3306 => "MariaDB/MySQL",
        5432 | 5433 => "PostgreSQL",
        6543 => "Supavisor pooler",
        6379 => "Redis",
        27017 => "MongoDB",
        9200 => "Elasticsearch",
        11211 => "Memcached",
        2375 | 2376 => "Docker API",
        23 => "Telnet",
        5900 => "VNC",
        139 | 445 => "SMB",
        25575 => "Minecraft RCON",
        _ => return None,
    })
}

/// `APT::Periodic::Unattended-Upgrade "1";` → 1. Commented lines (`//`, `#`) do not count.
pub fn periodic_flag(text: &str, key: &str) -> Option<u32> {
    text.lines()
        .map(str::trim)
        .filter(|l| !l.starts_with("//") && !l.starts_with('#'))
        .find(|l| l.contains(key))
        .and_then(|l| l.split('"').nth(1))
        .and_then(|v| v.trim().parse().ok())
}

/// `apt list --upgradable` rows, with whether the package comes from a security suite.
pub fn parse_upgradable(text: &str) -> Vec<(String, bool, usize)> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| l.contains('/') && l.contains("[upgradable"))
        .map(|(i, l)| {
            let name = l.split('/').next().unwrap_or("").to_owned();
            (clean(&name), l.contains("-security"), i + 1)
        })
        .collect()
}

fn uptime_seconds(text: &str) -> Option<u64> {
    text.split_whitespace()
        .next()?
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
        .map(|v| v as u64)
}

/// A unit name as `systemctl` prints it: no spaces, no control characters.
fn plausible_unit(name: &str) -> bool {
    !name.is_empty()
        && name.len() <= 120
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '-' | '_' | '@' | ':' | '\\'))
}

// ---------------------------------------------------------------- the checks

const NOT_DEBIAN: &str = "this check knows Debian and Ubuntu (no /var/lib/dpkg/status)";

fn is_debian_like(p: &dyn Probe) -> bool {
    p.exists("/var/lib/dpkg/status")
}

fn system(p: &dyn Probe) -> Area {
    let mut a = Area::new("system", "Uptime and kernel");
    let uptime = p.read("/proc/uptime").as_deref().and_then(uptime_seconds);
    let kernel = p
        .read("/proc/sys/kernel/osrelease")
        .map(|k| clean(k.trim()))
        .filter(|k| !k.is_empty());
    if uptime.is_some() || kernel.is_some() {
        a.got();
    }
    if uptime.is_none() {
        a.gap("uptime", "/proc/uptime not readable", None);
    }
    if kernel.is_none() {
        a.gap(
            "kernel version",
            "/proc/sys/kernel/osrelease not readable",
            None,
        );
    }
    a.data = json!({ "uptime_s": uptime, "kernel": kernel });
    a
}

fn units(p: &dyn Probe) -> Area {
    let mut a = Area::new("units", "Failed systemd units");
    let cmd = "systemctl list-units --type=service --state=failed --no-legend --plain";
    let args = [
        "list-units",
        "--type=service",
        "--state=failed",
        "--no-legend",
        "--plain",
    ];
    let Some(out) = p.run("systemctl", &args) else {
        a.gap("failed units", "systemctl did not run (no systemd?)", None);
        a.data = json!({ "failed": Value::Null });
        return a;
    };
    let mut failed = Vec::new();
    let mut odd = false;
    for l in out.lines().filter(|l| !l.trim().is_empty()) {
        // old systemd prints a bullet in front even with --plain
        let name = l
            .split_whitespace()
            .find(|t| !matches!(*t, "●" | "*"))
            .unwrap_or("");
        if plausible_unit(name) && name.contains('.') {
            failed.push(name.to_owned());
        } else {
            odd = true;
        }
    }
    if odd && failed.is_empty() {
        // the output was an error text, not a list
        a.gap("failed units", "unexpected output from systemctl", None);
        a.data = json!({ "failed": Value::Null });
        return a;
    }
    a.got();
    for name in &failed {
        let src = command_source(cmd, &out, |l| {
            l.split_whitespace().any(|t| t == name.as_str())
        });
        a.findings.push(finding(
            Severity::Medium,
            Trust::Measured,
            format!("systemd unit {name} failed"),
            format!("systemd · {name}"),
            "The unit is in the state failed and not active. It may be a leftover of a change.".into(),
            "Check whether the unit is needed; if not, disable or remove it (root), so the failed state does not hide real problems. If it is needed, read its journal.",
            src,
        ));
    }
    a.data = json!({ "failed": failed });
    a
}

fn reboot(p: &dyn Probe) -> Area {
    let mut a = Area::new("reboot", "Reboot after updates");
    if !is_debian_like(p) {
        a.gap("reboot after updates", NOT_DEBIAN, None);
        a.data = json!({ "required": Value::Null, "packages": [] });
        return a;
    }
    let files = ["/var/run/reboot-required", "/run/reboot-required"];
    let file = files.into_iter().find(|f| p.exists(f));
    a.got();
    let packages: Vec<String> = if file.is_some() {
        ["/var/run/reboot-required.pkgs", "/run/reboot-required.pkgs"]
            .into_iter()
            .find_map(|f| p.read(f))
            .map(|t| {
                t.lines()
                    .map(clean)
                    .filter(|l| !l.is_empty())
                    .take(10)
                    .collect()
            })
            .unwrap_or_default()
    } else {
        Vec::new()
    };
    if let Some(f) = file {
        let mut description =
            "The system says a reboot is needed (for example a new kernel).".to_owned();
        if !packages.is_empty() {
            description.push_str(&format!(" Packages: {}.", packages.join(", ")));
        }
        a.findings.push(finding(
            Severity::Medium,
            Trust::Measured,
            "Reboot after updates pending".into(),
            format!("host · {f}"),
            description,
            "Plan a reboot in a maintenance window, not while a disk or RAID problem is open.",
            (
                Source {
                    kind: "file",
                    what: f.to_owned(),
                    lines: None,
                },
                Vec::new(),
            ),
        ));
    }
    a.data = json!({ "required": file.is_some(), "packages": packages });
    a
}

fn updates(p: &dyn Probe, now: u64) -> Area {
    let mut a = Area::new("updates", "Pending updates");
    let cmd = "apt list --upgradable";
    let Some(out) = p.run_slow("apt", &["list", "--upgradable"]) else {
        a.gap(
            "pending updates",
            "apt did not run (not a Debian or Ubuntu system?)",
            None,
        );
        a.data = json!({ "total": Value::Null, "security": Value::Null });
        return a;
    };
    if !out.lines().any(|l| l.starts_with("Listing")) {
        a.gap("pending updates", "unexpected output from apt", None);
        a.data = json!({ "total": Value::Null, "security": Value::Null });
        return a;
    }
    a.got();
    let rows = parse_upgradable(&out);
    let security: Vec<&(String, bool, usize)> = rows.iter().filter(|r| r.1).collect();
    if !security.is_empty() {
        let names: Vec<&str> = security.iter().take(12).map(|r| r.0.as_str()).collect();
        let more = if security.len() > 12 { " ..." } else { "" };
        a.findings.push(finding(
            Severity::High,
            Trust::Measured,
            format!("{} security updates available", security.len()),
            "host · apt".into(),
            format!("Packages: {}{more}.", names.join(", ")),
            "Apply soon (apt-get upgrade as root); after kernel, libc or openssl updates plan a reboot.",
            command_source(cmd, &out, |l| l.contains("-security")),
        ));
    }
    // how old are the package lists? (a stale list makes the counts above doubtful)
    let lists_age_days = p
        .run("stat", &["-c", "%Y", "/var/lib/apt/lists"])
        .and_then(|t| t.trim().parse::<u64>().ok())
        .filter(|m| *m > 0 && *m <= now)
        .map(|m| (now - m) / 86_400);
    if let Some(days) = lists_age_days.filter(|d| *d > STALE_LISTS_DAYS) {
        a.findings.push(finding(
            Severity::Low,
            Trust::Derived,
            format!("Package lists are {days} days old"),
            "host · /var/lib/apt/lists".into(),
            "The update counts above come from old lists and may be too low.".into(),
            "Check that apt-daily.timer runs (systemctl status apt-daily.timer).",
            (
                Source {
                    kind: "command",
                    what: "stat -c %Y /var/lib/apt/lists".into(),
                    lines: Some("1".into()),
                },
                Vec::new(),
            ),
        ));
    }
    a.data = json!({
        "total": rows.len(),
        "security": security.len(),
        "security_packages": security.iter().take(50).map(|r| r.0.clone()).collect::<Vec<_>>(),
        "lists_age_days": lists_age_days,
    });
    a
}

fn autoupdates(p: &dyn Probe) -> Area {
    let mut a = Area::new("autoupdates", "Automatic security updates");
    if !is_debian_like(p) {
        a.gap("automatic security updates", NOT_DEBIAN, None);
        a.data =
            json!({ "installed": Value::Null, "periodic": Value::Null, "active": Value::Null });
        return a;
    }
    let conf_path = "/etc/apt/apt.conf.d/20auto-upgrades";
    let installed = p.exists("/usr/bin/unattended-upgrade");
    let conf = p.read(conf_path);
    a.got();
    let flag = conf
        .as_deref()
        .and_then(|t| periodic_flag(t, "Unattended-Upgrade"));
    let active = installed && flag.is_some_and(|f| f >= 1);
    if !active {
        let why = match (installed, flag) {
            (false, _) => "unattended-upgrades is not installed",
            (true, None) => {
                "unattended-upgrades is installed but not switched on in 20auto-upgrades"
            }
            (true, Some(_)) => "unattended-upgrades is switched off in 20auto-upgrades",
        };
        let src = match conf.as_deref() {
            Some(t) => file_source(conf_path, t, |l| l.contains("Unattended-Upgrade")),
            None => (
                Source {
                    kind: "file",
                    what: conf_path.to_owned(),
                    lines: None,
                },
                Vec::new(),
            ),
        };
        a.findings.push(finding(
            Severity::Medium,
            Trust::Derived,
            "No automatic security updates (unattended-upgrades not active)".into(),
            "host · apt".into(),
            format!("{why}; security updates are only applied by hand."),
            "Switch on unattended-upgrades for the security origin (root), or fix a manual update rhythm.",
            src,
        ));
    }
    a.data = json!({ "installed": installed, "periodic": flag, "active": active });
    a
}

fn storage(p: &dyn Probe) -> Area {
    let mut a = Area::new("storage", "Disk, inodes and mounts");
    const EXCLUDE: [&str; 10] = [
        "-x", "tmpfs", "-x", "devtmpfs", "-x", "squashfs", "-x", "overlay", "-x", "efivarfs",
    ];
    let df_cmd = "df -PT -B1 -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs";
    let dfi_cmd = "df -PTi -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs";
    let mut df_args = vec!["-PT", "-B1"];
    df_args.extend(EXCLUDE);
    let mut dfi_args = vec!["-PTi"];
    dfi_args.extend(EXCLUDE);

    let df = p.run("df", &df_args);
    let mut rows = df.as_deref().map(parse_df_bytes).unwrap_or_default();
    if df.is_none() || rows.is_empty() {
        a.gap("file systems", "df gave no usable output", None);
    } else {
        a.got();
    }
    let dfi = p.run("df", &dfi_args);
    match dfi.as_deref() {
        Some(t) if !rows.is_empty() => merge_inodes(&mut rows, t),
        Some(_) => {}
        None => a.gap("inode use", "df -i gave no output", None),
    }
    let df_text = df.unwrap_or_default();
    let dfi_text = dfi.unwrap_or_default();
    for r in &rows {
        if let Some(sev) = disk_severity(r.percent, r.free, r.total) {
            a.findings.push(finding(
                sev,
                Trust::Measured,
                format!("{} is {} % full ({} free of {})", r.mount, r.percent, human(r.free), human(r.total)),
                format!("{} · {}", r.device, r.mount),
                format!(
                    "{} of {} used, {} free ({}). Limits: from 85 % high, from 95 % critical; for file systems of 50 GiB and more also under 20 GiB free high and under 5 GiB free critical.",
                    human(r.used), human(r.total), human(r.free), r.fstype
                ),
                "Look at the biggest directories and Docker volumes (du -xh --max-depth=2) and clean up; think of backups and build artefacts on a shared file system.",
                command_source(df_cmd, &df_text, |l| clean(l.split_whitespace().last().unwrap_or("")) == r.mount && l.contains('%')),
            ));
        }
        if let Some((pct, sev)) = r
            .inode_percent
            .and_then(|pct| inode_severity(pct).map(|sev| (pct, sev)))
        {
            a.findings.push(finding(
                sev,
                Trust::Measured,
                format!("{}: {pct} % of the inodes used", r.mount),
                format!("{} · {}", r.device, r.mount),
                format!(
                    "{} of {} inodes used. A file system can be full while bytes are still free.",
                    r.inodes_used.unwrap_or(0),
                    r.inodes_total.unwrap_or(0)
                ),
                "Find directories with very many small files and clean them up.",
                command_source(dfi_cmd, &dfi_text, |l| {
                    clean(l.split_whitespace().last().unwrap_or("")) == r.mount && l.contains('%')
                }),
            ));
        }
    }
    // mounts that went read only
    let fm_cmd = "findmnt -rno TARGET,SOURCE,FSTYPE,OPTIONS";
    let mut read_only = Vec::new();
    match p.run("findmnt", &["-rno", "TARGET,SOURCE,FSTYPE,OPTIONS"]) {
        Some(t) => {
            a.got();
            for l in t.lines() {
                let c: Vec<&str> = l.split(' ').collect();
                if c.len() < 4 || !PERSISTENT_FS.contains(&c[2]) || !has_option(c[3], "ro") {
                    continue;
                }
                let target = clean(c[0]);
                let sev = if target == "/" || target == "/var" {
                    Severity::Critical
                } else {
                    Severity::High
                };
                read_only.push(target.clone());
                a.findings.push(finding(
                    sev,
                    Trust::Measured,
                    format!("{target} is mounted read only (ro)"),
                    format!("{} · {target}", clean(c[1])),
                    format!(
                        "The {} file system on {} is on ro. A file system that should be rw often turns ro after file system or disk errors.",
                        clean(c[2]),
                        clean(c[1])
                    ),
                    "Find the cause first (kernel log, file system errors, disk); do not just remount while a RAID or disk is unhealthy.",
                    command_source(fm_cmd, &t, |x| x.split(' ').next().map(clean).as_deref() == Some(target.as_str())),
                ));
            }
        }
        None => a.gap("mount options", "findmnt did not run", None),
    }
    a.data = json!({
        "file_systems": rows.iter().take(50).map(|r| json!({
            "device": r.device, "type": r.fstype, "mount": r.mount,
            "total_b": r.total, "used_b": r.used, "free_b": r.free, "percent": r.percent,
            "inode_percent": r.inode_percent,
        })).collect::<Vec<_>>(),
        "read_only_mounts": read_only,
    });
    a
}

fn raid(p: &dyn Probe) -> Area {
    let mut a = Area::new("raid", "Software RAID");
    let path = "/proc/mdstat";
    let Some(text) = p.read(path) else {
        // no md driver: there is no software RAID to be degraded
        a.got();
        a.data = json!({ "present": false, "arrays": [] });
        return a;
    };
    a.got();
    let arrays = parse_mdstat(&text);
    for r in arrays.iter().filter(|r| r.degraded) {
        let range = if r.first_line == r.last_line {
            r.first_line.to_string()
        } else {
            format!("{}-{}", r.first_line, r.last_line)
        };
        let ev = text
            .lines()
            .enumerate()
            .filter(|(i, _)| (r.first_line - 1..r.last_line).contains(i))
            .map(|(_, l)| clean(l))
            .take(MAX_EVIDENCE)
            .collect();
        a.findings.push(finding(
            Severity::Critical,
            Trust::Measured,
            format!("Software RAID {} is degraded: a member failed", r.name),
            format!("host · {path}"),
            format!(
                "The array {} ({}) runs without redundancy (pattern [{}]{}). If the remaining disk fails, the data is gone.",
                r.name,
                r.level,
                r.pattern,
                if r.failed.is_empty() {
                    String::new()
                } else {
                    format!(", failed: {}", r.failed.join(", "))
                }
            ),
            "Order the disk replacement at once and save a current backup somewhere else first. Do not run mdadm by yourself.",
            (
                Source {
                    kind: "file",
                    what: path.to_owned(),
                    lines: Some(range),
                },
                ev,
            ),
        ));
    }
    a.data = json!({
        "present": true,
        "arrays": arrays.iter().map(|r| json!({
            "name": r.name, "level": r.level, "members": r.members, "failed": r.failed,
            "pattern": r.pattern, "degraded": r.degraded,
        })).collect::<Vec<_>>(),
    });
    a
}

/// `/dev/sda`, `/dev/nvme0`: letters and digits only. Names that come out of `smartctl --scan` are
/// checked before they become an argument.
fn smart_device(name: &str) -> bool {
    name.strip_prefix("/dev/").is_some_and(|rest| {
        !rest.is_empty() && rest.len() <= 20 && rest.chars().all(|c| c.is_ascii_alphanumeric())
    })
}

fn smart(p: &dyn Probe, root: bool) -> Area {
    let mut a = Area::new("smart", "SMART disk health");
    if !root {
        a.gap(
            "SMART health of the disks",
            "reading SMART needs root",
            Some("run as root (sudo stonewave doctor --health) and install smartmontools"),
        );
        a.data = json!({ "devices": Value::Null });
        return a;
    }
    let Some(scan) = p.run_slow("smartctl", &["--scan"]) else {
        a.gap(
            "SMART health of the disks",
            "smartctl did not run",
            Some("install smartmontools"),
        );
        a.data = json!({ "devices": Value::Null });
        return a;
    };
    a.got();
    let mut devices = Vec::new();
    for l in scan.lines().filter(|l| !l.trim().is_empty()) {
        let mut t = l.split_whitespace();
        let dev = t.next().unwrap_or("");
        let kind = if t.next() == Some("-d") {
            t.next()
        } else {
            None
        };
        if !smart_device(dev) {
            a.gap(
                "a scanned device",
                "its name is not a plain /dev/<name> and was not asked",
                None,
            );
            continue;
        }
        let kind = kind.filter(|k| ["ata", "scsi", "nvme", "sat"].contains(k));
        devices.push((dev.to_owned(), kind));
    }
    let mut results = Vec::new();
    if devices.len() > MAX_SMART_DEVICES {
        a.gap(
            "more disks than the check asks",
            &format!("only the first {MAX_SMART_DEVICES} are asked"),
            None,
        );
    }
    for (dev, kind) in devices.iter().take(MAX_SMART_DEVICES) {
        let mut args = vec!["-H"];
        if let Some(k) = kind {
            args.extend(["-d", *k]);
        }
        args.push(dev.as_str());
        let cmd = format!("smartctl {}", args.join(" "));
        let Some(out) = p.run_slow("smartctl", &args) else {
            a.gap(
                &format!("SMART health of {dev}"),
                "smartctl did not answer in time",
                None,
            );
            results.push(json!({ "device": dev, "health": Value::Null }));
            continue;
        };
        let passed = out.contains("test result: PASSED") || out.contains("SMART Health Status: OK");
        let failed = out.contains("test result: FAILED")
            || (out.contains("SMART Health Status:") && !out.contains("SMART Health Status: OK"));
        if failed {
            a.findings.push(finding(
                Severity::Critical,
                Trust::Measured,
                format!("SMART reports FAILED for {dev}"),
                format!("host · {dev}"),
                "The disk does not pass its own health test.".into(),
                "Plan the replacement and save the data first; do not run mdadm by yourself.",
                command_source(&cmd, &out, |l| {
                    l.contains("test result") || l.contains("Health Status")
                }),
            ));
            results.push(json!({ "device": dev, "health": "failed" }));
        } else if passed {
            results.push(json!({ "device": dev, "health": "passed" }));
        } else {
            let reason = if out.to_ascii_lowercase().contains("permission denied") {
                "permission denied"
            } else {
                "no health line in the output"
            };
            a.gap(&format!("SMART health of {dev}"), reason, None);
            results.push(json!({ "device": dev, "health": Value::Null }));
        }
    }
    a.data = json!({ "devices": results });
    a
}

fn ports(p: &dyn Probe, root: bool) -> Area {
    let mut a = Area::new("ports", "Listening ports");
    let cmd = "ss -lntupH";
    let Some(out) = p.run("ss", &["-lntupH"]) else {
        a.gap("listening sockets", "ss did not run", None);
        a.data = json!({ "listeners": Value::Null, "public": Value::Null });
        return a;
    };
    a.got();
    let listeners = parse_ss(&out);
    let public: Vec<&Listener> = listeners.iter().filter(|l| !is_private(&l.addr)).collect();
    let mut public_keys: Vec<String> = public
        .iter()
        .map(|l| format!("{}/{}", l.proto, l.port))
        .collect();
    public_keys.sort();
    public_keys.dedup();

    if !public_keys.is_empty() {
        a.findings.push(finding(
            Severity::Info,
            Trust::Measured,
            format!("{} ports listen on all interfaces", public_keys.len()),
            format!("host · {cmd}"),
            format!(
                "Bound to a wildcard or public address (not loopback, private or Docker bridge): {}. Whether they are reachable from outside depends on the firewall, which this check does not see.",
                public_keys.join(", ")
            ),
            "Check every entry against the list of what should be open; bind what is only needed locally to 127.0.0.1.",
            command_source(cmd, &out, |l| {
                parse_ss(l).first().is_some_and(|x| !is_private(&x.addr))
            }),
        ));
    }
    let mut sensitive: BTreeMap<u16, (&'static str, usize)> = BTreeMap::new();
    for l in &public {
        if let Some(name) = sensitive_port(l.port) {
            sensitive.entry(l.port).or_insert((name, l.line));
        }
    }
    if !sensitive.is_empty() {
        let list: Vec<String> = sensitive
            .iter()
            .map(|(port, (name, _))| format!("{port} ({name})"))
            .collect();
        let ports_set: Vec<u16> = sensitive.keys().copied().collect();
        a.findings.push(finding(
            Severity::High,
            Trust::Derived,
            format!("Database or file services on all interfaces: {}", list.join(", ")),
            format!("host · {cmd}"),
            "These ports are not bound to loopback. Without a visible host firewall, their reachability depends on a firewall that this check cannot see.".into(),
            "Bind them to 127.0.0.1 or the Docker network (for example 127.0.0.1:5432:5432 in compose) or block them with a firewall; check from a second host.",
            command_source(cmd, &out, |l| {
                parse_ss(l)
                    .first()
                    .is_some_and(|x| !is_private(&x.addr) && ports_set.contains(&x.port))
            }),
        ));
    }
    let blind = listeners.iter().filter(|l| l.process.is_none()).count();
    if blind > 0 && !root {
        a.gap(
            &format!("process names of {blind} sockets"),
            "without root, ss only shows your own processes",
            Some("run as root"),
        );
    }
    a.data = json!({
        "listeners": listeners.iter().take(200).map(|l| json!({
            "proto": l.proto, "addr": l.addr, "port": l.port, "process": l.process,
        })).collect::<Vec<_>>(),
        "public": public_keys,
    });
    a
}

// ---------------------------------------------------------------- the report

/// All checks, in the order they are shown. `now` is the time in seconds since 1970.
pub fn collect(p: &dyn Probe, now: u64) -> (bool, Vec<Area>) {
    let root = p.read("/proc/self/status").as_deref().and_then(parse_euid) == Some(0);
    let areas = vec![
        system(p),
        units(p),
        reboot(p),
        updates(p, now),
        autoupdates(p),
        storage(p),
        raid(p),
        smart(p, root),
        ports(p, root),
    ];
    (root, areas)
}

/// The most severe finding, if any.
pub fn highest(areas: &[Area]) -> Option<Severity> {
    areas
        .iter()
        .flat_map(|a| a.findings.iter().map(|f| f.severity))
        .max()
}

/// Exit code of `doctor --health`: 1 when something `high` or `critical` was found, else 0.
/// Gaps do not change it; they are in the report.
pub fn exit_code(areas: &[Area]) -> u8 {
    u8::from(highest(areas).is_some_and(|s| s >= Severity::High))
}

fn finding_json(f: &Finding) -> Value {
    let mut source = json!({ f.source.kind: f.source.what });
    if let Some(l) = &f.source.lines {
        source["lines"] = json!(l);
    }
    json!({
        "severity": f.severity.as_str(),
        "severity_de": f.severity.hausmeister(),
        "title": f.title,
        "location": f.location,
        "description": f.description,
        "recommendation": f.recommendation,
        "trust": f.trust.as_str(),
        "source": source,
        "evidence": f.evidence,
    })
}

/// The report as one JSON value: [`SCHEMA`].
pub fn to_json(host: &str, root: bool, now: u64, areas: &[Area]) -> Value {
    let mut counts = BTreeMap::new();
    for s in [
        Severity::Critical,
        Severity::High,
        Severity::Medium,
        Severity::Low,
        Severity::Info,
    ] {
        counts.insert(s.as_str(), 0u32);
    }
    for f in areas.iter().flat_map(|a| &a.findings) {
        *counts.entry(f.severity.as_str()).or_insert(0) += 1;
    }
    let not_fully = areas
        .iter()
        .filter(|a| a.collection() != Collection::Collected)
        .count();
    json!({
        "schema": SCHEMA,
        "stonewave": crate::VERSION,
        "host": clean(host),
        "ts": now,
        "root": root,
        "summary": {
            "highest": highest(areas).map_or("none", Severity::as_str),
            "counts": counts,
            "checks": areas.len(),
            "not_fully_collected": not_fully,
        },
        "checks": areas.iter().map(|a| json!({
            "id": a.id,
            "title": a.title,
            "collection": a.collection().as_str(),
            "data": a.data,
            "findings": a.findings.iter().map(finding_json).collect::<Vec<_>>(),
            "not_collected": a.gaps.iter().map(|g| json!({
                "what": g.what, "reason": g.reason, "needs": g.needs,
            })).collect::<Vec<_>>(),
        })).collect::<Vec<_>>(),
    })
}

/// Plain text for people: no colours, no cursor codes, the same in a pipe and in a terminal.
pub fn to_text(host: &str, root: bool, areas: &[Area]) -> String {
    let mut out = format!(
        "stonewave doctor --health · {} · {}\n\n",
        clean(host),
        if root {
            "as root"
        } else {
            "not root: some checks need root"
        }
    );
    for a in areas {
        let mark = match a.collection() {
            Collection::Collected if a.findings.is_empty() => "ok".to_owned(),
            Collection::Collected => "found".to_owned(),
            Collection::Partial => "partial".to_owned(),
            Collection::NotCollected => "not checked".to_owned(),
        };
        out.push_str(&format!("{:<14}{:<13}{}\n", a.id, mark, a.title));
        for f in &a.findings {
            out.push_str(&format!(
                "    {:<9}{}\n",
                f.severity.as_str().to_uppercase(),
                f.title
            ));
        }
        for g in &a.gaps {
            let needs = g
                .needs
                .as_deref()
                .map_or(String::new(), |n| format!(" ({n})"));
            out.push_str(&format!(
                "    not checked: {} · {}{needs}\n",
                g.what, g.reason
            ));
        }
    }
    out.push('\n');
    out.push_str(&match highest(areas) {
        Some(s) => format!("Highest severity: {}\n", s.as_str()),
        None => "No findings.\n".to_owned(),
    });
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::Duration;

    const NOW: u64 = 1_800_000_000;

    /// A made-up server. Commands are looked up as `cmd arg arg`.
    #[derive(Default)]
    struct Fake {
        files: HashMap<String, String>,
        cmds: HashMap<String, String>,
    }

    impl Fake {
        fn file(mut self, p: &str, t: &str) -> Self {
            self.files.insert(p.into(), t.into());
            self
        }
        fn cmd(mut self, c: &str, t: &str) -> Self {
            self.cmds.insert(c.into(), t.into());
            self
        }
    }

    impl Probe for Fake {
        fn read(&self, path: &str) -> Option<String> {
            self.files.get(path).cloned()
        }
        fn exists(&self, path: &str) -> bool {
            self.files.contains_key(path)
        }
        fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
            self.cmds.get(&format!("{cmd} {}", args.join(" "))).cloned()
        }
        fn connect(&self, _: &str, _: u16) -> std::result::Result<Duration, String> {
            Err("not asked".into())
        }
        fn env(&self, _: &str) -> Option<String> {
            None
        }
    }

    const UNITS: &str = "systemctl list-units --type=service --state=failed --no-legend --plain";
    const DF: &str = "df -PT -B1 -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs";
    const DFI: &str = "df -PTi -x tmpfs -x devtmpfs -x squashfs -x overlay -x efivarfs";
    const SS: &str = "ss -lntupH";

    fn healthy() -> Fake {
        Fake::default()
            .file("/proc/self/status", "Name:\tstonewave\nUid:\t1000\t1000\t1000\t1000\n")
            .file("/proc/uptime", "864000.55 1000.00\n")
            .file("/proc/sys/kernel/osrelease", "6.8.0-45-generic\n")
            .file("/usr/bin/unattended-upgrade", "")
            .file("/var/lib/dpkg/status", "")
            .file(
                "/etc/apt/apt.conf.d/20auto-upgrades",
                "APT::Periodic::Update-Package-Lists \"1\";\nAPT::Periodic::Unattended-Upgrade \"1\";\n",
            )
            .cmd(UNITS, "")
            .cmd("apt list --upgradable", "Listing...\n")
            .cmd("stat -c %Y /var/lib/apt/lists", &(NOW - 86_400).to_string())
            .cmd(
                DF,
                "Filesystem Type 1-blocks Used Available Capacity Mounted on\n/dev/sda1 ext4 100000000000 40000000000 60000000000 40% /\n",
            )
            .cmd(
                DFI,
                "Filesystem Type Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 ext4 6000000 100000 5900000 2% /\n",
            )
            .cmd("findmnt -rno TARGET,SOURCE,FSTYPE,OPTIONS", "/ /dev/sda1 ext4 rw,relatime\n")
            .cmd(
                SS,
                "tcp LISTEN 0 128 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=1,fd=5))\ntcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=2,fd=3))\n",
            )
    }

    fn area<'a>(areas: &'a [Area], id: &str) -> &'a Area {
        areas.iter().find(|a| a.id == id).unwrap()
    }

    fn run(f: &Fake) -> Vec<Area> {
        collect(f, NOW).1
    }

    // ---- the whole report

    #[test]
    fn a_healthy_server_has_no_finding_above_info() {
        let areas = run(&healthy());
        assert_eq!(areas.len(), 9);
        assert_eq!(highest(&areas), Some(Severity::Info), "the open ssh port");
        assert_eq!(exit_code(&areas), 0);
        for id in [
            "system",
            "units",
            "reboot",
            "updates",
            "autoupdates",
            "storage",
            "raid",
        ] {
            assert_eq!(area(&areas, id).collection(), Collection::Collected, "{id}");
            assert!(area(&areas, id).findings.is_empty(), "{id}");
        }
    }

    #[test]
    fn what_needs_root_is_not_checked_not_fine() {
        let areas = run(&healthy());
        let smart = area(&areas, "smart");
        assert_eq!(smart.collection(), Collection::NotCollected);
        assert_eq!(smart.gaps.len(), 1);
        assert!(
            smart.gaps[0]
                .needs
                .as_deref()
                .unwrap()
                .contains("smartmontools")
        );
        assert_eq!(smart.data["devices"], Value::Null);
        // process names of the ports: partial, because ss hides other users' processes
        let ports = area(&areas, "ports");
        assert_eq!(
            ports.collection(),
            Collection::Collected,
            "all rows have a process here"
        );
    }

    #[test]
    fn sockets_without_a_process_name_make_the_ports_check_partial_unless_root() {
        let f = healthy().cmd(SS, "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n");
        let areas = run(&f);
        let ports = area(&areas, "ports");
        assert_eq!(ports.collection(), Collection::Partial);
        assert!(ports.gaps[0].what.contains("1 sockets"));
        let root = healthy()
            .file("/proc/self/status", "Uid:\t0\t0\t0\t0\n")
            .cmd(SS, "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:*\n");
        assert_eq!(
            area(&run(&root), "ports").collection(),
            Collection::Collected
        );
    }

    #[test]
    fn json_has_a_stable_shape() {
        let (root, areas) = collect(&healthy(), NOW);
        let v = to_json("srv-1", root, NOW, &areas);
        assert_eq!(v["schema"], "stonewave.health/1");
        assert_eq!(v["host"], "srv-1");
        assert_eq!(v["ts"], NOW);
        assert_eq!(v["root"], false);
        let keys: Vec<&str> = v.as_object().unwrap().keys().map(String::as_str).collect();
        for k in [
            "schema",
            "stonewave",
            "host",
            "ts",
            "root",
            "summary",
            "checks",
        ] {
            assert!(keys.contains(&k), "{k}");
        }
        assert_eq!(v["summary"]["highest"], "info");
        for sev in ["critical", "high", "medium", "low", "info"] {
            assert!(v["summary"]["counts"].get(sev).is_some(), "{sev}");
        }
        let ids: Vec<&str> = v["checks"]
            .as_array()
            .unwrap()
            .iter()
            .map(|c| c["id"].as_str().unwrap())
            .collect();
        assert_eq!(
            ids,
            [
                "system",
                "units",
                "reboot",
                "updates",
                "autoupdates",
                "storage",
                "raid",
                "smart",
                "ports"
            ]
        );
        for c in v["checks"].as_array().unwrap() {
            for k in [
                "id",
                "title",
                "collection",
                "data",
                "findings",
                "not_collected",
            ] {
                assert!(c.get(k).is_some(), "{} lacks {k}", c["id"]);
            }
        }
        // one finding has every field
        let f = &v["checks"][8]["findings"][0];
        for k in [
            "severity",
            "severity_de",
            "title",
            "location",
            "description",
            "recommendation",
            "trust",
            "source",
            "evidence",
        ] {
            assert!(f.get(k).is_some(), "finding lacks {k}");
        }
        assert!(serde_json::to_string(&v).unwrap().lines().count() == 1);
    }

    #[test]
    fn severities_map_to_the_hausmeister_words() {
        let pairs = [
            (Severity::Critical, "critical", "kritisch"),
            (Severity::High, "high", "hoch"),
            (Severity::Medium, "medium", "mittel"),
            (Severity::Low, "low", "niedrig"),
            (Severity::Info, "info", "info"),
        ];
        for (s, en, de) in pairs {
            assert_eq!((s.as_str(), s.hausmeister()), (en, de));
        }
        assert!(Severity::Critical > Severity::High && Severity::High > Severity::Medium);
        assert!(Severity::Medium > Severity::Low && Severity::Low > Severity::Info);
    }

    #[test]
    fn text_output_is_plain() {
        let (root, areas) = collect(&healthy(), NOW);
        let t = to_text("srv-1", root, &areas);
        assert!(!t.contains('\u{1b}'));
        assert!(t.contains("smart"));
        assert!(t.contains("not checked"));
        assert!(t.contains("Highest severity: info"));
    }

    // ---- units

    #[test]
    fn a_failed_unit_is_medium_with_its_source_line() {
        let out = "nginx.service loaded failed failed A high performance web server\n● old.service loaded failed failed Leftover\n";
        let areas = run(&healthy().cmd(UNITS, out));
        let a = area(&areas, "units");
        assert_eq!(a.findings.len(), 2);
        let f = &a.findings[0];
        assert_eq!(f.severity, Severity::Medium);
        assert_eq!(f.title, "systemd unit nginx.service failed");
        assert_eq!(f.source.what, UNITS);
        assert_eq!(f.source.lines.as_deref(), Some("1"));
        assert_eq!(a.findings[1].source.lines.as_deref(), Some("2"));
        assert!(f.evidence[0].starts_with("nginx.service"));
        assert_eq!(a.data["failed"], json!(["nginx.service", "old.service"]));
    }

    #[test]
    fn an_error_text_is_not_a_list_of_units() {
        let areas = run(&healthy().cmd(UNITS, "System has not been booted with systemd"));
        let a = area(&areas, "units");
        assert!(a.findings.is_empty());
        assert_eq!(a.collection(), Collection::NotCollected);
    }

    #[test]
    fn no_systemd_means_not_checked() {
        let mut f = healthy();
        f.cmds.remove(UNITS);
        let a = run(&f);
        assert_eq!(area(&a, "units").collection(), Collection::NotCollected);
    }

    // ---- reboot, updates, autoupdates

    #[test]
    fn a_pending_reboot_is_medium_and_names_packages() {
        let f = healthy()
            .file(
                "/var/run/reboot-required",
                "*** System restart required ***\n",
            )
            .file(
                "/var/run/reboot-required.pkgs",
                "linux-image-6.8.0-50-generic\nlibc6\n",
            );
        let a = run(&f);
        let r = area(&a, "reboot");
        assert_eq!(r.findings.len(), 1);
        assert_eq!(r.findings[0].severity, Severity::Medium);
        assert!(
            r.findings[0]
                .description
                .contains("linux-image-6.8.0-50-generic")
        );
        assert_eq!(r.data["required"], true);
        assert_eq!(area(&run(&healthy()), "reboot").data["required"], false);
    }

    #[test]
    fn security_updates_are_high_and_only_security_counts() {
        let apt = "Listing...\nopenssl/jammy-updates,jammy-security 3.0.2 amd64 [upgradable from: 3.0.1]\ncurl/jammy-updates,jammy-security 7.81 amd64 [upgradable from: 7.80]\nvim/jammy-updates 9.0 amd64 [upgradable from: 8.9]\n";
        let a = run(&healthy().cmd("apt list --upgradable", apt));
        let u = area(&a, "updates");
        assert_eq!(u.findings.len(), 1);
        let f = &u.findings[0];
        assert_eq!(f.severity, Severity::High);
        assert_eq!(f.title, "2 security updates available");
        assert!(f.description.contains("openssl") && f.description.contains("curl"));
        assert!(!f.description.contains("vim"));
        assert_eq!(f.source.lines.as_deref(), Some("2-3"));
        assert_eq!(u.data["total"], 3);
        assert_eq!(u.data["security"], 2);
        assert_eq!(exit_code(&a), 1);
    }

    #[test]
    fn stale_package_lists_are_a_low_finding() {
        let f = healthy().cmd(
            "stat -c %Y /var/lib/apt/lists",
            &(NOW - 30 * 86_400).to_string(),
        );
        let a = run(&f);
        let u = area(&a, "updates");
        assert_eq!(u.findings[0].severity, Severity::Low);
        assert!(u.findings[0].title.contains("30 days"));
        assert_eq!(u.findings[0].trust, Trust::Derived);
    }

    #[test]
    fn apt_error_text_is_a_gap_not_zero_updates() {
        let a = run(&healthy().cmd("apt list --upgradable", "E: Could not get lock"));
        let u = area(&a, "updates");
        assert_eq!(u.collection(), Collection::NotCollected);
        assert_eq!(u.data["security"], Value::Null);
    }

    #[test]
    fn automatic_updates_must_be_installed_and_switched_on() {
        let a = run(&healthy());
        assert!(area(&a, "autoupdates").findings.is_empty());
        assert_eq!(area(&a, "autoupdates").data["active"], true);

        let mut off = healthy();
        off.files.remove("/usr/bin/unattended-upgrade");
        let f = &run(&off)[4].findings;
        assert_eq!(f.len(), 1);
        assert_eq!(f[0].severity, Severity::Medium);
        assert!(f[0].description.contains("not installed"));

        let zero = healthy().file(
            "/etc/apt/apt.conf.d/20auto-upgrades",
            "APT::Periodic::Unattended-Upgrade \"0\";\n",
        );
        assert_eq!(run(&zero)[4].findings.len(), 1);

        let commented = healthy().file(
            "/etc/apt/apt.conf.d/20auto-upgrades",
            "// APT::Periodic::Unattended-Upgrade \"1\";\n",
        );
        assert_eq!(
            run(&commented)[4].findings.len(),
            1,
            "a commented line does not count"
        );
    }

    #[test]
    fn periodic_flag_reads_numbers() {
        assert_eq!(
            periodic_flag(
                "APT::Periodic::Unattended-Upgrade \"1\";",
                "Unattended-Upgrade"
            ),
            Some(1)
        );
        assert_eq!(
            periodic_flag(
                "# APT::Periodic::Unattended-Upgrade \"1\";",
                "Unattended-Upgrade"
            ),
            None
        );
        assert_eq!(periodic_flag("", "Unattended-Upgrade"), None);
    }

    #[test]
    fn a_system_without_dpkg_is_not_checked_for_debian_things() {
        let mut f = healthy();
        f.files.remove("/var/lib/dpkg/status");
        let a = run(&f);
        for id in ["reboot", "autoupdates"] {
            assert_eq!(area(&a, id).collection(), Collection::NotCollected, "{id}");
            assert!(area(&a, id).findings.is_empty(), "{id}: no false finding");
        }
        assert_eq!(area(&a, "reboot").data["required"], Value::Null);
    }

    // ---- storage

    #[test]
    fn disk_severity_follows_the_hausmeister_rules() {
        let small = 10 * GIB;
        assert_eq!(disk_severity(84, 2 * GIB, small), None);
        assert_eq!(disk_severity(85, 2 * GIB, small), Some(Severity::High));
        assert_eq!(disk_severity(94, 2 * GIB, small), Some(Severity::High));
        assert_eq!(disk_severity(95, GIB, small), Some(Severity::Critical));
        // a 1 GiB /boot with 10 % free is not critical by free space: the free-space rule is for big disks
        assert_eq!(disk_severity(90, GIB / 10, GIB), Some(Severity::High));
        // a big disk at 80 % is fine by percent but not by free space
        let big = 1000 * GIB;
        assert_eq!(disk_severity(80, 200 * GIB, big), None);
        assert_eq!(disk_severity(80, 19 * GIB, big), Some(Severity::High));
        assert_eq!(disk_severity(80, 4 * GIB, big), Some(Severity::Critical));
        // the worse of both wins
        assert_eq!(disk_severity(96, 100 * GIB, big), Some(Severity::Critical));
        assert_eq!(inode_severity(84), None);
        assert_eq!(inode_severity(85), Some(Severity::High));
        assert_eq!(inode_severity(95), Some(Severity::Critical));
    }

    #[test]
    fn a_full_disk_and_full_inodes_are_found_with_lines() {
        let df = "Filesystem Type 1-blocks Used Available Capacity Mounted on\n/dev/sda1 ext4 100000000000 40000000000 60000000000 40% /\n/dev/sdb1 ext4 10000000000 9700000000 300000000 97% /data\n";
        let dfi = "Filesystem Type Inodes IUsed IFree IUse% Mounted on\n/dev/sda1 ext4 6000000 100000 5900000 2% /\n/dev/sdb1 ext4 1000 900 100 90% /data\n";
        let a = run(&healthy().cmd(DF, df).cmd(DFI, dfi));
        let s = area(&a, "storage");
        assert_eq!(s.findings.len(), 2);
        assert_eq!(s.findings[0].severity, Severity::Critical);
        assert!(s.findings[0].title.starts_with("/data is 97 % full"));
        assert_eq!(s.findings[0].source.lines.as_deref(), Some("3"));
        assert_eq!(s.findings[1].severity, Severity::High);
        assert!(s.findings[1].title.contains("90 % of the inodes"));
        assert_eq!(s.findings[1].source.what, DFI);
        assert_eq!(exit_code(&a), 1);
    }

    #[test]
    fn a_read_only_root_is_critical_and_another_mount_high() {
        let fm = "/ /dev/sda1 ext4 ro,relatime\n/data /dev/sdb1 xfs ro\n/proc proc proc ro\n";
        let a = run(&healthy().cmd("findmnt -rno TARGET,SOURCE,FSTYPE,OPTIONS", fm));
        let s = area(&a, "storage");
        assert_eq!(s.findings.len(), 2, "proc is virtual");
        assert_eq!(s.findings[0].severity, Severity::Critical);
        assert_eq!(s.findings[1].severity, Severity::High);
        assert_eq!(s.findings[0].source.lines.as_deref(), Some("1"));
        assert_eq!(s.data["read_only_mounts"], json!(["/", "/data"]));
    }

    #[test]
    fn df_failing_is_a_gap() {
        let mut f = healthy();
        f.cmds.remove(DF);
        f.cmds.remove(DFI);
        let a = run(&f);
        let s = area(&a, "storage");
        assert!(s.gaps.iter().any(|g| g.what == "file systems"));
        assert!(s.gaps.iter().any(|g| g.what == "inode use"));
        assert_ne!(s.collection(), Collection::Collected);
    }

    // ---- raid

    const MDSTAT_OK: &str = "Personalities : [raid1]\nmd0 : active raid1 nvme1n1p1[1] nvme0n1p1[0]\n      523264 blocks super 1.2 [2/2] [UU]\n\nunused devices: <none>\n";
    const MDSTAT_BAD: &str = "Personalities : [raid1]\nmd0 : active raid1 nvme1n1p1[1] nvme0n1p1[0]\n      523264 blocks super 1.2 [2/1] [_U]\n\nunused devices: <none>\n";
    const MDSTAT_F: &str = "Personalities : [raid1]\nmd1 : active raid1 sdb1[1](F) sda1[0]\n      1000 blocks super 1.2 [2/2] [UU]\n";

    #[test]
    fn raid_degraded_is_always_critical() {
        let a = run(&healthy().file("/proc/mdstat", MDSTAT_BAD));
        let r = area(&a, "raid");
        assert_eq!(r.findings.len(), 1);
        let f = &r.findings[0];
        assert_eq!(f.severity, Severity::Critical);
        assert_eq!(f.severity.hausmeister(), "kritisch");
        assert_eq!(f.source.what, "/proc/mdstat");
        assert_eq!(f.source.kind, "file");
        assert_eq!(f.source.lines.as_deref(), Some("2-3"));
        assert_eq!(f.evidence.len(), 2);
        assert!(f.evidence[1].contains("[_U]"));
        assert_eq!(exit_code(&a), 1);
    }

    #[test]
    fn a_member_marked_failed_is_critical_even_when_the_pattern_looks_fine() {
        let a = run(&healthy().file("/proc/mdstat", MDSTAT_F));
        let r = area(&a, "raid");
        assert_eq!(r.findings.len(), 1);
        assert!(r.findings[0].description.contains("failed: sdb1"));
    }

    #[test]
    fn a_healthy_array_and_no_raid_are_both_fine() {
        let a = run(&healthy().file("/proc/mdstat", MDSTAT_OK));
        assert!(area(&a, "raid").findings.is_empty());
        assert_eq!(area(&a, "raid").data["arrays"][0]["name"], "md0");
        assert_eq!(area(&a, "raid").data["arrays"][0]["pattern"], "UU");
        let none = run(&healthy());
        assert_eq!(area(&none, "raid").data["present"], false);
        assert_eq!(area(&none, "raid").collection(), Collection::Collected);
    }

    #[test]
    fn mdstat_parser_reads_two_arrays() {
        let t = format!(
            "{MDSTAT_OK}md1 : active raid0 sdc1[0] sdd1[1]\n      100 blocks 512k chunks\n"
        );
        let arrays = parse_mdstat(&t);
        assert_eq!(arrays.len(), 2);
        assert_eq!(arrays[0].members, ["nvme1n1p1", "nvme0n1p1"]);
        assert!(!arrays[0].degraded);
        assert_eq!(arrays[1].level, "raid0");
        assert_eq!(arrays[1].pattern, "");
    }

    // ---- smart

    fn root_server() -> Fake {
        healthy().file("/proc/self/status", "Uid:\t0\t0\t0\t0\n")
    }

    #[test]
    fn smart_failed_is_critical_passed_is_quiet() {
        let f = root_server()
            .cmd("smartctl --scan", "/dev/nvme0 -d nvme # /dev/nvme0, NVMe device\n/dev/sda -d sat # /dev/sda, ATA device\n")
            .cmd("smartctl -H -d nvme /dev/nvme0", "SMART overall-health self-assessment test result: FAILED!\n")
            .cmd("smartctl -H -d sat /dev/sda", "SMART overall-health self-assessment test result: PASSED\n");
        let a = run(&f);
        let s = area(&a, "smart");
        assert_eq!(s.collection(), Collection::Collected);
        assert_eq!(s.findings.len(), 1);
        assert_eq!(s.findings[0].severity, Severity::Critical);
        assert!(s.findings[0].title.contains("/dev/nvme0"));
        assert_eq!(s.findings[0].source.what, "smartctl -H -d nvme /dev/nvme0");
        assert_eq!(s.data["devices"][0]["health"], "failed");
        assert_eq!(s.data["devices"][1]["health"], "passed");
    }

    #[test]
    fn smart_without_smartctl_names_what_is_needed() {
        let a = run(&root_server());
        let s = area(&a, "smart");
        assert_eq!(s.collection(), Collection::NotCollected);
        assert_eq!(s.gaps[0].needs.as_deref(), Some("install smartmontools"));
    }

    #[test]
    fn a_device_name_from_the_scan_is_checked_before_it_is_used() {
        let scan = "/dev/sda;reboot -d sat # x\n/dev/../etc/shadow -d sat # x\n/dev/sdb -d `id` # x\nnot-a-device\n";
        let f = root_server().cmd("smartctl --scan", scan).cmd(
            "smartctl -H /dev/sdb",
            "SMART overall-health self-assessment test result: PASSED\n",
        );
        let a = run(&f);
        let s = area(&a, "smart");
        // only /dev/sdb is asked, and without the odd type
        assert_eq!(s.data["devices"].as_array().unwrap().len(), 1);
        assert_eq!(s.data["devices"][0]["device"], "/dev/sdb");
        assert_eq!(
            s.gaps
                .iter()
                .filter(|g| g.what == "a scanned device")
                .count(),
            3
        );
        assert!(smart_device("/dev/nvme0") && smart_device("/dev/sda"));
        for bad in [
            "/dev/",
            "/dev/a/b",
            "/dev/a b",
            "sda",
            "/dev/sda;ls",
            "/etc/passwd",
            "/dev/../x",
        ] {
            assert!(!smart_device(bad), "{bad}");
        }
    }

    #[test]
    fn a_smart_answer_without_a_health_line_is_a_gap_not_a_pass() {
        let f = root_server()
            .cmd("smartctl --scan", "/dev/sda -d sat # x\n")
            .cmd(
                "smartctl -H -d sat /dev/sda",
                "Smartctl open device: /dev/sda failed: Permission denied\n",
            );
        let a = run(&f);
        let s = area(&a, "smart");
        assert!(s.findings.is_empty());
        assert_eq!(s.gaps[0].reason, "permission denied");
        assert_eq!(s.data["devices"][0]["health"], Value::Null);
    }

    // ---- ports

    #[test]
    fn address_classes() {
        for a in [
            "127.0.0.1",
            "127.0.0.53%lo",
            "::1",
            "[::1]",
            "10.1.2.3",
            "192.168.0.5",
            "172.17.0.1",
            "172.31.0.1",
            "169.254.1.1",
            "fe80::1",
            "::ffff:127.0.0.1",
        ] {
            assert!(is_private(a), "{a}");
        }
        for a in [
            "0.0.0.0",
            "*",
            "[::]",
            "::",
            "8.8.8.8",
            "172.32.0.1",
            "172.15.0.1",
            "203.0.113.5",
        ] {
            assert!(!is_private(a), "{a}");
        }
    }

    #[test]
    fn ss_rows_are_parsed() {
        let t = "tcp LISTEN 0 4096 127.0.0.53%lo:53 0.0.0.0:* users:((\"systemd-resolve\",pid=1,fd=14))\nudp UNCONN 0 0 [::]:5353 [::]:*\ntcp LISTEN 0 511 *:80 *:*\nweird line\n";
        let l = parse_ss(t);
        assert_eq!(l.len(), 3);
        assert_eq!(l[0].addr, "127.0.0.53%lo");
        assert_eq!(l[0].port, 53);
        assert_eq!(l[0].process.as_deref(), Some("systemd-resolve"));
        assert_eq!(l[1].addr, "[::]");
        assert_eq!(l[1].process, None);
        assert_eq!(l[2].addr, "*");
        assert_eq!(l[2].line, 3);
    }

    #[test]
    fn a_database_on_all_interfaces_is_high() {
        let ss = "tcp LISTEN 0 128 0.0.0.0:22 0.0.0.0:* users:((\"sshd\",pid=2,fd=3))\ntcp LISTEN 0 128 0.0.0.0:5432 0.0.0.0:* users:((\"postgres\",pid=1,fd=5))\ntcp LISTEN 0 128 127.0.0.1:6379 0.0.0.0:* users:((\"redis\",pid=3,fd=6))\n";
        let a = run(&healthy().cmd(SS, ss));
        let p = area(&a, "ports");
        assert_eq!(p.findings.len(), 2);
        assert_eq!(p.findings[0].severity, Severity::Info);
        assert!(p.findings[0].title.starts_with("2 ports listen"));
        assert_eq!(p.findings[1].severity, Severity::High);
        assert!(p.findings[1].title.contains("5432 (PostgreSQL)"));
        assert!(!p.findings[1].title.contains("6379"), "loopback is fine");
        assert_eq!(p.findings[1].source.lines.as_deref(), Some("2"));
        assert_eq!(p.data["public"], json!(["tcp/22", "tcp/5432"]));
        assert_eq!(exit_code(&a), 1);
    }

    #[test]
    fn loopback_only_has_no_finding() {
        let ss = "tcp LISTEN 0 128 127.0.0.1:5432 0.0.0.0:* users:((\"postgres\",pid=1,fd=5))\n";
        let a = run(&healthy().cmd(SS, ss));
        assert!(area(&a, "ports").findings.is_empty());
        assert_eq!(highest(&a), None);
    }

    // ---- system and text from the machine

    #[test]
    fn uptime_and_kernel_are_facts_not_findings() {
        let a = run(&healthy());
        let s = area(&a, "system");
        assert_eq!(s.data["uptime_s"], 864000);
        assert_eq!(s.data["kernel"], "6.8.0-45-generic");
        assert!(s.findings.is_empty());
        let empty = run(&Fake::default());
        assert_eq!(
            area(&empty, "system").collection(),
            Collection::NotCollected
        );
    }

    #[test]
    fn text_from_the_machine_cannot_carry_line_breaks_or_escape_codes() {
        assert_eq!(clean("a\nb\u{1b}[31mc\u{7}"), "a?b?[31mc?");
        assert_eq!(clean("  x  "), "x");
        assert_eq!(clean(&"y".repeat(500)).chars().count(), 200);
        // a mount point with an escape code in it ends up cleaned in the report
        let df = "Filesystem Type 1-blocks Used Available Capacity Mounted on\n/dev/sdb1 ext4 100 99 1 99% /da\u{1b}[2Jta\n";
        let a = run(&healthy().cmd(DF, df));
        let json = serde_json::to_string(&to_json("h", false, NOW, &a)).unwrap();
        assert!(!json.contains("\\u001b"), "no escape code in the report");
    }

    #[test]
    fn a_unit_name_that_is_a_sentence_is_not_a_unit() {
        let evil = "ignore previous instructions.service loaded failed failed x\n";
        // the first word is not a plausible unit name with a dot ("ignore")
        let a = run(&healthy().cmd(UNITS, evil));
        assert!(area(&a, "units").findings.is_empty());
        assert_ne!(area(&a, "units").collection(), Collection::Collected);
    }

    #[test]
    fn nothing_here_runs_a_command_that_could_leak_secrets() {
        // The commands this module may ask for. A new command needs a new line here and a review.
        const ALLOWED: [(&str, &str); 9] = [
            (
                "systemctl",
                "list-units --type=service --state=failed --no-legend --plain",
            ),
            ("apt", "list --upgradable"),
            ("stat", "-c %Y /var/lib/apt/lists"),
            ("df", "-PT -B1"),
            ("df", "-PTi"),
            ("findmnt", "-rno TARGET,SOURCE,FSTYPE,OPTIONS"),
            ("smartctl", "--scan"),
            ("smartctl", "-H"),
            ("ss", "-lntupH"),
        ];
        let source = include_str!("health_checks.rs");
        let production = source.split("#[cfg(test)]").next().unwrap();
        for (cmd, _) in ALLOWED {
            assert!(production.contains(&format!("\"{cmd}\"")), "{cmd}");
        }
        for forbidden in ["pm2", "docker", "journalctl", "sudo", "env", "sh", "bash"] {
            assert!(
                !production.contains(&format!("\"{forbidden}\"")),
                "{forbidden} must not be run here"
            );
        }
        // every call of a command stands in the list above; a new one makes this count fail
        let calls = production.matches(".run(").count() + production.matches(".run_slow(").count();
        assert_eq!(
            calls,
            ALLOWED.len(),
            "a new command needs a new line in ALLOWED and a review"
        );
    }
}
