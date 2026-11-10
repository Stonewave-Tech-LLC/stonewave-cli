//! The two long-running or forced commands on a connected server: `stonewave daemon` (the service) and
//! `stonewave agent-shell` (the forced command of the agent sshd).
//!
//! Neither can run an arbitrary command. The daemon only reports health; `agent-shell` knows a fixed list
//! of named operations (today only `health`) and refuses everything else.

use std::io::Write;
use std::process::ExitCode;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::doctor;
use crate::setup;

const HEALTH_FILE: &str = "/var/lib/stonewave/health.json";
const HEALTH_EVERY: Duration = Duration::from_secs(60);

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn host_name() -> String {
    std::fs::read_to_string("/etc/hostname")
        .map(|s| s.trim().to_owned())
        .unwrap_or_default()
}

/// The `doctor` checks as one JSON object with a time stamp: what `health.json` and the health report hold.
pub fn health_json() -> String {
    let checks = doctor::run_all(&doctor::SYSTEM);
    format!(
        r#"{{"ts":{},"report":{}}}"#,
        now_secs(),
        doctor::to_json(&host_name(), &checks)
    )
}

fn write_atomic(path: &str, text: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// This build of the daemon only keeps `health.json` fresh. The tunnel to Wave comes with the relay.
pub fn run_daemon() -> ExitCode {
    loop {
        if let Err(e) = write_atomic(HEALTH_FILE, &health_json()) {
            eprintln!("stonewaved: cannot write {HEALTH_FILE}: {e}");
        }
        std::thread::sleep(HEALTH_EVERY);
    }
}

/// Appends one line to the audit log. Failing to log must not hide the answer, but is told on stderr.
fn audit(event: &serde_json::Value) {
    let line = format!("{event}\n");
    let r = std::fs::OpenOptions::new()
        .append(true)
        .open(setup::AUDIT)
        .and_then(|mut f| f.write_all(line.as_bytes()));
    if let Err(e) = r {
        eprintln!("stonewave agent-shell: audit log not written: {e}");
    }
}

/// Operations the agent may ask for, by name. Everything else is refused (and logged).
pub fn run_agent_shell() -> ExitCode {
    let asked = std::env::var("SSH_ORIGINAL_COMMAND").unwrap_or_default();
    let asked = asked.trim();
    let result = match asked {
        "health" => {
            println!("{}", health_json());
            "ok"
        }
        _ => {
            eprintln!(
                "stonewave agent-shell: operation not available (only `health` exists in this build)"
            );
            "refused"
        }
    };
    audit(&serde_json::json!({
        "ts": now_secs(),
        "event": "agent-command",
        "command": asked.chars().take(200).collect::<String>(),
        "result": result,
        "from": std::env::var("SSH_CONNECTION").unwrap_or_default().chars().take(80).collect::<String>(),
        "certified": std::env::var("SSH_USER_AUTH").is_ok(),
    }));
    if result == "ok" {
        ExitCode::SUCCESS
    } else {
        ExitCode::from(126)
    }
}
