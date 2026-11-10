//! `stonewave setup` and `stonewave revoke`: the deterministic base setup of a server.
//!
//! Every step looks first and changes only what is missing, so a second run changes nothing. All
//! effects go through [`Sys`], so the steps run against a made-up server in the tests and, with
//! `--dry-run`, against the real one without touching it. The trust list of admin devices comes from
//! a file for now (`--trust-file`); Wave does not hand keys over yet.
//!
//! What is set up: an agent user without sudo, a trust list, a second sshd that only listens on
//! 127.0.0.1 and takes certificates for that one user (the server's own sshd stays untouched), the
//! daemon service, an audit log and a health check. Nothing here opens a port to the outside.

use crate::b64;
use crate::doctor::{Check, Level};

pub const BIN: &str = "/usr/local/bin/stonewave";
pub const ETC: &str = "/etc/stonewave";
pub const ETC_SSH: &str = "/etc/stonewave/ssh";
pub const VAR_LIB: &str = "/var/lib/stonewave";
pub const VAR_LOG: &str = "/var/log/stonewave";
pub const AGENT_USER: &str = "stonewave-agent";
pub const AGENT_HOME: &str = "/var/lib/stonewave-agent";
pub const DAEMON_USER: &str = "stonewave";
pub const AGENT_PORT: u16 = 2222;

pub const TRUST: &str = "/etc/stonewave/ssh/trusted_admin_cas";
pub const SSHD_CONFIG: &str = "/etc/stonewave/ssh/sshd_config";
pub const PRINCIPALS_DIR: &str = "/etc/stonewave/ssh/principals";
pub const KRL: &str = "/etc/stonewave/ssh/revoked_keys";
pub const HOST_KEY: &str = "/etc/stonewave/ssh/ssh_host_ed25519_key";
pub const SSHD_UNIT: &str = "/etc/systemd/system/stonewave-sshd.service";
pub const DAEMON_UNIT: &str = "/etc/systemd/system/stonewaved.service";
pub const LOGROTATE: &str = "/etc/logrotate.d/stonewave";
pub const STATE: &str = "/var/lib/stonewave/state.json";
pub const SERVER_KEY: &str = "/var/lib/stonewave/server.key";
pub const SERVER_IDENTITY: &str = "/var/lib/stonewave/server.json";
pub const AUDIT: &str = "/var/log/stonewave/audit.jsonl";
pub const SSHD_BIN: &str = "/usr/sbin/sshd";

/// What the agent may do right after connecting: read. More only with a signature of an admin device.
pub const DEFAULT_PRINCIPALS: [&str; 3] = ["sw.read", "sw.logs", "sw.health"];
/// Groups that would give the agent user power it must not have.
const FORBIDDEN_GROUPS: [&str; 6] = ["sudo", "admin", "wheel", "root", "adm", "docker"];

// ------------------------------------------------------------------ the machine

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Stat {
    pub mode: u32,
    pub user: String,
    pub group: String,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Run {
    pub ok: bool,
    pub out: String,
}

/// Everything setup does to or asks of the machine.
pub trait Sys {
    fn read(&self, path: &str) -> Option<String>;
    fn exists(&self, path: &str) -> bool;
    fn stat(&self, path: &str) -> Option<Stat>;
    /// Writes the whole file with this mode, never visible half written or with looser rights.
    fn write(&self, path: &str, content: &str, mode: u32) -> Result<(), String>;
    /// Creates the folder (and parents) and sets the mode of the last part.
    fn mkdir(&self, path: &str, mode: u32) -> Result<(), String>;
    fn remove(&self, path: &str);
    fn run(&self, cmd: &str, args: &[&str]) -> Run;
    /// Whether something listens on 127.0.0.1:port.
    fn listening(&self, port: u16) -> bool;
    fn euid(&self) -> u32;
}

/// One thing setup did, or would do with `--dry-run`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Change(pub String);

/// The state of one step after it ran.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Outcome {
    /// Was already in place.
    Kept(String),
    /// Changed (or would change) these things.
    Changed(Vec<Change>),
    /// A person has to look at it; nothing was forced.
    Attention(String),
}

pub struct Ctx<'a> {
    pub sys: &'a dyn Sys,
    pub dry: bool,
    pub changes: Vec<Change>,
}

type R<T> = Result<T, String>;

impl<'a> Ctx<'a> {
    pub fn new(sys: &'a dyn Sys, dry: bool) -> Ctx<'a> {
        Ctx {
            sys,
            dry,
            changes: Vec::new(),
        }
    }

    fn note(&mut self, what: impl Into<String>) {
        self.changes.push(Change(what.into()));
    }

    /// Runs a command that changes something. In a dry run it is only noted.
    fn act(&mut self, what: impl Into<String>, cmd: &str, args: &[&str]) -> R<()> {
        self.note(what);
        if self.dry {
            return Ok(());
        }
        let r = self.sys.run(cmd, args);
        if r.ok {
            Ok(())
        } else {
            Err(format!("{cmd} {}: {}", args.join(" "), first_line(&r.out)))
        }
    }

    fn matches(&self, path: &str, mode: u32, owner: (&str, &str)) -> bool {
        self.sys
            .stat(path)
            .is_some_and(|s| s.mode == mode && s.user == owner.0 && s.group == owner.1)
    }

    /// The file has exactly this content, mode and owner. Returns whether it had to change.
    fn file(&mut self, path: &str, content: &str, mode: u32, owner: (&str, &str)) -> R<bool> {
        let same = self.sys.read(path).as_deref() == Some(content);
        if same && self.matches(path, mode, owner) {
            return Ok(false);
        }
        let verb = if self.sys.exists(path) {
            "update"
        } else {
            "create"
        };
        self.note(format!("{verb} {path}"));
        if self.dry {
            return Ok(true);
        }
        self.sys.write(path, content, mode)?;
        let who = format!("{}:{}", owner.0, owner.1);
        let r = self.sys.run("chown", &[&who, path]);
        if !r.ok {
            return Err(format!("chown {who} {path}: {}", first_line(&r.out)));
        }
        Ok(true)
    }

    /// Like [`file`], but a candidate is tested with `check` first and the old file stays when it fails.
    fn file_checked(
        &mut self,
        path: &str,
        content: &str,
        mode: u32,
        owner: (&str, &str),
        check: &dyn Fn(&str) -> Run,
    ) -> R<bool> {
        if self.sys.read(path).as_deref() == Some(content) && self.matches(path, mode, owner) {
            return Ok(false);
        }
        if self.dry {
            return self.file(path, content, mode, owner);
        }
        let candidate = format!("{path}.new");
        self.sys.write(&candidate, content, 0o600)?;
        let tested = check(&candidate);
        if !tested.ok {
            self.sys.remove(&candidate);
            return Err(format!(
                "the new {path} does not pass its check: {}",
                first_line(&tested.out)
            ));
        }
        self.sys.remove(&candidate);
        self.file(path, content, mode, owner)
    }

    fn dir(&mut self, path: &str, mode: u32, owner: (&str, &str)) -> R<bool> {
        if self.sys.exists(path) && self.matches(path, mode, owner) {
            return Ok(false);
        }
        let verb = if self.sys.exists(path) {
            "fix rights of"
        } else {
            "create"
        };
        self.note(format!("{verb} {path}/"));
        if self.dry {
            return Ok(true);
        }
        self.sys.mkdir(path, mode)?;
        let who = format!("{}:{}", owner.0, owner.1);
        let r = self.sys.run("chown", &[&who, path]);
        if !r.ok {
            return Err(format!("chown {who} {path}: {}", first_line(&r.out)));
        }
        Ok(true)
    }
}

fn first_line(s: &str) -> String {
    s.lines().next().unwrap_or("").chars().take(200).collect()
}

fn finish(ctx: Ctx<'_>, kept: &str) -> Outcome {
    if ctx.changes.is_empty() {
        Outcome::Kept(kept.to_owned())
    } else {
        Outcome::Changed(ctx.changes)
    }
}

// ------------------------------------------------------------------ the trust list

/// A public key an admin device is known by. Only these two key types: they are what the devices make
/// (P-256 in a secure chip, or Ed25519 elsewhere) and what sshd takes as a certificate authority.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct TrustKey {
    pub kind: String,
    pub key: String,
    pub device: String,
}

const KEY_TYPES: [&str; 2] = ["ecdsa-sha2-nistp256", "ssh-ed25519"];
const DEVICE_PREFIX: &str = "stonewave-device:";

fn clean_device(raw: &str) -> String {
    let s: String = raw
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || " ._-".contains(c) {
                c
            } else {
                '_'
            }
        })
        .take(60)
        .collect();
    s.trim().to_owned()
}

/// `type base64 [device name]`. The key is checked for real: its own length-prefixed type name must
/// match the type in front of it, so a line cannot carry something else than it says.
pub fn parse_key_line(line: &str, fallback_name: &str) -> R<TrustKey> {
    let mut parts = line.split_whitespace();
    let kind = parts.next().ok_or("empty line")?;
    let key = parts.next().ok_or("no key after the key type")?;
    if !KEY_TYPES.contains(&kind) {
        return Err(format!(
            "key type {kind} is not allowed (ecdsa-sha2-nistp256 or ssh-ed25519 only)"
        ));
    }
    let blob = b64::decode(key).ok_or("the key is not valid base64")?;
    if blob.len() < 4 {
        return Err("the key is too short".into());
    }
    let n = u32::from_be_bytes([blob[0], blob[1], blob[2], blob[3]]) as usize;
    if blob.get(4..4 + n) != Some(kind.as_bytes()) {
        return Err("the key does not belong to its key type".into());
    }
    let rest: Vec<&str> = parts.collect();
    let joined = rest.join(" ");
    let mut name = clean_device(joined.strip_prefix(DEVICE_PREFIX).unwrap_or(&joined));
    if name.is_empty() {
        name = fallback_name.to_owned();
    }
    Ok(TrustKey {
        kind: kind.to_owned(),
        key: key.to_owned(),
        device: name,
    })
}

/// A file with one public key per line (`#` starts a comment).
pub fn parse_trust_file(text: &str) -> R<Vec<TrustKey>> {
    let mut out = Vec::new();
    for (i, line) in text.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let k = parse_key_line(line, &format!("device-{}", out.len() + 1))
            .map_err(|e| format!("line {}: {e}", i + 1))?;
        out.push(k);
    }
    Ok(out)
}

fn render_key(k: &TrustKey) -> String {
    format!("{} {} {DEVICE_PREFIX}{}", k.kind, k.key, k.device)
}

/// The trust list file as it is on the server. Lines that do not parse are dropped, never kept blindly.
pub fn read_trust(text: &str) -> Vec<TrustKey> {
    text.lines()
        .filter(|l| !l.trim().is_empty() && !l.trim().starts_with('#'))
        .filter_map(|l| parse_key_line(l, "device").ok())
        .collect()
}

/// Numbers of the lines in the trust file that stonewave cannot read (other key types, options, typos). Setup and revoke
/// rewrite the file from what they understand, so they must not touch a file that holds something else.
pub fn foreign_lines(text: &str) -> Vec<usize> {
    text.lines()
        .enumerate()
        .filter(|(_, l)| !l.trim().is_empty() && !l.trim().starts_with('#'))
        .filter(|(_, l)| parse_key_line(l, "device").is_err())
        .map(|(i, _)| i + 1)
        .collect()
}

fn foreign_message(lines: &[usize]) -> String {
    format!(
        "{TRUST} has line(s) {} that stonewave does not understand (not an ssh-ed25519 or ecdsa-sha2-nistp256 key without options). It was left as it is: fix or remove them by hand",
        lines
            .iter()
            .map(usize::to_string)
            .collect::<Vec<_>>()
            .join(", ")
    )
}

fn render_trust(keys: &[TrustKey]) -> String {
    let mut s = String::from(
        "# Admin devices this server trusts as certificate authorities (one public key per line).\n\
         # Managed by stonewave; `sudo stonewave revoke` empties it.\n",
    );
    for k in keys {
        s.push_str(&render_key(k));
        s.push('\n');
    }
    s
}

// ------------------------------------------------------------------ generated files

pub fn sshd_config() -> String {
    format!(
        "# Stonewave agent SSH. Written by `stonewave setup`; changes are overwritten.\n\
         # Only listens on this machine; the daemon connects outbound and hands connections to it.\n\
         Port {AGENT_PORT}\n\
         ListenAddress 127.0.0.1\n\
         HostKey {HOST_KEY}\n\
         PidFile /run/stonewave-sshd.pid\n\
         AllowUsers {AGENT_USER}\n\
         PermitRootLogin no\n\
         PasswordAuthentication no\n\
         KbdInteractiveAuthentication no\n\
         PubkeyAuthentication yes\n\
         AuthenticationMethods publickey\n\
         PermitEmptyPasswords no\n\
         TrustedUserCAKeys {TRUST}\n\
         AuthorizedPrincipalsFile {PRINCIPALS_DIR}/%u\n\
         RevokedKeys {KRL}\n\
         ExposeAuthInfo yes\n\
         ForceCommand {BIN} agent-shell\n\
         PermitTTY no\n\
         X11Forwarding no\n\
         AllowTcpForwarding no\n\
         AllowAgentForwarding no\n\
         PermitTunnel no\n\
         GatewayPorts no\n\
         PermitUserEnvironment no\n\
         MaxAuthTries 3\n\
         LoginGraceTime 20\n\
         ClientAliveInterval 30\n\
         ClientAliveCountMax 3\n\
         LogLevel VERBOSE\n\
         UsePAM yes\n"
    )
}

pub fn principals() -> String {
    let mut s =
        String::from("# Rights an agent certificate may carry on this server (upper limit).\n");
    for p in DEFAULT_PRINCIPALS {
        s.push_str(p);
        s.push('\n');
    }
    s
}

pub fn sshd_unit() -> String {
    format!(
        "[Unit]\n\
         Description=Stonewave agent SSH (127.0.0.1 only, certificates only)\n\
         After=network.target\n\
         \n\
         [Service]\n\
         ExecStartPre=/usr/bin/install -d -m 0755 /run/sshd\n\
         ExecStartPre={SSHD_BIN} -t -f {SSHD_CONFIG}\n\
         ExecStart={SSHD_BIN} -D -e -f {SSHD_CONFIG}\n\
         Restart=on-failure\n\
         RestartSec=3\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

pub fn daemon_unit() -> String {
    format!(
        "[Unit]\n\
         Description=Stonewave daemon (only connects outbound, executes nothing)\n\
         After=network-online.target\n\
         Wants=network-online.target\n\
         \n\
         [Service]\n\
         User={DAEMON_USER}\n\
         Group={DAEMON_USER}\n\
         ExecStart={BIN} daemon\n\
         Restart=always\n\
         RestartSec=5\n\
         RestartPreventExitStatus=78\n\
         NoNewPrivileges=yes\n\
         CapabilityBoundingSet=\n\
         AmbientCapabilities=\n\
         ProtectSystem=strict\n\
         ReadWritePaths={VAR_LIB} {ETC_SSH}\n\
         ProtectHome=yes\n\
         PrivateTmp=yes\n\
         PrivateDevices=yes\n\
         ProtectKernelTunables=yes\n\
         ProtectKernelModules=yes\n\
         ProtectControlGroups=yes\n\
         RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX\n\
         RestrictNamespaces=yes\n\
         RestrictSUIDSGID=yes\n\
         LockPersonality=yes\n\
         SystemCallArchitectures=native\n\
         UMask=0077\n\
         \n\
         [Install]\n\
         WantedBy=multi-user.target\n"
    )
}

pub fn logrotate() -> String {
    format!(
        "{AUDIT} {{\n    weekly\n    rotate 52\n    compress\n    delaycompress\n    missingok\n    notifempty\n    create 0640 {AGENT_USER} {DAEMON_USER}\n}}\n"
    )
}

pub fn state_json(version: &str, revoked: bool) -> String {
    serde_json::json!({
        "version": version,
        "revoked": revoked,
        "agent_user": AGENT_USER,
        "agent_port": AGENT_PORT,
    })
    .to_string()
        + "\n"
}

// ------------------------------------------------------------------ steps

/// What a run of setup is asked to do.
pub struct Config<'a> {
    pub version: &'a str,
    /// Keys to add to the trust list (the list on the server is never shortened here).
    pub trust: Vec<TrustKey>,
}

pub const STEP_NAMES: [&str; 8] = [
    "Precheck",
    "Users",
    "Admin keys",
    "Agent SSH",
    "Daemon",
    "Firewall",
    "Logbook",
    "Health",
];

/// Step 0. Changes nothing. Outbound access is not needed for the local setup: the account and the
/// relay are only needed once the server connects, so a failed `outbound` is only a note here.
pub fn precheck(checks: &[Check]) -> Outcome {
    let blocking: Vec<String> = checks
        .iter()
        .filter(|c| c.level == Level::Fail && c.id != "outbound")
        .map(|c| format!("{}: {}", c.label, c.detail))
        .collect();
    if !blocking.is_empty() {
        return Outcome::Attention(blocking.join("; "));
    }
    let warns = checks
        .iter()
        .filter(|c| c.level == Level::Warn && c.id != "outbound")
        .count();
    Outcome::Kept(if warns == 0 {
        "this server can be set up".into()
    } else {
        format!("can be set up · {warns} thing(s) worth a look (stonewave doctor)")
    })
}

fn user_exists(sys: &dyn Sys, name: &str) -> bool {
    sys.run("id", &["-u", name]).ok
}

fn passwd_field(sys: &dyn Sys, name: &str, index: usize) -> Option<String> {
    let r = sys.run("getent", &["passwd", name]);
    if !r.ok {
        return None;
    }
    r.out.trim().split(':').nth(index).map(str::to_owned)
}

/// Step 1: the agent user (no password, no sudo) and the daemon user.
pub fn users(sys: &dyn Sys, dry: bool) -> R<Outcome> {
    let mut ctx = Ctx::new(sys, dry);
    if !user_exists(sys, DAEMON_USER) {
        ctx.act(
            format!("create user {DAEMON_USER} (no shell, no home, no sudo)"),
            "useradd",
            &[
                "--system",
                "--user-group",
                "--no-create-home",
                "--home-dir",
                VAR_LIB,
                "--shell",
                "/usr/sbin/nologin",
                "--comment",
                "Stonewave daemon",
                DAEMON_USER,
            ],
        )?;
    }
    if !user_exists(sys, AGENT_USER) {
        ctx.act(
            format!("create user {AGENT_USER} (no password, no sudo)"),
            "useradd",
            &[
                "--system",
                "--user-group",
                "--create-home",
                "--home-dir",
                AGENT_HOME,
                "--shell",
                "/bin/sh",
                "--comment",
                "Stonewave agent",
                AGENT_USER,
            ],
        )?;
    } else {
        // A user a revoke locked (nologin shell, expired account) is opened again by a new setup.
        if passwd_field(sys, AGENT_USER, 6).is_some_and(|s| s != "/bin/sh") {
            ctx.act(
                format!("restore the shell of {AGENT_USER} (it was locked by a revoke)"),
                "usermod",
                &["--shell", "/bin/sh", AGENT_USER],
            )?;
        }
        let chage = sys.run("chage", &["-l", AGENT_USER]);
        let expired = chage.ok
            && chage
                .out
                .lines()
                .find(|l| l.starts_with("Account expires"))
                .is_some_and(|l| !l.contains("never"));
        if expired {
            ctx.act(
                format!("clear the expiry of {AGENT_USER} (it was locked by a revoke)"),
                "usermod",
                &["--expiredate", "", AGENT_USER],
            )?;
        }
    }
    // The agent must never get power through a group, whatever else an admin did to the user.
    let groups = sys.run("id", &["-nG", AGENT_USER]);
    if groups.ok {
        let bad: Vec<&str> = groups
            .out
            .split_whitespace()
            .filter(|g| FORBIDDEN_GROUPS.contains(g))
            .collect();
        if !bad.is_empty() {
            return Ok(Outcome::Attention(format!(
                "{AGENT_USER} is in group(s) {}: it would have more than reading rights. Remove it from them",
                bad.join(", ")
            )));
        }
    }
    if user_exists(sys, AGENT_USER) || !dry {
        ctx.dir(AGENT_HOME, 0o700, (AGENT_USER, AGENT_USER))?;
    }
    Ok(finish(ctx, "users in place"))
}

/// Step 2: the trust list. Keys are added, never taken away here (that is `revoke`).
pub fn admin_keys(sys: &dyn Sys, dry: bool, wanted: &[TrustKey]) -> R<Outcome> {
    let mut ctx = Ctx::new(sys, dry);
    ctx.dir(ETC, 0o755, ("root", "root"))?;
    ctx.dir(ETC_SSH, 0o755, ("root", "root"))?;
    let existing_text = sys.read(TRUST).unwrap_or_default();
    let foreign = foreign_lines(&existing_text);
    if !foreign.is_empty() {
        return Ok(Outcome::Attention(foreign_message(&foreign)));
    }
    let mut keys = read_trust(&existing_text);
    for k in wanted {
        if keys.iter().any(|e| e.key == k.key) {
            continue;
        }
        keys.push(k.clone());
        ctx.note(format!("trust admin device \"{}\"", k.device));
    }
    ctx.file(TRUST, &render_trust(&keys), 0o644, ("root", "root"))?;
    if keys.is_empty() {
        let msg = "no admin device is trusted yet · the agent cannot log in until one is added (stonewave setup --trust-file <file>)";
        if ctx.changes.is_empty() {
            return Ok(Outcome::Kept(msg.into()));
        }
        ctx.note(format!("note: {msg}"));
    }
    Ok(finish(
        ctx,
        &format!("{} admin device(s) trusted", keys.len()),
    ))
}

fn is_active(sys: &dyn Sys, unit: &str) -> bool {
    sys.run("systemctl", &["is-active", "--quiet", unit]).ok
}

fn is_enabled(sys: &dyn Sys, unit: &str) -> bool {
    sys.run("systemctl", &["is-enabled", "--quiet", unit]).ok
}

/// Step 3: the agent sshd on 127.0.0.1, with its own host key, principals file, revocation list and unit.
pub fn agent_ssh(sys: &dyn Sys, dry: bool) -> R<Outcome> {
    let mut ctx = Ctx::new(sys, dry);
    if !sys.exists(SSHD_BIN) {
        return Ok(Outcome::Attention(
            "no OpenSSH server installed · install it first (apt install openssh-server)".into(),
        ));
    }
    ctx.dir(PRINCIPALS_DIR, 0o755, ("root", "root"))?;
    ctx.file(
        &format!("{PRINCIPALS_DIR}/{AGENT_USER}"),
        &principals(),
        0o644,
        ("root", "root"),
    )?;
    if !sys.exists(HOST_KEY) {
        ctx.act(
            "make the host key of the agent sshd (ed25519, own key, not the server's)",
            "ssh-keygen",
            &[
                "-q",
                "-t",
                "ed25519",
                "-N",
                "",
                "-C",
                "stonewave-agent-sshd",
                "-f",
                HOST_KEY,
            ],
        )?;
    }
    if !sys.exists(KRL) {
        ctx.act(
            "make an empty revocation list",
            "ssh-keygen",
            &["-q", "-k", "-f", KRL],
        )?;
    }
    // `sshd -t` needs its privilege separation folder. The server's own ssh service makes it only while it runs, and
    // our unit makes it at start (`install -d`), so a first setup has to make it for the check (found on a real Ubuntu).
    ctx.dir("/run/sshd", 0o755, ("root", "root"))?;
    let config_changed = ctx.file_checked(
        SSHD_CONFIG,
        &sshd_config(),
        0o644,
        ("root", "root"),
        &|path| sys.run(SSHD_BIN, &["-t", "-f", path]),
    )?;
    let unit_changed = ctx.file(SSHD_UNIT, &sshd_unit(), 0o644, ("root", "root"))?;
    if unit_changed {
        ctx.act("reload systemd", "systemctl", &["daemon-reload"])?;
    }
    let name = "stonewave-sshd";
    if !is_enabled(sys, name) || !is_active(sys, name) {
        if !is_enabled(sys, name) {
            ctx.note(format!("enable {name}"));
        }
        if !is_active(sys, name) {
            ctx.note(format!("start {name}"));
        }
        if !dry {
            let r = sys.run("systemctl", &["enable", "--now", name]);
            if !r.ok {
                return Err(format!("could not start {name}: {}", first_line(&r.out)));
            }
        }
    } else if config_changed || unit_changed {
        ctx.act(
            format!("restart {name} for the new configuration"),
            "systemctl",
            &["restart", name],
        )?;
    }
    Ok(finish(ctx, "agent sshd in place on 127.0.0.1:2222"))
}

/// Step 4: the daemon service. It runs from the installed binary, so that one has to exist.
pub fn daemon(sys: &dyn Sys, dry: bool, version: &str) -> R<Outcome> {
    let mut ctx = Ctx::new(sys, dry);
    if !sys.exists(BIN) {
        return Ok(Outcome::Attention(format!(
            "stonewave is not installed at {BIN} · install it with the command from Wave (install.sh), then run setup again"
        )));
    }
    ctx.dir(VAR_LIB, 0o750, (DAEMON_USER, DAEMON_USER))?;
    // The server key and the identity are made by `connect` as root; the daemon runs as its own user and must be able to read
    // them, and nobody else may.
    for file in [SERVER_KEY, SERVER_IDENTITY] {
        if sys.exists(file) && !ctx.matches(file, 0o600, (DAEMON_USER, DAEMON_USER)) {
            ctx.act(
                format!("give {file} to {DAEMON_USER} (mode 600)"),
                "chown",
                &[&format!("{DAEMON_USER}:{DAEMON_USER}"), file],
            )?;
            ctx.act(
                format!("close {file} to everybody else"),
                "chmod",
                &["0600", file],
            )?;
        }
    }
    let unit_changed = ctx.file(DAEMON_UNIT, &daemon_unit(), 0o644, ("root", "root"))?;
    if unit_changed {
        ctx.act("reload systemd", "systemctl", &["daemon-reload"])?;
    }
    // The running daemon must be the installed version.
    let installed_version = sys
        .read(STATE)
        .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
        .and_then(|v| v["version"].as_str().map(str::to_owned));
    let outdated = installed_version.as_deref() != Some(version);
    let name = "stonewaved";
    if !is_enabled(sys, name) || !is_active(sys, name) {
        if !is_enabled(sys, name) {
            ctx.note(format!("enable {name}"));
        }
        if !is_active(sys, name) {
            ctx.note(format!("start {name}"));
        }
        if !dry {
            let r = sys.run("systemctl", &["enable", "--now", name]);
            if !r.ok {
                return Err(format!("could not start {name}: {}", first_line(&r.out)));
            }
        }
    } else if unit_changed || outdated {
        ctx.act(
            format!("restart {name} (new unit or new version)"),
            "systemctl",
            &["restart", name],
        )?;
    }
    Ok(finish(ctx, "daemon in place"))
}

/// Step 5: the firewall. This build never changes it: a wrong rule can lock the admin out of their
/// own server, and the daemon needs no inbound port. It says what it found.
pub fn firewall(sys: &dyn Sys) -> Outcome {
    let ufw = sys.read("/etc/ufw/ufw.conf").unwrap_or_default();
    if ufw
        .lines()
        .any(|l| l.trim().eq_ignore_ascii_case("ENABLED=yes"))
    {
        Outcome::Kept("ufw is on · left as it is".into())
    } else {
        Outcome::Kept(
            "no active firewall seen · not changed by this build; the agent sshd listens on 127.0.0.1 only".into(),
        )
    }
}

/// Step 6: the audit log and its rotation. The log stays after a revoke.
pub fn logbook(sys: &dyn Sys, dry: bool) -> R<Outcome> {
    let mut ctx = Ctx::new(sys, dry);
    ctx.dir(VAR_LOG, 0o755, ("root", "root"))?;
    // The agent shell appends to it as its own user; the daemon reads it through the group.
    if !sys.exists(AUDIT) {
        ctx.file(AUDIT, "", 0o640, (AGENT_USER, DAEMON_USER))?;
    } else if !ctx.matches(AUDIT, 0o640, (AGENT_USER, DAEMON_USER)) {
        ctx.act(
            format!("fix rights of {AUDIT}"),
            "chown",
            &[&format!("{AGENT_USER}:{DAEMON_USER}"), AUDIT],
        )?;
        ctx.act(format!("fix mode of {AUDIT}"), "chmod", &["0640", AUDIT])?;
    }
    ctx.file(LOGROTATE, &logrotate(), 0o644, ("root", "root"))?;
    Ok(finish(ctx, "audit log in place"))
}

/// Step 7: does it work? Reads only.
pub fn health(sys: &dyn Sys, checks: &[Check]) -> Outcome {
    let mut problems = Vec::new();
    for unit in ["stonewave-sshd", "stonewaved"] {
        if !is_active(sys, unit) {
            problems.push(format!("{unit} is not running"));
        }
    }
    if !sys.listening(AGENT_PORT) {
        problems.push(format!("nothing listens on 127.0.0.1:{AGENT_PORT}"));
    }
    if !sys.run(SSHD_BIN, &["-t", "-f", SSHD_CONFIG]).ok {
        problems.push("the agent sshd configuration does not pass sshd -t".into());
    }
    for c in checks {
        if (c.id == "disk" || c.id == "clock") && c.level != Level::Ok {
            problems.push(format!("{}: {}", c.label, c.detail));
        }
    }
    if problems.is_empty() {
        Outcome::Kept("agent sshd and daemon run · disk and clock fine".into())
    } else {
        Outcome::Attention(problems.join("; "))
    }
}

/// Writes the state file at the end of a successful apply.
pub fn write_state(sys: &dyn Sys, version: &str) -> R<()> {
    let mut ctx = Ctx::new(sys, false);
    ctx.file(
        STATE,
        &state_json(version, false),
        0o640,
        (DAEMON_USER, DAEMON_USER),
    )?;
    Ok(())
}

/// One reported step.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StepReport {
    pub name: &'static str,
    pub outcome: Outcome,
}

/// Runs all steps in order. Stops at the first error (an `Attention` does not stop a dry run, but
/// a real run stops there as well: later steps depend on it).
pub fn run_steps(
    sys: &dyn Sys,
    dry: bool,
    cfg: &Config<'_>,
    checks: &[Check],
    mut on_step: impl FnMut(&StepReport),
) -> Result<Vec<StepReport>, (Vec<StepReport>, String)> {
    let mut reports: Vec<StepReport> = Vec::new();
    for (i, name) in STEP_NAMES.iter().enumerate() {
        let result = match i {
            0 => Ok(precheck(checks)),
            1 => users(sys, dry),
            2 => admin_keys(sys, dry, &cfg.trust),
            3 => agent_ssh(sys, dry),
            4 => daemon(sys, dry, cfg.version),
            5 => Ok(firewall(sys)),
            6 => logbook(sys, dry),
            _ => Ok(if dry {
                Outcome::Kept("checked after the changes (not in a dry run)".into())
            } else {
                health(sys, checks)
            }),
        };
        match result {
            Ok(outcome) => {
                let stop = matches!(outcome, Outcome::Attention(_)) && !dry && i < 7;
                let r = StepReport { name, outcome };
                on_step(&r);
                reports.push(r);
                if stop {
                    return Err((reports, format!("{name} needs a look first")));
                }
            }
            Err(e) => return Err((reports, format!("{name}: {e}"))),
        }
    }
    if !dry {
        write_state(sys, cfg.version).map_err(|e| (reports.clone(), format!("state file: {e}")))?;
    }
    Ok(reports)
}

/// Whether any step wants to change something.
pub fn has_changes(reports: &[StepReport]) -> bool {
    reports
        .iter()
        .any(|r| matches!(r.outcome, Outcome::Changed(_)))
}

// ------------------------------------------------------------------ revoke

/// The local kill switch: ends agent sessions, empties the trust list (or removes one device), stops
/// the services and locks the agent user. The audit log stays. Needs no internet.
pub fn revoke(sys: &dyn Sys, dry: bool, device: Option<&str>) -> R<Vec<StepReport>> {
    let mut reports = Vec::new();
    if let Some(name) = device {
        let mut ctx = Ctx::new(sys, dry);
        let text = sys.read(TRUST).unwrap_or_default();
        let foreign = foreign_lines(&text);
        if !foreign.is_empty() {
            return Err(foreign_message(&foreign));
        }
        let mut keys = read_trust(&text);
        let before = keys.len();
        keys.retain(|k| !k.device.eq_ignore_ascii_case(name));
        if keys.len() == before {
            return Err(format!(
                "no admin device called \"{name}\" in the trust list"
            ));
        }
        ctx.note(format!(
            "remove admin device \"{name}\" from the trust list"
        ));
        if !dry {
            ctx.file(TRUST, &render_trust(&keys), 0o644, ("root", "root"))?;
            // sshd reads the trust list on every login: nothing to restart. Sessions this device
            // signed keep running until their certificate ends (at most one hour).
        }
        audit(sys, dry, &format!("revoke-device {name}"));
        reports.push(StepReport {
            name: "Admin keys",
            outcome: Outcome::Changed(ctx.changes),
        });
        return Ok(reports);
    }

    // 1. Stop the way in first, then end what is open.
    let mut ctx = Ctx::new(sys, dry);
    for unit in ["stonewaved", "stonewave-sshd"] {
        if is_active(sys, unit) || is_enabled(sys, unit) {
            ctx.act(
                format!("stop and disable {unit}"),
                "systemctl",
                &["disable", "--now", unit],
            )?;
        }
    }
    reports.push(StepReport {
        name: "Services",
        outcome: finish(ctx, "daemon and agent sshd already stopped"),
    });

    let mut ctx = Ctx::new(sys, dry);
    if user_exists(sys, AGENT_USER) {
        let sessions = sys.run("loginctl", &["list-sessions", "--no-legend"]);
        let has_session = sessions.ok && sessions.out.contains(AGENT_USER);
        let procs = sys.run("pgrep", &["-u", AGENT_USER]);
        if has_session || (procs.ok && !procs.out.trim().is_empty()) {
            if has_session {
                ctx.act(
                    format!("end the login sessions of {AGENT_USER}"),
                    "loginctl",
                    &["terminate-user", AGENT_USER],
                )?;
            }
            // Exactly this user's processes, nothing else.
            let procs = sys.run("pgrep", &["-u", AGENT_USER]);
            if procs.ok && !procs.out.trim().is_empty() && !dry {
                let _ = sys.run("pkill", &["-KILL", "-u", AGENT_USER]);
                ctx.note(format!("end the processes of {AGENT_USER}"));
            } else if dry && procs.ok && !procs.out.trim().is_empty() {
                ctx.note(format!("end the processes of {AGENT_USER}"));
            }
        }
    }
    reports.push(StepReport {
        name: "Agent sessions",
        outcome: finish(ctx, "no agent session open"),
    });

    // 2. Empty the trust list.
    let mut ctx = Ctx::new(sys, dry);
    let keys = sys.read(TRUST).map(|t| read_trust(&t)).unwrap_or_default();
    if !keys.is_empty() {
        ctx.note(format!(
            "remove {} admin device(s) from the trust list",
            keys.len()
        ));
        if !dry {
            ctx.file(TRUST, &render_trust(&[]), 0o644, ("root", "root"))?;
        }
    }
    reports.push(StepReport {
        name: "Admin keys",
        outcome: finish(ctx, "trust list already empty"),
    });

    // 3. Lock the agent user.
    let mut ctx = Ctx::new(sys, dry);
    if user_exists(sys, AGENT_USER)
        && passwd_field(sys, AGENT_USER, 6).is_none_or(|s| s != "/usr/sbin/nologin")
    {
        ctx.act(
            format!("lock {AGENT_USER} (no shell, account expired)"),
            "usermod",
            &[
                "--shell",
                "/usr/sbin/nologin",
                "--expiredate",
                "1",
                "--lock",
                AGENT_USER,
            ],
        )?;
    }
    reports.push(StepReport {
        name: "Agent user",
        outcome: finish(ctx, "agent user already locked"),
    });

    // 4. Note it in the log, which stays.
    audit(sys, dry, "revoke-all");
    if !dry && sys.exists(STATE) {
        let mut ctx = Ctx::new(sys, false);
        let version = sys
            .read(STATE)
            .and_then(|t| serde_json::from_str::<serde_json::Value>(&t).ok())
            .and_then(|v| v["version"].as_str().map(str::to_owned))
            .unwrap_or_default();
        ctx.file(
            STATE,
            &state_json(&version, true),
            0o640,
            (DAEMON_USER, DAEMON_USER),
        )?;
    }
    reports.push(StepReport {
        name: "Audit log",
        outcome: Outcome::Kept("revoke noted · log kept".into()),
    });
    Ok(reports)
}

/// Appends one line to the audit log (best effort: a revoke must not fail because of the log).
pub fn audit(sys: &dyn Sys, dry: bool, event: &str) {
    if dry || !sys.exists(AUDIT) {
        return;
    }
    let line = serde_json::json!({ "event": event, "by": "stonewave-cli" }).to_string();
    let old = sys.read(AUDIT).unwrap_or_default();
    let mode = sys.stat(AUDIT).map_or(0o640, |s| s.mode);
    let _ = sys.write(AUDIT, &format!("{old}{line}\n"), mode);
    let _ = sys.run("chown", &[&format!("{AGENT_USER}:{DAEMON_USER}"), AUDIT]);
}

#[cfg(test)]
pub mod fake {
    //! A made-up server: files, users and services in memory.
    use super::*;
    use std::cell::RefCell;
    use std::collections::{BTreeMap, BTreeSet};

    #[derive(Default)]
    pub struct Fake {
        pub files: RefCell<BTreeMap<String, (String, u32, String, String)>>,
        pub dirs: RefCell<BTreeMap<String, (u32, String, String)>>,
        pub users: RefCell<BTreeMap<String, String>>, // name -> shell
        pub groups_of: RefCell<BTreeMap<String, String>>, // name -> "g1 g2"
        pub expired: RefCell<BTreeSet<String>>,
        pub active: RefCell<BTreeSet<String>>,
        pub enabled: RefCell<BTreeSet<String>>,
        pub log: RefCell<Vec<String>>,
        pub no_sshd: bool,
        pub bad_sshd_config: bool,
        pub no_bin: bool,
        pub sessions: RefCell<bool>,
        pub procs: RefCell<bool>,
        pub euid: u32,
    }

    impl Fake {
        pub fn new() -> Fake {
            let f = Fake::default();
            f.files.borrow_mut().insert(
                SSHD_BIN.into(),
                (String::new(), 0o755, "root".into(), "root".into()),
            );
            f.files.borrow_mut().insert(
                BIN.into(),
                (String::new(), 0o755, "root".into(), "root".into()),
            );
            f
        }
        pub fn commands(&self) -> Vec<String> {
            self.log.borrow().clone()
        }
        pub fn clear_log(&self) {
            self.log.borrow_mut().clear();
        }
        pub fn file(&self, p: &str) -> Option<String> {
            self.files.borrow().get(p).map(|f| f.0.clone())
        }
        pub fn set_file(&self, p: &str, content: &str, mode: u32, user: &str, group: &str) {
            self.files
                .borrow_mut()
                .insert(p.into(), (content.into(), mode, user.into(), group.into()));
        }
    }

    impl Sys for Fake {
        fn read(&self, path: &str) -> Option<String> {
            self.files.borrow().get(path).map(|f| f.0.clone())
        }
        fn exists(&self, path: &str) -> bool {
            if self.no_sshd && path == SSHD_BIN {
                return false;
            }
            if self.no_bin && path == BIN {
                return false;
            }
            self.files.borrow().contains_key(path) || self.dirs.borrow().contains_key(path)
        }
        fn stat(&self, path: &str) -> Option<Stat> {
            if let Some(f) = self.files.borrow().get(path) {
                return Some(Stat {
                    mode: f.1,
                    user: f.2.clone(),
                    group: f.3.clone(),
                });
            }
            self.dirs.borrow().get(path).map(|d| Stat {
                mode: d.0,
                user: d.1.clone(),
                group: d.2.clone(),
            })
        }
        fn write(&self, path: &str, content: &str, mode: u32) -> Result<(), String> {
            self.log.borrow_mut().push(format!("write {path} {mode:o}"));
            let (u, g) = self
                .files
                .borrow()
                .get(path)
                .map_or(("root".to_owned(), "root".to_owned()), |f| {
                    (f.2.clone(), f.3.clone())
                });
            self.files
                .borrow_mut()
                .insert(path.into(), (content.into(), mode, u, g));
            Ok(())
        }
        fn mkdir(&self, path: &str, mode: u32) -> Result<(), String> {
            self.log.borrow_mut().push(format!("mkdir {path} {mode:o}"));
            let (u, g) = self
                .dirs
                .borrow()
                .get(path)
                .map_or(("root".to_owned(), "root".to_owned()), |d| {
                    (d.1.clone(), d.2.clone())
                });
            self.dirs.borrow_mut().insert(path.into(), (mode, u, g));
            Ok(())
        }
        fn remove(&self, path: &str) {
            self.files.borrow_mut().remove(path);
        }
        fn listening(&self, port: u16) -> bool {
            port == AGENT_PORT && self.active.borrow().contains("stonewave-sshd")
        }
        fn euid(&self) -> u32 {
            self.euid
        }
        fn run(&self, cmd: &str, args: &[&str]) -> Run {
            let ok = |out: &str| Run {
                ok: true,
                out: out.into(),
            };
            let no = |out: &str| Run {
                ok: false,
                out: out.into(),
            };
            let readonly = matches!(cmd, "id" | "getent" | "chage" | "pgrep")
                || cmd == SSHD_BIN
                || (cmd == "systemctl" && args.first().is_some_and(|a| a.starts_with("is-")))
                || (cmd == "loginctl" && args.first() == Some(&"list-sessions"));
            if !readonly {
                self.log
                    .borrow_mut()
                    .push(format!("{cmd} {}", args.join(" ")));
            }
            match (cmd, args) {
                ("id", ["-u", name]) => {
                    if self.users.borrow().contains_key(*name) {
                        ok("1000")
                    } else {
                        no("no such user")
                    }
                }
                ("id", ["-nG", name]) => ok(self
                    .groups_of
                    .borrow()
                    .get(*name)
                    .map_or(*name, |s| s.as_str())),
                ("getent", ["passwd", name]) => match self.users.borrow().get(*name) {
                    Some(shell) => ok(&format!("{name}:x:999:999::/home/{name}:{shell}")),
                    None => no(""),
                },
                ("chage", ["-l", name]) => ok(&format!(
                    "Account expires\t\t\t\t\t\t: {}\n",
                    if self.expired.borrow().contains(*name) {
                        "Jan 02, 1970"
                    } else {
                        "never"
                    }
                )),
                ("useradd", rest) => {
                    let name = rest.last().unwrap();
                    let shell = rest
                        .iter()
                        .position(|a| *a == "--shell")
                        .map_or("/bin/sh", |i| rest[i + 1]);
                    self.users.borrow_mut().insert((*name).into(), shell.into());
                    ok("")
                }
                ("usermod", rest) => {
                    let name = rest.last().unwrap();
                    if let Some(i) = rest.iter().position(|a| *a == "--shell") {
                        self.users
                            .borrow_mut()
                            .insert((*name).into(), rest[i + 1].into());
                    }
                    if let Some(i) = rest.iter().position(|a| *a == "--expiredate") {
                        if rest[i + 1].is_empty() {
                            self.expired.borrow_mut().remove(*name);
                        } else {
                            self.expired.borrow_mut().insert((*name).into());
                        }
                    }
                    ok("")
                }
                ("chown", [who, path]) => {
                    let (u, g) = who.split_once(':').unwrap();
                    if let Some(f) = self.files.borrow_mut().get_mut(*path) {
                        f.2 = u.into();
                        f.3 = g.into();
                    }
                    if let Some(d) = self.dirs.borrow_mut().get_mut(*path) {
                        d.1 = u.into();
                        d.2 = g.into();
                    }
                    ok("")
                }
                ("chmod", [mode, path]) => {
                    let m = u32::from_str_radix(mode, 8).unwrap();
                    if let Some(f) = self.files.borrow_mut().get_mut(*path) {
                        f.1 = m;
                    }
                    ok("")
                }
                ("ssh-keygen", rest) => {
                    let path = rest.last().unwrap();
                    self.set_file(path, "KEY", 0o600, "root", "root");
                    ok("")
                }
                (c, ["-t", "-f", path]) if c == SSHD_BIN => {
                    if self.bad_sshd_config || self.file(path).is_none() {
                        no("Bad configuration option")
                    } else {
                        ok("")
                    }
                }
                ("systemctl", ["is-active", "--quiet", unit]) => {
                    if self.active.borrow().contains(*unit) {
                        ok("")
                    } else {
                        no("")
                    }
                }
                ("systemctl", ["is-enabled", "--quiet", unit]) => {
                    if self.enabled.borrow().contains(*unit) {
                        ok("")
                    } else {
                        no("")
                    }
                }
                ("systemctl", ["enable", "--now", unit]) => {
                    self.enabled.borrow_mut().insert((*unit).into());
                    self.active.borrow_mut().insert((*unit).into());
                    ok("")
                }
                ("systemctl", ["disable", "--now", unit]) => {
                    self.enabled.borrow_mut().remove(*unit);
                    self.active.borrow_mut().remove(*unit);
                    ok("")
                }
                ("systemctl", _) => ok(""),
                ("loginctl", ["list-sessions", ..]) => {
                    if *self.sessions.borrow() {
                        ok("3 1000 stonewave-agent seat0")
                    } else {
                        ok("")
                    }
                }
                ("loginctl", ["terminate-user", _]) => {
                    *self.sessions.borrow_mut() = false;
                    *self.procs.borrow_mut() = false;
                    ok("")
                }
                ("pgrep", _) => {
                    if *self.procs.borrow() {
                        ok("4242\n")
                    } else {
                        no("")
                    }
                }
                ("pkill", _) => {
                    *self.procs.borrow_mut() = false;
                    ok("")
                }
                _ => ok(""),
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::fake::Fake;
    use super::*;
    use crate::doctor::{Check, Level};

    // A real-looking ed25519 public key blob: length-prefixed "ssh-ed25519" and 32 bytes.
    fn ed25519_line(seed: u8, name: &str) -> String {
        let mut blob = Vec::new();
        blob.extend_from_slice(&11u32.to_be_bytes());
        blob.extend_from_slice(b"ssh-ed25519");
        blob.extend_from_slice(&32u32.to_be_bytes());
        blob.extend_from_slice(&[seed; 32]);
        format!("ssh-ed25519 {} {name}", b64::encode(&blob))
    }

    fn checks(level: Level) -> Vec<Check> {
        vec![
            Check {
                id: "os",
                label: "System",
                level,
                detail: "x".into(),
            },
            Check {
                id: "outbound",
                label: "Outbound",
                level: Level::Fail,
                detail: "no".into(),
            },
            Check {
                id: "disk",
                label: "Disk",
                level: Level::Ok,
                detail: "ok".into(),
            },
            Check {
                id: "clock",
                label: "Clock",
                level: Level::Ok,
                detail: "ok".into(),
            },
        ]
    }

    fn cfg(trust: Vec<TrustKey>) -> Config<'static> {
        Config {
            version: "0.2.0",
            trust,
        }
    }

    fn run(fake: &Fake, dry: bool, trust: Vec<TrustKey>) -> Vec<StepReport> {
        run_steps(fake, dry, &cfg(trust), &checks(Level::Ok), |_| {}).expect("setup runs")
    }

    #[test]
    fn trust_file_accepts_good_keys_and_names_devices() {
        let text = format!(
            "# admin devices\n\n{}\n{}\n",
            ed25519_line(1, "Office PC"),
            ed25519_line(2, "")
        );
        let keys = parse_trust_file(&text).unwrap();
        assert_eq!(keys.len(), 2);
        assert_eq!(keys[0].device, "Office PC");
        assert_eq!(keys[1].device, "device-2", "a missing name gets a number");
    }

    #[test]
    fn trust_file_refuses_what_is_not_a_plain_key() {
        let good = ed25519_line(1, "x");
        let key = good.split(' ').nth(1).unwrap().to_owned();
        for (line, why) in [
            (format!("ssh-rsa {key} x"), "key type"),
            (
                format!("ssh-ed25519-cert-v01@openssh.com {key} x"),
                "certificate",
            ),
            (
                format!("command=\"rm -rf /\" ssh-ed25519 {key} x"),
                "options in front",
            ),
            ("ssh-ed25519 !!!notbase64 x".to_owned(), "base64"),
            (
                format!("ecdsa-sha2-nistp256 {key} x"),
                "key that does not match its type",
            ),
            ("ssh-ed25519".to_owned(), "no key"),
        ] {
            assert!(parse_trust_file(&line).is_err(), "{why}: {line}");
        }
    }

    #[test]
    fn device_names_cannot_smuggle_a_second_line_or_options() {
        let line = ed25519_line(1, "evil\" ; rm -rf /");
        let k = parse_key_line(&line, "d").unwrap();
        assert!(!k.device.contains(['"', ';', '\n']));
        assert!(render_key(&k).lines().count() == 1);
    }

    #[test]
    fn first_run_sets_everything_up() {
        let fake = Fake::new();
        let reports = run(
            &fake,
            false,
            vec![parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap()],
        );
        assert_eq!(reports.len(), 8);
        assert!(fake.users.borrow().contains_key(AGENT_USER));
        assert!(fake.users.borrow().contains_key(DAEMON_USER));
        assert!(fake.active.borrow().contains("stonewave-sshd"));
        assert!(fake.active.borrow().contains("stonewaved"));
        assert!(
            fake.file(TRUST)
                .unwrap()
                .contains("stonewave-device:Office PC")
        );
        assert_eq!(fake.file(SSHD_CONFIG).unwrap(), sshd_config());
        assert!(fake.file(HOST_KEY).is_some());
        assert!(fake.file(KRL).is_some());
        assert!(fake.file(STATE).unwrap().contains("\"revoked\":false"));
        let st = fake.stat(AUDIT).unwrap();
        assert_eq!(
            (st.mode, st.user.as_str(), st.group.as_str()),
            (0o640, AGENT_USER, DAEMON_USER)
        );
        // The agent user is never made a member of a group with power.
        assert!(!fake.commands().iter().any(|c| c.contains("sudo")));
    }

    #[test]
    fn second_run_changes_nothing() {
        let fake = Fake::new();
        let trust = vec![parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap()];
        run(&fake, false, trust.clone());
        fake.clear_log();
        let again = run(&fake, false, trust);
        assert!(!has_changes(&again), "second run: {again:#?}");
        assert!(
            fake.commands().is_empty(),
            "second run touched the machine: {:?}",
            fake.commands()
        );
    }

    #[test]
    fn dry_run_touches_nothing_and_matches_the_real_plan() {
        let fake = Fake::new();
        let trust = vec![parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap()];
        let plan = run(&fake, true, trust.clone());
        assert!(has_changes(&plan));
        assert!(
            fake.commands().is_empty(),
            "dry run did something: {:?}",
            fake.commands()
        );
        assert!(fake.users.borrow().is_empty());
        assert!(fake.file(STATE).is_none());
        let planned: Vec<String> = plan
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Changed(_)))
            .map(|r| r.name.to_owned())
            .collect();
        let real = run(&fake, false, trust);
        let done: Vec<String> = real
            .iter()
            .filter(|r| matches!(r.outcome, Outcome::Changed(_)))
            .map(|r| r.name.to_owned())
            .collect();
        assert_eq!(planned, done, "the plan names the steps that then change");
    }

    #[test]
    fn trust_list_only_grows_and_keeps_existing_names() {
        let fake = Fake::new();
        let a = parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap();
        let b = parse_key_line(&ed25519_line(2, "iPhone"), "d").unwrap();
        run(&fake, false, vec![a.clone()]);
        run(&fake, false, vec![b.clone()]);
        let list = read_trust(&fake.file(TRUST).unwrap());
        assert_eq!(list.len(), 2);
        // The same key under another name does not rename or duplicate it.
        let a2 = TrustKey {
            device: "other".into(),
            ..a.clone()
        };
        run(&fake, false, vec![a2]);
        let list = read_trust(&fake.file(TRUST).unwrap());
        assert_eq!(list.iter().filter(|k| k.key == a.key).count(), 1);
        assert_eq!(list[0].device, "Office PC");
    }

    #[test]
    fn without_a_trusted_device_setup_says_so() {
        let fake = Fake::new();
        let r = run(&fake, false, vec![]);
        let step = r.iter().find(|r| r.name == "Admin keys").unwrap();
        match &step.outcome {
            Outcome::Changed(c) => assert!(
                c.iter()
                    .any(|c| c.0.contains("no admin device is trusted yet"))
            ),
            other => panic!("{other:?}"),
        }
    }

    #[test]
    fn a_trust_file_with_lines_we_do_not_know_is_never_rewritten() {
        let fake = Fake::new();
        let a = parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap();
        run(&fake, false, vec![a.clone()]);
        let hand = format!(
            "{}ssh-rsa AAAAB3NzaC1yc2E= some-old-ca\n",
            fake.file(TRUST).unwrap()
        );
        fake.set_file(TRUST, &hand, 0o644, "root", "root");
        assert_eq!(foreign_lines(&hand), vec![4]);
        let err = run_steps(&fake, false, &cfg(vec![a]), &checks(Level::Ok), |_| {}).unwrap_err();
        assert!(err.1.contains("Admin keys"), "{err:?}");
        assert_eq!(
            fake.file(TRUST).unwrap(),
            hand,
            "the file stays as the admin left it"
        );
        assert!(revoke(&fake, false, Some("Office PC")).is_err());
        assert_eq!(fake.file(TRUST).unwrap(), hand);
    }

    #[test]
    fn the_agent_user_may_not_sit_in_powerful_groups() {
        let fake = Fake::new();
        run(&fake, false, vec![]);
        fake.groups_of
            .borrow_mut()
            .insert(AGENT_USER.into(), "stonewave-agent sudo".into());
        let err = run_steps(&fake, false, &cfg(vec![]), &checks(Level::Ok), |_| {});
        match err {
            Err((_, msg)) => assert!(msg.contains("Users"), "{msg}"),
            Ok(_) => panic!("must stop"),
        }
    }

    #[test]
    fn a_broken_sshd_config_never_replaces_a_working_one() {
        let fake = Fake::new();
        run(&fake, false, vec![]);
        let before = fake.file(SSHD_CONFIG).unwrap();
        // Someone edited the file by hand, and now the check of the new text fails.
        fake.set_file(SSHD_CONFIG, "Port 22\n", 0o644, "root", "root");
        let mut broken = Fake::new();
        broken.bad_sshd_config = true;
        broken.set_file(SSHD_CONFIG, &before, 0o644, "root", "root");
        let mut c = Ctx::new(&broken, false);
        let r = c.file_checked(SSHD_CONFIG, "new text", 0o644, ("root", "root"), &|p| {
            broken.run(SSHD_BIN, &["-t", "-f", p])
        });
        assert!(r.is_err());
        assert_eq!(
            broken.file(SSHD_CONFIG).unwrap(),
            before,
            "the old file stays"
        );
        assert!(
            broken.file(&format!("{SSHD_CONFIG}.new")).is_none(),
            "no candidate left behind"
        );
    }

    #[test]
    fn missing_pieces_are_reported_not_forced() {
        let mut fake = Fake::new();
        fake.no_sshd = true;
        let err = run_steps(&fake, false, &cfg(vec![]), &checks(Level::Ok), |_| {}).unwrap_err();
        assert!(err.1.contains("Agent SSH"));
        let mut fake = Fake::new();
        fake.no_bin = true;
        let err = run_steps(&fake, false, &cfg(vec![]), &checks(Level::Ok), |_| {}).unwrap_err();
        assert!(err.1.contains("Daemon"));
        assert!(!fake.active.borrow().contains("stonewaved"));
    }

    #[test]
    fn precheck_stops_on_a_failed_check_but_not_on_outbound() {
        assert!(matches!(precheck(&checks(Level::Ok)), Outcome::Kept(_)));
        assert!(matches!(
            precheck(&checks(Level::Fail)),
            Outcome::Attention(_)
        ));
    }

    #[test]
    fn a_new_version_restarts_the_daemon() {
        let fake = Fake::new();
        run(&fake, false, vec![]);
        fake.clear_log();
        let mut c = cfg(vec![]);
        c.version = "0.3.0";
        run_steps(&fake, false, &c, &checks(Level::Ok), |_| {}).unwrap();
        assert!(
            fake.commands()
                .iter()
                .any(|c| c == "systemctl restart stonewaved")
        );
    }

    #[test]
    fn revoke_ends_everything_and_keeps_the_log() {
        let fake = Fake::new();
        run(
            &fake,
            false,
            vec![parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap()],
        );
        *fake.sessions.borrow_mut() = true;
        *fake.procs.borrow_mut() = true;
        let r = revoke(&fake, false, None).unwrap();
        assert!(
            r.iter()
                .all(|s| !matches!(s.outcome, Outcome::Attention(_)))
        );
        assert!(read_trust(&fake.file(TRUST).unwrap()).is_empty());
        assert!(!fake.active.borrow().contains("stonewaved"));
        assert!(!fake.active.borrow().contains("stonewave-sshd"));
        assert_eq!(fake.users.borrow()[AGENT_USER], "/usr/sbin/nologin");
        assert!(fake.expired.borrow().contains(AGENT_USER));
        assert!(!*fake.procs.borrow());
        assert!(
            fake.file(AUDIT).unwrap().contains("revoke-all"),
            "the log keeps the revoke"
        );
        assert!(fake.file(STATE).unwrap().contains("\"revoked\":true"));
        // Only this user's processes are ended, with exactly this user's name.
        let kills: Vec<String> = fake
            .commands()
            .into_iter()
            .filter(|c| c.starts_with("pkill"))
            .collect();
        assert!(
            kills.iter().all(|k| k == "pkill -KILL -u stonewave-agent"),
            "{kills:?}"
        );
    }

    #[test]
    fn revoke_twice_is_fine_and_setup_afterwards_opens_the_user_again() {
        let fake = Fake::new();
        let trust = vec![parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap()];
        run(&fake, false, trust.clone());
        revoke(&fake, false, None).unwrap();
        let again = revoke(&fake, false, None).unwrap();
        assert!(!has_changes(&again), "second revoke: {again:#?}");
        run(&fake, false, trust);
        assert_eq!(fake.users.borrow()[AGENT_USER], "/bin/sh");
        assert!(!fake.expired.borrow().contains(AGENT_USER));
        assert!(fake.active.borrow().contains("stonewave-sshd"));
    }

    #[test]
    fn revoke_one_device_leaves_the_others() {
        let fake = Fake::new();
        let a = parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap();
        let b = parse_key_line(&ed25519_line(2, "iPhone"), "d").unwrap();
        run(&fake, false, vec![a.clone(), b.clone()]);
        revoke(&fake, false, Some("office pc")).unwrap();
        let list = read_trust(&fake.file(TRUST).unwrap());
        assert_eq!(list.len(), 1);
        assert_eq!(list[0].key, b.key);
        assert!(
            fake.active.borrow().contains("stonewave-sshd"),
            "services keep running"
        );
        assert!(revoke(&fake, false, Some("nobody")).is_err());
    }

    #[test]
    fn revoke_dry_run_changes_nothing() {
        let fake = Fake::new();
        run(
            &fake,
            false,
            vec![parse_key_line(&ed25519_line(1, "Office PC"), "d").unwrap()],
        );
        fake.clear_log();
        *fake.procs.borrow_mut() = true;
        let plan = revoke(&fake, true, None).unwrap();
        assert!(has_changes(&plan));
        assert!(fake.commands().is_empty(), "{:?}", fake.commands());
        assert_eq!(read_trust(&fake.file(TRUST).unwrap()).len(), 1);
    }

    #[test]
    fn generated_files_keep_the_promises_of_the_concept() {
        let c = sshd_config();
        for must in [
            "ListenAddress 127.0.0.1",
            "AuthenticationMethods publickey",
            "PasswordAuthentication no",
            "ExposeAuthInfo yes",
            "AllowTcpForwarding no",
            "PermitTTY no",
            "ForceCommand /usr/local/bin/stonewave agent-shell",
            "AllowUsers stonewave-agent",
            "RevokedKeys",
            "LogLevel VERBOSE",
        ] {
            assert!(c.contains(must), "sshd_config lacks {must}");
        }
        let u = daemon_unit();
        for must in [
            "NoNewPrivileges=yes",
            "CapabilityBoundingSet=\n",
            "ProtectSystem=strict",
            "RestrictAddressFamilies=AF_INET AF_INET6 AF_UNIX",
            "User=stonewave\n",
            "RestartPreventExitStatus=78\n",
        ] {
            assert!(u.contains(must), "daemon unit lacks {must}");
        }
        assert!(
            !principals().contains("sw.shell") && !principals().contains("restart"),
            "reading rights only"
        );
        assert!(
            sshd_unit().contains("install -d -m 0755 /run/sshd"),
            "never removes the server's own /run/sshd"
        );
    }
}
