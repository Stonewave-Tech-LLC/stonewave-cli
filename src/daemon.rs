//! The two long-running or forced commands on a connected server: `stonewave daemon` (the service) and
//! `stonewave agent-shell` (the forced command of the agent sshd).
//!
//! Neither can run an arbitrary command. The daemon keeps the line to Wave (see `tunnel.rs`) and reports
//! health; `agent-shell` knows a fixed list of named operations (today only `health`) and refuses
//! everything else.

use std::io::Write;
use std::process::ExitCode;
use std::sync::Arc;
use std::sync::atomic::AtomicBool;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use crate::connect::Account;
use crate::relay::{self, Paths};
use crate::tunnel::{self, CurlTransport, EXIT_REVOKED, End, Settings};
use crate::{doctor, health, setup};

pub const HEALTH_FILE: &str = "/var/lib/stonewave/health.json";
pub const TUNNEL_FILE: &str = "/var/lib/stonewave/tunnel.json";

pub fn now_secs() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn write_atomic(path: &str, text: &str) -> std::io::Result<()> {
    let tmp = format!("{path}.tmp");
    std::fs::write(&tmp, text)?;
    std::fs::rename(&tmp, path)
}

/// The health report as JSON text (the `v` of a health frame).
pub fn health_text() -> String {
    health::report(&doctor::SYSTEM, now_secs()).to_string()
}

/// What `stonewave status` reads: the state of the line, written whenever it changes.
pub fn write_tunnel_state(state: &str, detail: &str) {
    let v = serde_json::json!({ "state": state, "detail": detail, "ts": now_secs() });
    if let Err(e) = write_atomic(TUNNEL_FILE, &(v.to_string() + "\n")) {
        eprintln!("stonewaved: cannot write {TUNNEL_FILE}: {e}");
    }
}

pub fn run_daemon() -> ExitCode {
    // The report is also kept on disk for `status` and for the agent shell, whether or not Wave is reached.
    let report: Arc<dyn Fn() -> String + Send + Sync> = Arc::new(|| {
        let text = health_text();
        let wrapped = format!(r#"{{"ts":{},"health":{text}}}"#, now_secs());
        if let Err(e) = write_atomic(HEALTH_FILE, &(wrapped + "\n")) {
            eprintln!("stonewaved: cannot write {HEALTH_FILE}: {e}");
        }
        text
    });

    // Without a key the daemon only keeps the local report. It looks again every round, so a server that is connected while
    // it runs (`connect` after `setup`) goes online without anyone restarting the service.
    let mut told = false;
    let (key, identity) = loop {
        if let Some(found) = relay::load(&Paths::system()) {
            break found;
        }
        if !told {
            eprintln!(
                "stonewaved: this server is not connected to Wave yet (no server key); keeping the local health report only"
            );
            told = true;
        }
        write_tunnel_state("local", "not connected: run `sudo stonewave connect`");
        report();
        std::thread::sleep(Duration::from_secs(60));
    };
    let relay_addr = match Account::parse(&identity.relay) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stonewaved: the relay address in server.json is not usable: {e}");
            return ExitCode::from(EXIT_REVOKED);
        }
    };
    let transport = Arc::new(CurlTransport {
        base: relay_addr.base().to_owned(),
        server_ref: identity.server_ref.clone(),
        key,
    });
    eprintln!(
        "stonewaved: {} for {} via {}",
        identity.name,
        identity.company,
        relay_addr.host_text()
    );
    let status = |state: &str, detail: &str| {
        eprintln!("stonewaved: {state} {detail}");
        write_tunnel_state(state, detail);
    };
    match tunnel::run_forever(
        transport,
        &Settings::default(),
        report,
        &status,
        &AtomicBool::new(false),
    ) {
        End::Revoked => {
            eprintln!(
                "stonewaved: Wave cut this server off; stopping for good (`sudo stonewave setup` to start again)"
            );
            ExitCode::from(EXIT_REVOKED)
        }
        End::Retry(_) => ExitCode::FAILURE,
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
            println!(r#"{{"ts":{},"health":{}}}"#, now_secs(), health_text());
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
