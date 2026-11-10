//! `stonewave status`: what is true on this machine right now. It reads files and asks systemd; it talks to nobody.

use crate::banner;
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
