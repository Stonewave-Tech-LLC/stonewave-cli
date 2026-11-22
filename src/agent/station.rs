//! The wake service ("station"): a small background program that asks Wave what was addressed to the agent and writes one line per
//! new thing to a log file. A running session follows that file and wakes up on each line.
//!
//! - the script `fleet-watch.mjs` is part of this program (`include_str!`) and needs Node 18 or newer
//! - it runs under the current user, never as root: systemd `--user` (Linux), a LaunchAgent (macOS), a scheduled task at logon (Windows)
//! - its output goes to `~/.local/state/wave/<agent>.wake.log` (Windows `%LOCALAPPDATA%\wave`), 0600, cut to the last 1000 lines
//!   whenever the service is installed again
//! - the key is not in any service file: the script reads it from the key file named by `WAVE_AGENT_KEY_FILE`
//!
//! The text of each service file comes from a pure function (tested on every OS); installing goes through [`Exec`].

use std::fs;
use std::io::{self, Write};
use std::path::{Path, PathBuf};

use super::dirs::{Dirs, Os, text};
use super::exec::Exec;
use super::key::{Acl, atomic_write, ensure_private_dir, plain_path, scrub};
use super::state::ServiceRec;

pub const SCRIPT: &str = include_str!("../../assets/fleet-watch.mjs");
pub const MIN_NODE_MAJOR: u32 = 18;
pub const LOG_LINES: usize = 1000;
/// The most of an old log that is looked at when it is cut.
const LOG_READ_MAX: u64 = 8 * 1024 * 1024;

pub fn unit_name(agent: &str) -> String {
    format!("stonewave-wave-{agent}.service")
}

pub fn launchd_label(agent: &str) -> String {
    format!("tech.stonewave.wave.{agent}")
}

pub fn task_name(agent: &str) -> String {
    format!("StonewaveWave-{agent}")
}

/// What a service needs to know.
pub struct Unit<'a> {
    pub agent: &'a str,
    pub node: &'a str,
    pub script: &'a str,
    pub mcp_url: &'a str,
    pub key_file: &'a str,
    pub log: &'a str,
}

// ------------------------------------------------------------------ systemd

/// A word of an `ExecStart=` line: double quotes, `\` and `"` escaped, `%` (specifiers) and `$` (variables) doubled.
pub fn systemd_exec_word(s: &str) -> String {
    let mut o = String::from("\"");
    for c in s.chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '%' => o.push_str("%%"),
            '$' => o.push_str("$$"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A whole `Environment=` assignment in double quotes (no variable expansion happens in there, only `%` specifiers).
pub fn systemd_env(name: &str, value: &str) -> String {
    let mut o = String::from("\"");
    for c in format!("{name}={value}").chars() {
        match c {
            '\\' => o.push_str("\\\\"),
            '"' => o.push_str("\\\""),
            '%' => o.push_str("%%"),
            c => o.push(c),
        }
    }
    o.push('"');
    o
}

/// A path after `append:`: no quoting exists there, only `%` is doubled.
fn systemd_path(s: &str) -> String {
    s.replace('%', "%%")
}

pub fn systemd_unit(u: &Unit<'_>) -> String {
    format!(
        "[Unit]\n\
Description=Stonewave wake service for the Wave agent {agent}\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
Restart=always\n\
RestartSec=20\n\
Environment={url}\n\
Environment={keyfile}\n\
ExecStart={node} {script} --session {session}\n\
StandardOutput=append:{log}\n\
StandardError=append:{log}\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        agent = u.agent,
        url = systemd_env("WAVE_MCP_URL", u.mcp_url),
        keyfile = systemd_env("WAVE_AGENT_KEY_FILE", u.key_file),
        node = systemd_exec_word(u.node),
        script = systemd_exec_word(u.script),
        session = systemd_exec_word(u.agent),
        log = systemd_path(u.log),
    )
}

// ------------------------------------------------------------------ launchd

pub fn xml_escape(s: &str) -> String {
    let mut o = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '&' => o.push_str("&amp;"),
            '<' => o.push_str("&lt;"),
            '>' => o.push_str("&gt;"),
            '"' => o.push_str("&quot;"),
            '\'' => o.push_str("&apos;"),
            c => o.push(c),
        }
    }
    o
}

pub fn launchd_plist(u: &Unit<'_>) -> String {
    let s = |v: &str| format!("<string>{}</string>", xml_escape(v));
    let mut p = String::new();
    p.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    p.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    p.push_str("<plist version=\"1.0\">\n<dict>\n");
    p.push_str(&format!(
        "  <key>Label</key>\n  {}\n",
        s(&launchd_label(u.agent))
    ));
    p.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for a in [u.node, u.script, "--session", u.agent] {
        p.push_str(&format!("    {}\n", s(a)));
    }
    p.push_str("  </array>\n");
    p.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
    p.push_str(&format!(
        "    <key>WAVE_MCP_URL</key>\n    {}\n",
        s(u.mcp_url)
    ));
    p.push_str(&format!(
        "    <key>WAVE_AGENT_KEY_FILE</key>\n    {}\n",
        s(u.key_file)
    ));
    p.push_str("  </dict>\n");
    p.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    p.push_str("  <key>KeepAlive</key>\n  <true/>\n");
    p.push_str("  <key>ThrottleInterval</key>\n  <integer>20</integer>\n");
    p.push_str(&format!("  <key>StandardOutPath</key>\n  {}\n", s(u.log)));
    p.push_str(&format!("  <key>StandardErrorPath</key>\n  {}\n", s(u.log)));
    p.push_str("</dict>\n</plist>\n");
    p
}

// ------------------------------------------------------------------ Windows

/// A value inside double quotes in a batch file: `%` is doubled (it expands even inside quotes). A double quote cannot be
/// escaped there, so values with one are refused before this is called (see `plain_path`).
fn cmd_quoted(s: &str) -> String {
    s.replace('%', "%%")
}

pub fn windows_wrapper(u: &Unit<'_>) -> String {
    let log = cmd_quoted(u.log);
    format!(
        "@echo off\r\n\
rem Wake service of the Wave agent {agent}, made by stonewave. Runs at logon, output goes to the wake log.\r\n\
set \"WAVE_MCP_URL={url}\"\r\n\
set \"WAVE_AGENT_KEY_FILE={keyfile}\"\r\n\
:run\r\n\
\"{node}\" \"{script}\" --session \"{session}\" >> \"{log}\" 2>&1\r\n\
ping -n 21 127.0.0.1 >nul\r\n\
goto run\r\n",
        agent = u.agent,
        url = cmd_quoted(u.mcp_url),
        keyfile = cmd_quoted(u.key_file),
        node = cmd_quoted(u.node),
        script = cmd_quoted(u.script),
        session = cmd_quoted(u.agent),
    )
}

/// The task definition: at logon of this user, with the rights of this user, no time limit, no window (`conhost --headless`).
/// `schtasks /Create /SC ONLOGON` needs an elevated shell, a task made from a definition like this one does not.
pub fn windows_task_xml(user: &str, wrapper: &str) -> String {
    let (user, wrapper) = (xml_escape(user), xml_escape(wrapper));
    format!(
        "<?xml version=\"1.0\" encoding=\"UTF-16\"?>\r\n\
<Task version=\"1.2\" xmlns=\"http://schemas.microsoft.com/windows/2004/02/mit/task\">\r\n\
  <Triggers>\r\n    <LogonTrigger>\r\n      <Enabled>true</Enabled>\r\n      <UserId>{user}</UserId>\r\n    </LogonTrigger>\r\n  </Triggers>\r\n\
  <Principals>\r\n    <Principal id=\"Author\">\r\n      <UserId>{user}</UserId>\r\n      <LogonType>InteractiveToken</LogonType>\r\n      <RunLevel>LeastPrivilege</RunLevel>\r\n    </Principal>\r\n  </Principals>\r\n\
  <Settings>\r\n    <MultipleInstancesPolicy>IgnoreNew</MultipleInstancesPolicy>\r\n    <DisallowStartIfOnBatteries>false</DisallowStartIfOnBatteries>\r\n    <StopIfGoingOnBatteries>false</StopIfGoingOnBatteries>\r\n    <StartWhenAvailable>true</StartWhenAvailable>\r\n    <ExecutionTimeLimit>PT0S</ExecutionTimeLimit>\r\n    <Enabled>true</Enabled>\r\n  </Settings>\r\n\
  <Actions Context=\"Author\">\r\n    <Exec>\r\n      <Command>conhost.exe</Command>\r\n      <Arguments>--headless cmd.exe /c \"{wrapper}\"</Arguments>\r\n    </Exec>\r\n  </Actions>\r\n\
</Task>\r\n"
    )
}

/// PowerShell that stops the programs a stopped task leaves behind: `schtasks /End` ends the task's first process only, so the
/// batch file and node would go on. Only processes of this one service are named: the batch file's own path, and this
/// script with this session name.
pub fn windows_stop_script(wrapper: &str, agent: &str) -> String {
    let q = |s: &str| s.replace('\'', "''");
    format!(
        "$w = '{}'; $s = '--session \"{}\"'; Get-CimInstance Win32_Process | Where-Object {{ $_.ProcessId -ne $PID -and $_.CommandLine -and ($_.CommandLine.Contains($w) -or ($_.CommandLine.Contains('fleet-watch.mjs') -and $_.CommandLine.Contains($s))) }} | ForEach-Object {{ Stop-Process -Id $_.ProcessId -Force -ErrorAction SilentlyContinue }}",
        q(wrapper),
        q(agent)
    )
}

/// Ends the task and what it started.
fn windows_stop(rec: &ServiceRec, exec: &dyn Exec) {
    let _ = exec.run(
        "schtasks",
        &["/End".to_owned(), "/TN".to_owned(), rec.name.clone()],
    );
    let agent = rec.name.strip_prefix("StonewaveWave-").unwrap_or("");
    if let (Some(wrapper), false) = (rec.files.first(), agent.is_empty()) {
        let _ = exec.run(
            "powershell",
            &[
                "-NoProfile".to_owned(),
                "-NonInteractive".to_owned(),
                "-Command".to_owned(),
                windows_stop_script(wrapper, agent),
            ],
        );
    }
}

/// What `schtasks /XML` reads: UTF-16 little endian with a byte order mark.
pub fn utf16le_with_bom(s: &str) -> Vec<u8> {
    let mut b = vec![0xFF, 0xFE];
    for u in s.encode_utf16() {
        b.extend_from_slice(&u.to_le_bytes());
    }
    b
}

pub fn schtasks_create_args(task: &str, xml_file: &str) -> Vec<String> {
    vec![
        "/Create".into(),
        "/TN".into(),
        task.into(),
        "/XML".into(),
        xml_file.into(),
        "/F".into(),
    ]
}

// ------------------------------------------------------------------ helpers

/// `v18.19.0` -> 18.
pub fn node_major(version_output: &str) -> Option<u32> {
    version_output
        .trim()
        .strip_prefix('v')?
        .split('.')
        .next()?
        .parse()
        .ok()
}

/// The last `keep` lines, or None when the text is not longer than that.
pub fn trim_lines(text: &str, keep: usize) -> Option<String> {
    let lines: Vec<&str> = text.split_inclusive('\n').collect();
    if lines.len() <= keep {
        return None;
    }
    Some(lines[lines.len() - keep..].concat())
}

/// The command that follows the log, for a person to copy (or a session to put into a monitor).
pub fn follow_command(os: Os, log: &str) -> String {
    if os == Os::Windows {
        format!(
            "Get-Content -Wait -Tail 0 -LiteralPath '{}'",
            log.replace('\'', "''")
        )
    } else {
        format!("tail -n 0 -F '{}'", log.replace('\'', "'\\''"))
    }
}

/// Node 18 or newer: its absolute path, or why not.
pub fn find_node(exec: &dyn Exec) -> Result<PathBuf, String> {
    let path = exec
        .which("node")
        .ok_or_else(|| "Node.js (version 18 or newer) was not found on your PATH".to_owned())?;
    let out = exec
        .run(&text(&path), &["--version".to_owned()])
        .map_err(|e| format!("cannot run node: {}", scrub(&e.to_string())))?;
    match node_major(&out.stdout) {
        Some(m) if m >= MIN_NODE_MAJOR => Ok(path),
        Some(m) => Err(format!(
            "Node.js {m} is too old (version 18 or newer is needed)"
        )),
        None => Err("could not tell which version of Node.js this is".to_owned()),
    }
}

fn uid() -> u32 {
    #[cfg(unix)]
    {
        // SAFETY: getuid has no arguments and cannot fail.
        unsafe { libc::getuid() }
    }
    #[cfg(not(unix))]
    {
        0
    }
}

// ------------------------------------------------------------------ files

/// Makes the wake log (private, appendable) and cuts it to the last [`LOG_LINES`] lines.
pub fn prepare_log(dirs: &Dirs, agent: &str, acl: &Acl<'_>) -> io::Result<PathBuf> {
    ensure_private_dir(&dirs.state, acl)?;
    let log = dirs.wake_log(agent);
    {
        let mut o = fs::OpenOptions::new();
        o.create(true).append(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            o.mode(0o600).custom_flags(libc::O_NOFOLLOW);
        }
        o.open(&log)?.flush()?;
    }
    let len = fs::metadata(&log)?.len();
    let bytes = if len > LOG_READ_MAX {
        use std::io::{Read, Seek, SeekFrom};
        let mut f = fs::File::open(&log)?;
        f.seek(SeekFrom::Start(len - LOG_READ_MAX))?;
        let mut b = Vec::new();
        f.read_to_end(&mut b)?;
        b
    } else {
        fs::read(&log)?
    };
    let body = String::from_utf8_lossy(&bytes).into_owned();
    // a cut in the middle of a line leaves a partial first line: drop it
    let body = if len > LOG_READ_MAX {
        body.split_once('\n')
            .map_or(String::new(), |x| x.1.to_owned())
    } else {
        body
    };
    let trimmed = trim_lines(&body, LOG_LINES);
    if let Some(new) = trimmed {
        atomic_write(&log, new.as_bytes(), 0o600)?;
    } else if len > LOG_READ_MAX {
        atomic_write(&log, body.as_bytes(), 0o600)?;
    }
    Ok(log)
}

pub fn write_script(dirs: &Dirs) -> io::Result<PathBuf> {
    let p = dirs.wake_script();
    atomic_write(&p, SCRIPT.as_bytes(), 0o644)?;
    Ok(p)
}

/// The result of installing: what was written (always recorded) and what went wrong, if anything.
pub struct Installed {
    pub rec: ServiceRec,
    pub problem: Option<String>,
}

pub struct Request<'a> {
    pub agent: &'a str,
    pub mcp_url: &'a str,
    pub node: &'a Path,
    pub dirs: &'a Dirs,
    pub exec: &'a dyn Exec,
}

fn run_ok(exec: &dyn Exec, program: &str, args: &[&str]) -> Result<(), String> {
    let args: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    match exec.run(program, &args) {
        Ok(o) if o.ok => Ok(()),
        Ok(o) => Err(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            scrub(&o.problem())
        )),
        Err(e) => Err(format!("cannot run {program}: {}", scrub(&e.to_string()))),
    }
}

/// Writes the script and the service file(s) and starts the service. The key file must exist already (the script reads it).
pub fn install(r: &Request<'_>, acl: &Acl<'_>) -> Installed {
    let mut rec = ServiceRec {
        kind: String::new(),
        name: String::new(),
        files: Vec::new(),
    };
    let result = install_inner(r, acl, &mut rec);
    Installed {
        rec,
        problem: result.err(),
    }
}

fn install_inner(r: &Request<'_>, acl: &Acl<'_>, rec: &mut ServiceRec) -> Result<(), String> {
    let dirs = r.dirs;
    let script = write_script(dirs)
        .map_err(|e| format!("cannot write the wake script: {}", scrub(&e.to_string())))?;
    let log = prepare_log(dirs, r.agent, acl)
        .map_err(|e| format!("cannot prepare the wake log: {}", scrub(&e.to_string())))?;
    let key_file = dirs.key_file(r.agent);
    let paths = [script.as_path(), log.as_path(), key_file.as_path(), r.node];
    for p in paths {
        plain_path(p)?;
    }
    plain_path(&dirs.config)?;
    let (script_s, log_s, key_s, node_s) =
        (text(&script), text(&log), text(&key_file), text(r.node));
    let unit = Unit {
        agent: r.agent,
        node: &node_s,
        script: &script_s,
        mcp_url: r.mcp_url,
        key_file: &key_s,
        log: &log_s,
    };
    match dirs.os {
        Os::Linux => {
            let name = unit_name(r.agent);
            let file = dirs.systemd_user.join(&name);
            plain_path(&file)?;
            let existed = file.exists();
            fs::create_dir_all(&dirs.systemd_user)
                .map_err(|e| format!("cannot make {}: {e}", dirs.systemd_user.display()))?;
            atomic_write(&file, systemd_unit(&unit).as_bytes(), 0o644).map_err(|e| {
                format!("cannot write {}: {}", file.display(), scrub(&e.to_string()))
            })?;
            rec.kind = "systemd".into();
            rec.name = name.clone();
            rec.files.push(text(&file));
            run_ok(r.exec, "systemctl", &["--user", "daemon-reload"])?;
            run_ok(r.exec, "systemctl", &["--user", "enable", "--now", &name])?;
            if existed {
                // the file changed under a running service: make it read the new one
                run_ok(r.exec, "systemctl", &["--user", "restart", &name])?;
            }
        }
        Os::Mac => {
            let label = launchd_label(r.agent);
            let file = dirs.launch_agents.join(format!("{label}.plist"));
            plain_path(&file)?;
            fs::create_dir_all(&dirs.launch_agents)
                .map_err(|e| format!("cannot make {}: {e}", dirs.launch_agents.display()))?;
            atomic_write(&file, launchd_plist(&unit).as_bytes(), 0o644).map_err(|e| {
                format!("cannot write {}: {}", file.display(), scrub(&e.to_string()))
            })?;
            rec.kind = "launchd".into();
            rec.name = label.clone();
            rec.files.push(text(&file));
            let domain = format!("gui/{}", uid());
            // a service that is loaded already must be unloaded first; failing here only means it was not loaded
            let _ = r.exec.run(
                "launchctl",
                &["bootout".to_owned(), format!("{domain}/{label}")],
            );
            run_ok(r.exec, "launchctl", &["bootstrap", &domain, &text(&file)])?;
        }
        Os::Windows => {
            let name = task_name(r.agent);
            let file = dirs.config.join(format!("stonewave-wave-{}.cmd", r.agent));
            plain_path(&file)?;
            atomic_write(&file, windows_wrapper(&unit).as_bytes(), 0o644).map_err(|e| {
                format!("cannot write {}: {}", file.display(), scrub(&e.to_string()))
            })?;
            rec.kind = "schtasks".into();
            rec.name = name.clone();
            rec.files.push(text(&file));
            if dirs.win_user.is_empty() {
                return Err("cannot tell which Windows account to run the task as".into());
            }
            let task_file = dirs
                .config
                .join(format!("stonewave-wave-{}.task.xml", r.agent));
            plain_path(&task_file)?;
            let xml = windows_task_xml(&dirs.win_user, &text(&file));
            atomic_write(&task_file, &utf16le_with_bom(&xml), 0o644).map_err(|e| {
                format!(
                    "cannot write {}: {}",
                    task_file.display(),
                    scrub(&e.to_string())
                )
            })?;
            rec.files.push(text(&task_file));
            // a task that runs already is stopped first (the result does not matter)
            windows_stop(rec, r.exec);
            let args = schtasks_create_args(&name, &text(&task_file));
            let out = r
                .exec
                .run("schtasks", &args)
                .map_err(|e| format!("cannot run schtasks: {}", scrub(&e.to_string())))?;
            if !out.ok {
                return Err(format!(
                    "`schtasks /Create` failed: {}",
                    scrub(&out.problem())
                ));
            }
            run_ok(r.exec, "schtasks", &["/Run", "/TN", &name])?;
        }
    }
    Ok(())
}

#[derive(Debug, PartialEq, Eq)]
pub struct ServiceStatus {
    /// The service file is there.
    pub installed: bool,
    /// What the service manager says; None when it could not be asked.
    pub active: Option<bool>,
}

pub fn status(rec: &ServiceRec, exec: &dyn Exec) -> ServiceStatus {
    let installed = !rec.files.is_empty() && rec.files.iter().all(|f| Path::new(f).exists());
    let ask = |program: &str, args: Vec<String>| exec.run(program, &args).ok().map(|o| o.ok);
    let active = match rec.kind.as_str() {
        "systemd" => ask(
            "systemctl",
            vec![
                "--user".into(),
                "is-active".into(),
                "--quiet".into(),
                rec.name.clone(),
            ],
        ),
        "launchd" => ask(
            "launchctl",
            vec!["print".into(), format!("gui/{}/{}", uid(), rec.name)],
        ),
        "schtasks" => ask(
            "schtasks",
            vec!["/Query".into(), "/TN".into(), rec.name.clone()],
        ),
        _ => None,
    };
    ServiceStatus { installed, active }
}

/// Makes a running service read the key file again (the script reads it once, when it starts).
pub fn restart(rec: &ServiceRec, exec: &dyn Exec) -> Result<(), String> {
    match rec.kind.as_str() {
        "systemd" => run_ok(exec, "systemctl", &["--user", "restart", &rec.name]),
        "launchd" => run_ok(
            exec,
            "launchctl",
            &["kickstart", "-k", &format!("gui/{}/{}", uid(), rec.name)],
        ),
        "schtasks" => {
            windows_stop(rec, exec);
            run_ok(exec, "schtasks", &["/Run", "/TN", &rec.name])
        }
        other => Err(format!("unknown service kind {other}")),
    }
}

#[derive(Debug, Default)]
pub struct Removal {
    pub removed: Vec<String>,
    pub problems: Vec<String>,
}

/// Stops the service and removes the files recorded for it.
pub fn uninstall(rec: &ServiceRec, exec: &dyn Exec) -> Removal {
    let mut out = Removal::default();
    let quiet = |program: &str, args: Vec<String>| {
        let _ = exec.run(program, &args);
    };
    match rec.kind.as_str() {
        "systemd" => quiet(
            "systemctl",
            vec![
                "--user".into(),
                "disable".into(),
                "--now".into(),
                rec.name.clone(),
            ],
        ),
        "launchd" => quiet(
            "launchctl",
            vec!["bootout".into(), format!("gui/{}/{}", uid(), rec.name)],
        ),
        "schtasks" => {
            windows_stop(rec, exec);
            let del = exec.run(
                "schtasks",
                &[
                    "/Delete".to_owned(),
                    "/TN".to_owned(),
                    rec.name.clone(),
                    "/F".to_owned(),
                ],
            );
            match del {
                Ok(o) if o.ok => out.removed.push(format!("scheduled task {}", rec.name)),
                Ok(o) => {
                    // gone already is fine
                    let gone = exec
                        .run(
                            "schtasks",
                            &["/Query".to_owned(), "/TN".to_owned(), rec.name.clone()],
                        )
                        .map(|q| !q.ok)
                        .unwrap_or(false);
                    if !gone {
                        out.problems.push(format!(
                            "scheduled task {} could not be removed: {}",
                            rec.name,
                            scrub(&o.problem())
                        ));
                    }
                }
                Err(e) => out
                    .problems
                    .push(format!("cannot run schtasks: {}", scrub(&e.to_string()))),
            }
        }
        _ => {}
    }
    for f in &rec.files {
        match super::key::remove_if_there(Path::new(f)) {
            Ok(true) => out.removed.push(f.clone()),
            Ok(false) => {}
            Err(e) => out
                .problems
                .push(format!("cannot remove {f}: {}", scrub(&e.to_string()))),
        }
    }
    if rec.kind == "systemd" {
        quiet("systemctl", vec!["--user".into(), "daemon-reload".into()]);
    }
    out
}

#[cfg(test)]
mod tests {
    use super::super::exec::fake::{Fake, failed as fail_out};
    use super::super::key::testdir::TempDir;
    use super::*;

    fn unit<'a>(node: &'a str, script: &'a str, log: &'a str, key: &'a str) -> Unit<'a> {
        Unit {
            agent: "agent-demo",
            node,
            script,
            mcp_url: "https://wave.test/mcp",
            key_file: key,
            log,
        }
    }

    /// Is there text like `X-123` or `X-word-123` (a letter, a dash and a number: the shape of a ticket or decision id)?
    fn id_like(text: &str) -> Option<String> {
        let b = text.as_bytes();
        for i in 1..b.len().saturating_sub(2) {
            let starts_word = i < 2 || !b[i - 2].is_ascii_alphanumeric();
            if b[i] == b'-' && b[i - 1].is_ascii_uppercase() && starts_word {
                let rest = &text[i + 1..];
                let word = rest.chars().take_while(char::is_ascii_lowercase).count();
                let after = &rest[word..];
                let digits = |t: &str| t.chars().take_while(char::is_ascii_digit).count();
                let hit = (word == 0 && digits(rest) >= 2)
                    || (word > 0 && after.starts_with('-') && digits(&after[1..]) >= 2);
                if hit {
                    return Some(text[i - 1..(i + 12).min(text.len())].to_owned());
                }
            }
        }
        None
    }

    #[test]
    fn the_script_is_generic_and_holds_no_internal_ids() {
        assert_eq!(
            id_like(SCRIPT),
            None,
            "an id-like text in the embedded script"
        );
        // the check itself finds what it is meant to find
        assert!(id_like("see M-abc-103 for it").is_some());
        assert!(id_like("see X-123.").is_some());
        assert!(id_like("UTF-16 and U+E0000-E007F and Node-18").is_none());
        // nothing but a placeholder where an address would be, no company names, no section signs
        assert!(!SCRIPT.contains("stonewavetech"));
        assert!(!SCRIPT.contains('\u{a7}'));
        for (n, _) in SCRIPT.match_indices("https://") {
            let next = SCRIPT[n + 8..].chars().next();
            assert!(
                next.is_none_or(|c| matches!(c, '<' | '$' | '\'' | ' ' | '(' | '"' | '`')),
                "{}",
                &SCRIPT[n..n + 24]
            );
        }
        assert!(SCRIPT.starts_with("#!/usr/bin/env node"));
    }

    #[test]
    fn the_script_passes_node_check() {
        let Some(node) = super::super::exec::System.which("node") else {
            eprintln!("node not installed: skipped");
            return;
        };
        let t = TempDir::new("nodecheck");
        let f = t.path().join("fleet-watch.mjs");
        fs::write(&f, SCRIPT).unwrap();
        let out = std::process::Command::new(node)
            .arg("--check")
            .arg(&f)
            .output()
            .unwrap();
        assert!(
            out.status.success(),
            "{}",
            String::from_utf8_lossy(&out.stderr)
        );
    }

    #[test]
    fn systemd_unit_text() {
        let t = systemd_unit(&unit(
            "/usr/bin/node",
            "/home/a/.config/wave/fleet-watch.mjs",
            "/home/a/.local/state/wave/agent-demo.wake.log",
            "/home/a/.config/wave/agent-demo.key",
        ));
        for line in [
            "Type=simple",
            "Restart=always",
            "RestartSec=20",
            "Environment=\"WAVE_MCP_URL=https://wave.test/mcp\"",
            "Environment=\"WAVE_AGENT_KEY_FILE=/home/a/.config/wave/agent-demo.key\"",
            "ExecStart=\"/usr/bin/node\" \"/home/a/.config/wave/fleet-watch.mjs\" --session \"agent-demo\"",
            "StandardOutput=append:/home/a/.local/state/wave/agent-demo.wake.log",
            "StandardError=append:/home/a/.local/state/wave/agent-demo.wake.log",
            "WantedBy=default.target",
        ] {
            assert!(t.lines().any(|l| l == line), "missing {line}\n{t}");
        }
        assert!(!t.contains("wak_"));
    }

    #[test]
    fn systemd_quoting_survives_spaces_quotes_percent_dollar_and_ampersand() {
        let node = "/opt/my node/bin/node";
        let script = "/home/a b/\"q\"/100%/$HOME/a&b.mjs";
        let t = systemd_unit(&unit(node, script, "/l og/%h/x", "/k $ey"));
        let exec = t.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert_eq!(
            exec,
            "ExecStart=\"/opt/my node/bin/node\" \"/home/a b/\\\"q\\\"/100%%/$$HOME/a&b.mjs\" --session \"agent-demo\""
        );
        assert!(t.contains("StandardOutput=append:/l og/%%h/x\n"), "{t}");
        assert!(
            t.contains("Environment=\"WAVE_AGENT_KEY_FILE=/k $ey\"\n"),
            "{t}"
        );
        assert_eq!(systemd_env("A", "x\"y\\z%"), "\"A=x\\\"y\\\\z%%\"");
    }

    #[test]
    fn launchd_plist_text_and_xml_escaping() {
        let p = launchd_plist(&unit(
            "/usr/local/bin/node",
            "/Users/a/.config/wave/fleet-watch.mjs",
            "/Users/a/.local/state/wave/agent-demo.wake.log",
            "/Users/a/.config/wave/agent-demo.key",
        ));
        for needle in [
            "<key>Label</key>\n  <string>tech.stonewave.wave.agent-demo</string>",
            "<string>/usr/local/bin/node</string>",
            "<string>--session</string>",
            "<string>agent-demo</string>",
            "<key>WAVE_MCP_URL</key>\n    <string>https://wave.test/mcp</string>",
            "<key>WAVE_AGENT_KEY_FILE</key>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>KeepAlive</key>\n  <true/>",
            "<key>StandardOutPath</key>\n  <string>/Users/a/.local/state/wave/agent-demo.wake.log</string>",
            "<key>StandardErrorPath</key>",
        ] {
            assert!(p.contains(needle), "missing {needle}\n{p}");
        }
        let tricky = launchd_plist(&unit("/n/<node>", "/s/a&b \"q\" 'x'.mjs", "/l", "/k"));
        assert!(tricky.contains("<string>/n/&lt;node&gt;</string>"));
        assert!(tricky.contains("<string>/s/a&amp;b &quot;q&quot; &apos;x&apos;.mjs</string>"));
        assert!(!tricky.contains("a&b"));
        assert!(!p.contains("wak_"));
    }

    #[test]
    fn windows_wrapper_text_and_quoting() {
        let w = windows_wrapper(&unit(
            "C:\\Program Files\\nodejs\\node.exe",
            "C:\\Users\\a b\\AppData\\Roaming\\wave\\fleet-watch.mjs",
            "C:\\Users\\a b\\AppData\\Local\\wave\\agent-demo.wake.log",
            "C:\\Users\\a b\\AppData\\Roaming\\wave\\agent-demo.key",
        ));
        assert!(w.contains("set \"WAVE_MCP_URL=https://wave.test/mcp\"\r\n"));
        assert!(w.contains(
            "set \"WAVE_AGENT_KEY_FILE=C:\\Users\\a b\\AppData\\Roaming\\wave\\agent-demo.key\"\r\n"
        ));
        assert!(w.contains("\"C:\\Program Files\\nodejs\\node.exe\" \"C:\\Users\\a b\\AppData\\Roaming\\wave\\fleet-watch.mjs\" --session \"agent-demo\" >> \"C:\\Users\\a b\\AppData\\Local\\wave\\agent-demo.wake.log\" 2>&1\r\n"), "{w}");
        assert!(w.contains("goto run"));
        assert!(!w.replace("\r\n", "").contains('\n'), "only CRLF line ends");
        // percent signs would be expanded by cmd; ampersands and carets inside quotes are literal
        let tricky = windows_wrapper(&unit(
            "C:\\n&n^\\100%\\node.exe",
            "C:\\s.mjs",
            "C:\\l.log",
            "C:\\k",
        ));
        assert!(tricky.contains("\"C:\\n&n^\\100%%\\node.exe\""), "{tricky}");
        assert!(!w.contains("wak_"));
    }

    #[test]
    fn schtasks_arguments_and_the_task_definition() {
        assert_eq!(
            schtasks_create_args("StonewaveWave-agent-demo", "C:\\a b\\x.task.xml"),
            [
                "/Create",
                "/TN",
                "StonewaveWave-agent-demo",
                "/XML",
                "C:\\a b\\x.task.xml",
                "/F"
            ]
        );
        let x = windows_task_xml("PC\\ada", "C:\\Users\\a & b\\x.cmd");
        assert!(x.contains("<LogonTrigger>"));
        assert!(x.contains("<UserId>PC\\ada</UserId>"));
        assert!(x.contains("<RunLevel>LeastPrivilege</RunLevel>"));
        assert!(x.contains("<ExecutionTimeLimit>PT0S</ExecutionTimeLimit>"));
        assert!(x.contains("<Command>conhost.exe</Command>"));
        assert!(
            x.contains(
                "<Arguments>--headless cmd.exe /c \"C:\\Users\\a &amp; b\\x.cmd\"</Arguments>"
            ),
            "{x}"
        );
        assert!(!x.contains("wak_"));
        let b = utf16le_with_bom("A\u{e4}");
        assert_eq!(b, vec![0xFF, 0xFE, b'A', 0, 0xE4, 0]);
    }

    #[test]
    fn node_versions() {
        assert_eq!(node_major("v18.19.0\n"), Some(18));
        assert_eq!(node_major("v24.20.0"), Some(24));
        assert_eq!(node_major("v16.20.2"), Some(16));
        assert_eq!(node_major("nope"), None);
        let ok =
            Fake::new(&["node"]).with(|_, _| Some(super::super::exec::fake::ok_out("v20.1.0\n")));
        assert!(find_node(&ok).is_ok());
        let old =
            Fake::new(&["node"]).with(|_, _| Some(super::super::exec::fake::ok_out("v16.0.0\n")));
        assert!(find_node(&old).unwrap_err().contains("too old"));
        assert!(
            find_node(&Fake::new(&[]))
                .unwrap_err()
                .contains("not found")
        );
    }

    #[test]
    fn log_trimming_and_follow_commands() {
        let long: String = (0..1500).map(|i| format!("line {i}\n")).collect();
        let t = trim_lines(&long, 1000).unwrap();
        assert_eq!(t.lines().count(), 1000);
        assert!(t.starts_with("line 500\n") && t.ends_with("line 1499\n"));
        assert_eq!(trim_lines("a\nb\n", 1000), None);
        // an unfinished last line is kept as it is
        assert_eq!(trim_lines("a\nb\nc", 2).unwrap(), "b\nc");
        assert_eq!(
            follow_command(Os::Linux, "/home/a b/it's.log"),
            "tail -n 0 -F '/home/a b/it'\\''s.log'"
        );
        assert_eq!(
            follow_command(Os::Windows, "C:\\a\\it's.log"),
            "Get-Content -Wait -Tail 0 -LiteralPath 'C:\\a\\it''s.log'"
        );
    }

    #[test]
    fn prepare_log_creates_private_file_and_cuts_it() {
        let t = TempDir::new("log");
        let dirs = Dirs::under(t.path());
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "x",
        };
        let log = prepare_log(&dirs, "agent-demo", &acl).unwrap();
        assert!(log.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                fs::metadata(&log).unwrap().permissions().mode() & 0o777,
                0o600
            );
            assert_eq!(
                fs::metadata(&dirs.state).unwrap().permissions().mode() & 0o777,
                0o700
            );
        }
        let long: String = (0..1500).map(|i| format!("line {i}\n")).collect();
        fs::write(&log, long).unwrap();
        prepare_log(&dirs, "agent-demo", &acl).unwrap();
        let after = fs::read_to_string(&log).unwrap();
        assert_eq!(after.lines().count(), 1000);
        assert!(after.starts_with("line 500\n"));
        // a short log is left alone
        prepare_log(&dirs, "agent-demo", &acl).unwrap();
        assert_eq!(fs::read_to_string(&log).unwrap(), after);
    }

    fn request<'a>(dirs: &'a Dirs, exec: &'a Fake, node: &'a Path) -> Request<'a> {
        Request {
            agent: "agent-demo",
            mcp_url: "https://wave.test/mcp",
            node,
            dirs,
            exec,
        }
    }

    #[test]
    fn install_per_os_calls_the_service_manager_with_exact_arguments() {
        let t = TempDir::new("inst");
        let node = Path::new("/usr/bin/node");
        for os in [Os::Linux, Os::Mac, Os::Windows] {
            let mut dirs = Dirs::under(&t.path().join(format!("{os:?}")));
            dirs.os = os;
            fs::create_dir_all(&dirs.config).unwrap();
            let fake = Fake::new(&[]);
            let acl = Acl {
                exec: &fake,
                user: "x",
            };
            let done = install(&request(&dirs, &fake, node), &acl);
            assert_eq!(done.problem, None, "{os:?}");
            let lines: Vec<String> = fake
                .lines()
                .into_iter()
                .filter(|l| !l.starts_with("icacls"))
                .collect();
            match os {
                Os::Linux => {
                    assert_eq!(done.rec.kind, "systemd");
                    assert_eq!(
                        lines,
                        vec![
                            "systemctl --user daemon-reload",
                            "systemctl --user enable --now stonewave-wave-agent-demo.service"
                        ]
                    );
                    assert!(Path::new(&done.rec.files[0]).exists());
                }
                Os::Mac => {
                    assert_eq!(done.rec.kind, "launchd");
                    assert!(lines[0].starts_with("launchctl bootout gui/"), "{lines:?}");
                    assert!(lines[0].ends_with("/tech.stonewave.wave.agent-demo"));
                    assert!(
                        lines[1].starts_with("launchctl bootstrap gui/"),
                        "{lines:?}"
                    );
                    assert!(lines[1].ends_with(".plist"));
                }
                Os::Windows => {
                    assert_eq!(done.rec.kind, "schtasks");
                    assert_eq!(lines[0], "schtasks /End /TN StonewaveWave-agent-demo");
                    assert!(
                        lines[1]
                            .starts_with("powershell -NoProfile -NonInteractive -Command $w = '"),
                        "{lines:?}"
                    );
                    assert!(
                        lines[2].starts_with("schtasks /Create /TN StonewaveWave-agent-demo /XML ")
                            && lines[2].ends_with(".task.xml /F"),
                        "{lines:?}"
                    );
                    assert_eq!(done.rec.files.len(), 2);
                    let raw = fs::read(&done.rec.files[1]).unwrap();
                    assert_eq!(&raw[..2], &[0xFF, 0xFE], "UTF-16 with a byte order mark");
                    assert_eq!(lines[3], "schtasks /Run /TN StonewaveWave-agent-demo");
                }
            }
            // the script is on disk, the log exists, nothing holds a key
            assert!(dirs.wake_script().exists());
            assert!(dirs.wake_log("agent-demo").exists());
        }
    }

    #[test]
    fn a_second_install_restarts_a_systemd_service_and_a_failure_is_reported_with_what_was_written()
    {
        let t = TempDir::new("inst2");
        let mut dirs = Dirs::under(t.path());
        dirs.os = Os::Linux;
        fs::create_dir_all(&dirs.config).unwrap();
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "x",
        };
        let node = Path::new("/usr/bin/node");
        install(&request(&dirs, &fake, node), &acl);
        let fake2 = Fake::new(&[]);
        let done = install(&request(&dirs, &fake2, node), &acl);
        assert_eq!(done.problem, None);
        assert_eq!(
            fake2.lines().last().unwrap(),
            "systemctl --user restart stonewave-wave-agent-demo.service"
        );
        let failing = Fake::new(&[]).with(|p, a| {
            (p == "systemctl" && a.iter().any(|x| x == "enable"))
                .then(|| fail_out("Failed to connect to bus"))
        });
        let done = install(&request(&dirs, &failing, node), &acl);
        assert!(done.problem.unwrap().contains("Failed to connect to bus"));
        assert_eq!(
            done.rec.files.len(),
            1,
            "the file that was written is recorded for disconnect"
        );
    }

    #[test]
    fn uninstall_stops_and_removes_only_the_recorded_files() {
        let t = TempDir::new("uninst");
        let mut dirs = Dirs::under(t.path());
        dirs.os = Os::Linux;
        fs::create_dir_all(&dirs.config).unwrap();
        let fake = Fake::new(&[]);
        let acl = Acl {
            exec: &fake,
            user: "x",
        };
        let done = install(&request(&dirs, &fake, Path::new("/usr/bin/node")), &acl);
        let bystander = dirs.systemd_user.join("other.service");
        fs::write(&bystander, "x").unwrap();
        let fake = Fake::new(&[]);
        let r = uninstall(&done.rec, &fake);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(r.removed.len(), 1);
        assert!(!Path::new(&done.rec.files[0]).exists());
        assert!(bystander.exists());
        assert_eq!(
            fake.lines(),
            vec![
                "systemctl --user disable --now stonewave-wave-agent-demo.service",
                "systemctl --user daemon-reload"
            ]
        );
        // a second time: nothing left, no problem
        assert!(uninstall(&done.rec, &Fake::new(&[])).problems.is_empty());
        let st = status(&done.rec, &Fake::new(&[]));
        assert!(!st.installed);
    }

    #[test]
    fn schtasks_uninstall_ends_and_deletes() {
        let rec = ServiceRec {
            kind: "schtasks".into(),
            name: "StonewaveWave-agent-demo".into(),
            files: vec!["C:\\Users\\a b\\wave\\x.cmd".into()],
        };
        let fake = Fake::new(&[]);
        let r = uninstall(&rec, &fake);
        assert!(r.problems.is_empty());
        let lines = fake.lines();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert_eq!(lines[0], "schtasks /End /TN StonewaveWave-agent-demo");
        assert!(lines[1].starts_with("powershell -NoProfile -NonInteractive -Command "));
        assert_eq!(lines[2], "schtasks /Delete /TN StonewaveWave-agent-demo /F");
        // what the stop script names: this batch file, and this script with this session
        let ps = windows_stop_script("C:\\Users\\it's\\x.cmd", "agent-demo");
        assert!(
            ps.starts_with("$w = 'C:\\Users\\it''s\\x.cmd'; $s = '--session \"agent-demo\"';"),
            "{ps}"
        );
        assert!(
            ps.contains("$_.ProcessId -ne $PID"),
            "the stopper does not stop itself"
        );
        assert!(ps.contains("fleet-watch.mjs"));
        assert!(!ps.contains("wak_"));
        // delete fails and the task is still there: a problem; task gone already: fine
        let stuck =
            Fake::new(&[]).with(|_, a| (a[0] == "/Delete").then(|| fail_out("Access is denied.")));
        assert_eq!(uninstall(&rec, &stuck).problems.len(), 1);
        let gone = Fake::new(&[]).with(|_, a| {
            matches!(a[0].as_str(), "/Delete" | "/Query")
                .then(|| fail_out("ERROR: The system cannot find the file specified."))
        });
        assert!(uninstall(&rec, &gone).problems.is_empty());
    }

    /// Against the real Task Scheduler and icacls of this Windows machine: `cargo test -- --ignored windows_real`.
    /// Makes a task for the current user (named for this test), starts it, restarts it and deletes it again.
    #[cfg(windows)]
    #[test]
    #[ignore = "changes the Task Scheduler of this machine (and removes it again)"]
    fn windows_real_service_round_trip() {
        use super::super::exec::System;
        let t = TempDir::new("winreal");
        let mut dirs = Dirs::under(t.path());
        dirs.os = Os::Windows;
        dirs.win_user = format!(
            "{}\\{}",
            std::env::var("USERDOMAIN").unwrap(),
            std::env::var("USERNAME").unwrap()
        );
        let acl = Acl {
            exec: &System,
            user: &dirs.win_user,
        };
        ensure_private_dir(&dirs.config, &acl).unwrap();
        // the agent name makes the task name unique to this test
        let agent = "agent-teststation";
        fs::write(dirs.key_file(agent), "wak_notarealkey0000").unwrap();
        let node = System.which("node").expect("node");
        let done = install(
            &Request {
                agent,
                mcp_url: "http://127.0.0.1:9/mcp",
                node: &node,
                dirs: &dirs,
                exec: &System,
            },
            &acl,
        );
        assert_eq!(done.problem, None);
        assert_eq!(done.rec.files.len(), 2);
        let st = status(&done.rec, &System);
        assert!(st.installed);
        assert_eq!(st.active, Some(true), "the task is known to the scheduler");
        let r = restart(&done.rec, &System);
        assert!(r.is_ok(), "{r:?}");
        let r = uninstall(&done.rec, &System);
        assert!(r.problems.is_empty(), "{:?}", r.problems);
        assert_eq!(
            status(&done.rec, &System).active,
            Some(false),
            "the task is gone"
        );
        assert!(done.rec.files.iter().all(|f| !Path::new(f).exists()));
    }
}
