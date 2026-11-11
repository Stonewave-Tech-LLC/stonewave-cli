//! `stonewave status`: what is true on this machine right now. It reads files and asks systemd; it talks to nobody.

use serde_json::{Value, json};

use crate::banner;
use crate::connect::tame;
use crate::flow::{Host, Outcome};
use crate::realsys::RealSys;
use crate::relay::{IDENTITY_FILE, Identity};
use crate::setup::{self, Sys};
use crate::style::{ColorMode, pal};
use crate::term::{Result, Term};
use crate::ui::{DETAIL, INDENT, kv};

fn read_json(path: &str) -> Option<serde_json::Value> {
    serde_json::from_str(&std::fs::read_to_string(path).ok()?).ok()
}

fn age(ts: u64) -> String {
    let d = crate::daemon::now_secs().saturating_sub(ts);
    match d {
        0..=89 => format!("{d} s ago"),
        90..=5399 => format!("{} min ago", d / 60),
        _ => format!("{} h ago", d / 3600),
    }
}

fn line(m: ColorMode, label: &str, value: &str) -> String {
    kv(m, DETAIL, label, 11, value)
}

pub fn show(term: &mut Term, host: &Host) -> Result<Outcome> {
    let m = term.mode();
    term.blank();
    term.line(&banner::compact(m, &host.name));
    term.blank();
    let Some(state) = read_json(setup::STATE) else {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(
                pal::GREY,
                "Not set up on this server. `sudo stonewave connect` connects it to Wave."
            )
        ));
        term.flush()?;
        return Ok(Outcome::Done);
    };
    let ident = Identity::parse(
        &std::fs::read_to_string(format!("{}/{IDENTITY_FILE}", setup::VAR_LIB)).unwrap_or_default(),
    );
    match &ident {
        Some(i) => term.line(&line(m, "Server", &format!("{} · {}", i.name, i.company))),
        None => term.line(&line(
            m,
            "Server",
            &m.fg(pal::GREY, "not connected to Wave (set up locally)"),
        )),
    }
    if state["revoked"].as_bool() == Some(true) {
        term.line(&line(
            m,
            "Access",
            &m.fg(
                pal::AMBER,
                "revoked · `sudo stonewave setup` opens it again",
            ),
        ));
    }
    if ident.is_some() {
        let text = match read_json(crate::daemon::TUNNEL_FILE) {
            Some(t) => {
                let st = t["state"].as_str().unwrap_or("?");
                let detail = t["detail"].as_str().unwrap_or("");
                let when = age(t["ts"].as_u64().unwrap_or(0));
                let colored = match st {
                    "connected" => m.fg_bold(pal::GREEN, "connected"),
                    "revoked" => m.fg_bold(pal::RED, "revoked by Wave"),
                    other => m.fg(pal::AMBER, other),
                };
                let extra = if detail.is_empty() {
                    when
                } else {
                    format!("{when} · {detail}")
                };
                format!("{colored}  {}", m.fg(pal::GREY, &extra))
            }
            None => m.fg(pal::GREY, "the daemon has not reported yet"),
        };
        term.line(&line(m, "Line", &text));
    }
    let trusted = std::fs::read_to_string(setup::TRUST)
        .map(|t| setup::read_trust(&t).len())
        .unwrap_or(0);
    let sshd = RealSys
        .run("systemctl", &["is-active", "stonewave-sshd"])
        .out
        .trim()
        .to_owned();
    term.line(&line(
        m,
        "Agent SSH",
        &format!(
            "{sshd} · 127.0.0.1:{} · {trusted} admin device(s) trusted",
            setup::AGENT_PORT
        ),
    ));
    match read_json(crate::daemon::HEALTH_FILE) {
        Some(h) => {
            let v = &h["health"];
            let n = |k: &str| v["metrics"][k].as_f64();
            let mut parts = Vec::new();
            if let (Some(f), Some(t)) = (n("disk_free_mb"), n("disk_total_mb")) {
                parts.push(format!(
                    "disk {:.0} of {:.0} GB free",
                    f / 1024.0,
                    t / 1024.0
                ));
            }
            if let (Some(f), Some(t)) = (n("mem_available_mb"), n("mem_total_mb")) {
                parts.push(format!("memory {f:.0} of {t:.0} MB free"));
            }
            if let Some(l) = n("load1") {
                parts.push(format!("load {l:.2}"));
            }
            let when = m.fg(pal::GREY, &age(h["ts"].as_u64().unwrap_or(0)));
            term.line(&line(
                m,
                "Health",
                &format!("{}  {when}", parts.join(" · ")),
            ));
            if let Some(checks) = v["checks"].as_array() {
                for c in checks
                    .iter()
                    .filter(|c| matches!(c["level"].as_str(), Some("warn" | "fail")))
                {
                    let text = format!(
                        "{}: {}",
                        c["id"].as_str().unwrap_or("?"),
                        c["detail"].as_str().unwrap_or("")
                    );
                    term.line(&format!("{DETAIL}{:11}{}", "", m.fg(pal::AMBER, &text)));
                }
            }
        }
        None => term.line(&line(
            m,
            "Health",
            &m.fg(
                pal::GREY,
                "no report yet (the daemon writes one every minute)",
            ),
        )),
    }
    term.flush()?;
    Ok(Outcome::Done)
}

// ------------------------------------------------------------------ `status --json`

/// Metrics the health report may carry (the relay accepts exactly these).
const METRICS: [&str; 7] = [
    "disk_free_mb",
    "disk_total_mb",
    "mem_available_mb",
    "mem_total_mb",
    "load1",
    "uptime_s",
    "updates",
];

/// What reading the state file said. `/var/lib/stonewave` is private to the daemon user, so a person
/// without sudo sees `Denied`, which is not the same as "not set up".
#[derive(Debug, Clone, PartialEq)]
pub enum Access {
    Found(Value),
    Missing,
    Denied,
}

fn read_access(path: &str) -> Access {
    match std::fs::read_to_string(path) {
        Ok(text) => serde_json::from_str(&text).map_or(Access::Missing, Access::Found),
        Err(e) if e.kind() == std::io::ErrorKind::PermissionDenied => Access::Denied,
        Err(_) => Access::Missing,
    }
}

/// Everything the report is made from, read once. The report itself is a pure function of this.
#[derive(Debug, Clone)]
pub struct Inputs {
    pub now: u64,
    pub state: Access,
    pub identity: Option<Identity>,
    pub tunnel: Option<Value>,
    pub health: Option<Value>,
    pub trusted_devices: usize,
    /// `systemctl is-active` of the agent sshd and of the daemon ("active", "inactive", "failed", ...).
    pub agent_ssh: String,
    pub daemon: String,
}

fn unit_state(unit: &str) -> String {
    let out = RealSys.run("systemctl", &["is-active", unit]).out;
    match tame(out.trim(), 20) {
        s if s.is_empty() => "unknown".to_owned(),
        s => s,
    }
}

pub fn gather() -> Inputs {
    let state = read_access(setup::STATE);
    let identity = Identity::parse(
        &std::fs::read_to_string(format!("{}/{IDENTITY_FILE}", setup::VAR_LIB)).unwrap_or_default(),
    );
    let set_up = matches!(state, Access::Found(_));
    Inputs {
        now: crate::daemon::now_secs(),
        identity,
        tunnel: read_json(crate::daemon::TUNNEL_FILE),
        health: read_json(crate::daemon::HEALTH_FILE),
        trusted_devices: std::fs::read_to_string(setup::TRUST)
            .map(|t| setup::read_trust(&t).len())
            .unwrap_or(0),
        agent_ssh: if set_up {
            unit_state("stonewave-sshd")
        } else {
            String::new()
        },
        daemon: if set_up {
            unit_state("stonewaved")
        } else {
            String::new()
        },
        state,
    }
}

/// The health report, rebuilt field by field like the relay does: nothing unknown is passed on.
fn health_json(h: &Value, now: u64) -> Value {
    let v = &h["health"];
    let ts = h["ts"].as_u64().unwrap_or(0);
    let metrics: serde_json::Map<String, Value> = METRICS
        .iter()
        .map(|k| {
            let n = &v["metrics"][*k];
            (
                (*k).to_owned(),
                if n.is_number() {
                    n.clone()
                } else {
                    Value::Null
                },
            )
        })
        .collect();
    let units: serde_json::Map<String, Value> = ["stonewave-sshd", "sshd"]
        .iter()
        .filter_map(|k| {
            v["units"][*k]
                .as_bool()
                .map(|b| ((*k).to_owned(), Value::Bool(b)))
        })
        .collect();
    let checks: Vec<Value> = v["checks"]
        .as_array()
        .map(Vec::as_slice)
        .unwrap_or_default()
        .iter()
        .filter_map(|c| {
            let level = c["level"]
                .as_str()
                .filter(|l| ["ok", "warn", "fail", "info"].contains(l))?;
            Some(json!({
                "id": tame(c["id"].as_str().unwrap_or("?"), 24),
                "level": level,
                "detail": tame(c["detail"].as_str().unwrap_or(""), 200),
            }))
        })
        .collect();
    json!({
        "ts": ts,
        "age_s": now.saturating_sub(ts),
        "version": tame(v["version"].as_str().unwrap_or(""), 24),
        "metrics": metrics,
        "units": units,
        "checks": checks,
    })
}

/// The one JSON object of `stonewave status --json`. Only what `status` shows (plus the daemon unit
/// state): never the server key, never tokens, never `server.json` as a whole.
pub fn report_json(i: &Inputs) -> Value {
    let base = |readable: bool, set_up: Value| {
        json!({
            "stonewave": crate::VERSION,
            "ts": i.now,
            "readable": readable,
            "set_up": set_up,
            "connected": false,
            "access_revoked": false,
            "server": null,
            "line": null,
            "daemon": null,
            "agent_ssh": null,
            "health": null,
        })
    };
    let state = match &i.state {
        Access::Denied => {
            let mut v = base(false, Value::Null);
            v["hint"] = json!(
                "cannot read /var/lib/stonewave as this user; run `sudo stonewave status --json`"
            );
            return v;
        }
        Access::Missing => return base(true, json!(false)),
        Access::Found(s) => s,
    };
    let mut v = base(true, json!(true));
    let revoked = state["revoked"].as_bool() == Some(true);
    v["access_revoked"] = json!(revoked);
    if let Some(id) = &i.identity {
        v["server"] = json!({ "name": tame(&id.name, 120), "company": tame(&id.company, 120) });
        if let Some(t) = &i.tunnel {
            let ts = t["ts"].as_u64().unwrap_or(0);
            v["line"] = json!({
                "state": tame(t["state"].as_str().unwrap_or("?"), 20),
                "detail": tame(t["detail"].as_str().unwrap_or(""), 200),
                "ts": ts,
                "age_s": i.now.saturating_sub(ts),
            });
        }
    }
    v["daemon"] = json!(i.daemon);
    v["agent_ssh"] = json!({
        "state": i.agent_ssh,
        "address": format!("127.0.0.1:{}", setup::AGENT_PORT),
        "trusted_devices": i.trusted_devices,
    });
    // "connected" must not rest on a stale file: the daemon has to be running too.
    v["connected"] =
        json!(!revoked && i.daemon == "active" && v["line"]["state"].as_str() == Some("connected"));
    v["health"] = i
        .health
        .as_ref()
        .map_or(Value::Null, |h| health_json(h, i.now));
    v
}

/// `stonewave status --json`: prints the report. Exit code 0 whenever a report could be made.
pub fn print_json() -> std::process::ExitCode {
    use std::io::Write;
    let text = report_json(&gather()).to_string();
    match writeln!(std::io::stdout().lock(), "{text}") {
        Ok(()) => std::process::ExitCode::SUCCESS,
        // The reader went away (`stonewave status --json | head`): nothing left to tell anyone.
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => std::process::ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stonewave: {e}");
            std::process::ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const REF: &str = "11111111-1111-4111-8111-111111111111.22222222-2222-4222-8222-222222222222";

    fn identity() -> Identity {
        Identity {
            server_ref: REF.into(),
            relay: "https://relay.example".into(),
            company: "acme".into(),
            name: "web-01".into(),
        }
    }

    fn inputs() -> Inputs {
        Inputs {
            now: 1_000_100,
            state: Access::Found(json!({ "revoked": false })),
            identity: Some(identity()),
            tunnel: Some(json!({ "state": "connected", "detail": "", "ts": 1_000_000u64 })),
            health: Some(json!({
                "ts": 1_000_040u64,
                "health": {
                    "ts": 1_000_040u64,
                    "version": "0.1.0",
                    "checks": [
                        { "id": "disk", "level": "warn", "detail": "only 2 GB left", "extra": 1 },
                        { "id": "ssh", "level": "bogus", "detail": "dropped" },
                    ],
                    "metrics": { "disk_free_mb": 2048, "load1": 0.5, "uptime_s": null, "secret": 1 },
                    "units": { "stonewave-sshd": true, "sshd": false, "other": true },
                    "something_else": "not passed on",
                },
            })),
            trusted_devices: 2,
            agent_ssh: "active".into(),
            daemon: "active".into(),
        }
    }

    #[test]
    fn a_connected_server() {
        let v = report_json(&inputs());
        assert_eq!(v["readable"], true);
        assert_eq!(v["set_up"], true);
        assert_eq!(v["connected"], true);
        assert_eq!(v["access_revoked"], false);
        assert_eq!(v["server"], json!({ "name": "web-01", "company": "acme" }));
        assert_eq!(v["line"]["state"], "connected");
        assert_eq!(v["line"]["age_s"], 100);
        assert_eq!(v["daemon"], "active");
        assert_eq!(v["agent_ssh"]["state"], "active");
        assert_eq!(v["agent_ssh"]["address"], "127.0.0.1:2222");
        assert_eq!(v["agent_ssh"]["trusted_devices"], 2);
        assert_eq!(v["health"]["age_s"], 60);
        assert_eq!(v["health"]["metrics"]["disk_free_mb"], 2048);
        assert!(v["health"]["metrics"]["uptime_s"].is_null());
        assert!(
            v["health"]["metrics"]["updates"].is_null(),
            "missing is null"
        );
    }

    #[test]
    fn health_is_rebuilt_and_unknown_fields_are_dropped() {
        let h = report_json(&inputs())["health"].clone();
        let keys: Vec<&String> = h.as_object().unwrap().keys().collect();
        assert_eq!(keys.len(), 6, "{keys:?}");
        assert_eq!(
            h["checks"].as_array().unwrap().len(),
            1,
            "a check with an unknown level is dropped"
        );
        assert!(h["checks"][0].get("extra").is_none());
        assert!(h["metrics"].get("secret").is_none());
        assert_eq!(h["units"], json!({ "stonewave-sshd": true, "sshd": false }));
        assert!(!h.to_string().contains("not passed on"));
    }

    #[test]
    fn no_key_material_and_no_server_ref() {
        let mut i = inputs();
        i.tunnel = Some(json!({ "state": "reconnecting", "detail": "refused: x", "ts": 1u64 }));
        let text = report_json(&i).to_string();
        assert!(
            !text.contains(REF),
            "the server ref is not shown by status, so it is not here"
        );
        assert!(!text.contains("relay.example"));
        assert!(!text.to_lowercase().contains("seed"));
        assert!(!text.contains("server.key"));
    }

    #[test]
    fn not_set_up_is_a_report_not_an_error() {
        let mut i = inputs();
        i.state = Access::Missing;
        let v = report_json(&i);
        assert_eq!(v["readable"], true);
        assert_eq!(v["set_up"], false);
        assert_eq!(v["connected"], false);
        assert!(v["server"].is_null() && v["health"].is_null() && v["daemon"].is_null());
    }

    #[test]
    fn a_folder_we_may_not_read_is_not_called_not_set_up() {
        let mut i = inputs();
        i.state = Access::Denied;
        let v = report_json(&i);
        assert_eq!(v["readable"], false);
        assert!(v["set_up"].is_null());
        assert!(v["hint"].as_str().unwrap().contains("sudo"));
        assert_eq!(v["connected"], false);
    }

    #[test]
    fn a_stale_connected_file_with_a_dead_daemon_is_not_connected() {
        let mut i = inputs();
        i.daemon = "inactive".into();
        let v = report_json(&i);
        assert_eq!(v["connected"], false);
        assert_eq!(
            v["line"]["state"], "connected",
            "the file is shown as it is"
        );
        assert_eq!(v["daemon"], "inactive");
    }

    #[test]
    fn revoked_is_never_connected() {
        let mut i = inputs();
        i.state = Access::Found(json!({ "revoked": true }));
        i.tunnel = Some(
            json!({ "state": "revoked", "detail": "Wave cut this server off", "ts": 1_000_050u64 }),
        );
        let v = report_json(&i);
        assert_eq!(v["access_revoked"], true);
        assert_eq!(v["connected"], false);
        assert_eq!(v["line"]["state"], "revoked");
    }

    #[test]
    fn set_up_locally_without_wave_has_no_server_and_no_line() {
        let mut i = inputs();
        i.identity = None;
        let v = report_json(&i);
        assert!(v["server"].is_null() && v["line"].is_null());
        assert_eq!(v["connected"], false);
        assert_eq!(v["set_up"], true);
    }

    #[test]
    fn strange_text_is_tamed_and_the_output_is_one_line() {
        let mut i = inputs();
        i.tunnel = Some(json!({ "state": "x", "detail": "a\u{1b}[31mb\nc", "ts": 1u64 }));
        let text = report_json(&i).to_string();
        assert!(!text.contains('\u{1b}'));
        assert!(!text.contains('\n'), "one physical line");
        assert!(text.contains("a?[31mb?c"));
    }
}
