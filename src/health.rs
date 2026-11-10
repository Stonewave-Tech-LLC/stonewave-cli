//! The health report: the `doctor` checks plus a few fixed numbers. Read only. It is what the daemon
//! sends to Wave once a minute and what `agent-shell health` and `stonewave status` show.
//!
//! The shape is fixed on purpose (the relay rebuilds it field by field and drops everything else), and
//! the daemon never reports file names, process lists or log lines.

use serde_json::{Value, json};

use crate::doctor::{self, Level, Probe};

fn num(s: &str) -> Option<f64> {
    s.trim()
        .parse::<f64>()
        .ok()
        .filter(|v| v.is_finite() && *v >= 0.0)
}

/// `MemTotal`/`MemAvailable` from `/proc/meminfo`, in megabytes.
pub fn parse_meminfo(text: &str, key: &str) -> Option<u64> {
    let line = text
        .lines()
        .find(|l| l.starts_with(key) && l[key.len()..].starts_with(':'))?;
    let kb: u64 = line[key.len() + 1..]
        .split_whitespace()
        .next()?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

pub fn parse_loadavg(text: &str) -> Option<f64> {
    num(text.split_whitespace().next()?)
}

pub fn parse_uptime(text: &str) -> Option<u64> {
    num(text.split_whitespace().next()?).map(|s| s as u64)
}

/// Total megabytes from `df -Pk <path>` output.
pub fn parse_df_total_mb(text: &str) -> Option<u64> {
    let kb: u64 = text
        .lines()
        .nth(1)?
        .split_whitespace()
        .nth(1)?
        .parse()
        .ok()?;
    Some(kb / 1024)
}

/// "5 updates can be applied immediately." (Ubuntu's update-notifier) → 5.
pub fn parse_updates(text: &str) -> Option<u64> {
    text.lines()
        .find(|l| l.contains("can be applied"))?
        .split_whitespace()
        .next()?
        .parse()
        .ok()
}

fn level(l: Level) -> &'static str {
    match l {
        Level::Ok => "ok",
        Level::Warn => "warn",
        Level::Fail => "fail",
        Level::Info => "info",
    }
}

fn active(p: &dyn Probe, unit: &str) -> bool {
    p.run("systemctl", &["is-active", unit]).as_deref() == Some("active")
}

/// The report as a JSON value. Anything that cannot be read is `null`, never a guess.
pub fn report(p: &dyn Probe, ts: u64) -> Value {
    let checks: Vec<Value> = doctor::run_all(p)
        .into_iter()
        .map(|c| json!({ "id": c.id, "level": level(c.level), "detail": c.detail }))
        .collect();
    let df = p.run("df", &["-Pk", "/"]);
    let mem = p.read("/proc/meminfo");
    let ssh_active = active(p, "ssh") || active(p, "sshd");
    json!({
        "ts": ts,
        "version": crate::VERSION,
        "checks": checks,
        "metrics": {
            "disk_free_mb": df.as_deref().and_then(doctor::parse_df_free_mb),
            "disk_total_mb": df.as_deref().and_then(parse_df_total_mb),
            "mem_available_mb": mem.as_deref().and_then(|t| parse_meminfo(t, "MemAvailable")),
            "mem_total_mb": mem.as_deref().and_then(|t| parse_meminfo(t, "MemTotal")),
            "load1": p.read("/proc/loadavg").as_deref().and_then(parse_loadavg),
            "uptime_s": p.read("/proc/uptime").as_deref().and_then(parse_uptime),
            "updates": p.read("/var/lib/update-notifier/updates-available").as_deref().and_then(parse_updates),
        },
        "units": {
            "stonewave-sshd": active(p, "stonewave-sshd"),
            "sshd": ssh_active,
        },
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    struct Fake;
    impl Probe for Fake {
        fn read(&self, path: &str) -> Option<String> {
            Some(
                match path {
                    "/proc/meminfo" => {
                        "MemTotal:        8123456 kB\nMemFree: 1 kB\nMemAvailable:    4096000 kB\n"
                    }
                    "/proc/loadavg" => "0.42 0.30 0.20 1/200 1234\n",
                    "/proc/uptime" => "86400.55 1000.00\n",
                    "/var/lib/update-notifier/updates-available" => {
                        "\n5 updates can be applied immediately.\n"
                    }
                    "/etc/os-release" => {
                        "ID=ubuntu\nVERSION_ID=\"24.04\"\nPRETTY_NAME=\"Ubuntu 24.04 LTS\"\n"
                    }
                    _ => return None,
                }
                .to_owned(),
            )
        }
        fn exists(&self, path: &str) -> bool {
            path == "/run/systemd/system"
        }
        fn run(&self, cmd: &str, args: &[&str]) -> Option<String> {
            match (cmd, args) {
                ("df", _) => Some("Filesystem 1024-blocks Used Available Capacity Mounted on\n/dev/sda1 100000000 40000000 60000000 40% /".into()),
                ("systemctl", ["is-active", "stonewave-sshd"]) => Some("active".into()),
                ("systemctl", ["is-active", "ssh"]) => Some("inactive".into()),
                ("systemctl", ["is-active", "sshd"]) => Some("active".into()),
                _ => None,
            }
        }
        fn connect(&self, _: &str, _: u16) -> std::result::Result<Duration, String> {
            Ok(Duration::from_millis(5))
        }
        fn env(&self, _: &str) -> Option<String> {
            None
        }
    }

    #[test]
    fn parsers() {
        assert_eq!(
            parse_meminfo("MemTotal: 2048 kB\nMemAvailable: 1024 kB\n", "MemAvailable"),
            Some(1)
        );
        assert_eq!(parse_meminfo("MemTotal: 2048 kB\n", "MemAvailable"), None);
        assert_eq!(
            parse_meminfo("MemTotalX: 2048 kB\n", "MemTotal"),
            None,
            "the key must be the whole word"
        );
        assert_eq!(parse_loadavg("1.50 1 1"), Some(1.5));
        assert_eq!(parse_loadavg("nan 1 1"), None);
        assert_eq!(parse_loadavg("-1 1 1"), None);
        assert_eq!(parse_uptime("90.9 1"), Some(90));
        assert_eq!(parse_df_total_mb("h\n/dev/x 2048 1 1024 1% /"), Some(2));
        assert_eq!(
            parse_updates("1 update can be applied immediately."),
            Some(1)
        );
        assert_eq!(
            parse_updates("12 updates can be applied immediately."),
            Some(12)
        );
        assert_eq!(
            parse_updates("nothing here"),
            None,
            "no line, no number: null, not a guess"
        );
    }

    #[test]
    fn the_report_has_the_fixed_shape() {
        let v = report(&Fake, 1_700_000_000);
        assert_eq!(v["ts"], 1_700_000_000u64);
        assert_eq!(v["version"], crate::VERSION);
        assert_eq!(v["metrics"]["disk_free_mb"], 58593u64);
        assert_eq!(v["metrics"]["disk_total_mb"], 97656u64);
        assert_eq!(v["metrics"]["mem_available_mb"], 4000u64);
        assert_eq!(v["metrics"]["load1"], 0.42);
        assert_eq!(v["metrics"]["uptime_s"], 86400u64);
        assert_eq!(v["metrics"]["updates"], 5u64);
        assert_eq!(v["units"]["stonewave-sshd"], true);
        assert_eq!(v["units"]["sshd"], true, "ssh or sshd");
        let checks = v["checks"].as_array().unwrap();
        assert_eq!(checks.len(), 10, "the ten doctor checks");
        for c in checks {
            let id = c["id"].as_str().unwrap();
            assert!(id.chars().all(|ch| ch.is_ascii_lowercase() || ch == '_') && id.len() <= 24);
            assert!(["ok", "warn", "fail", "info"].contains(&c["level"].as_str().unwrap()));
            assert!(
                c["detail"].as_str().unwrap().chars().count() <= 200,
                "the relay cuts details at 200"
            );
        }
        let keys: Vec<&String> = v.as_object().unwrap().keys().collect();
        assert_eq!(
            keys.len(),
            5,
            "ts, version, checks, metrics, units and nothing else: {keys:?}"
        );
    }

    #[test]
    fn what_cannot_be_read_is_null() {
        struct Blind;
        impl Probe for Blind {
            fn read(&self, _: &str) -> Option<String> {
                None
            }
            fn exists(&self, _: &str) -> bool {
                false
            }
            fn run(&self, _: &str, _: &[&str]) -> Option<String> {
                None
            }
            fn connect(&self, _: &str, _: u16) -> std::result::Result<Duration, String> {
                Err("no".into())
            }
            fn env(&self, _: &str) -> Option<String> {
                None
            }
        }
        let v = report(&Blind, 1);
        assert!(v["metrics"]["disk_free_mb"].is_null());
        assert!(v["metrics"]["load1"].is_null());
        assert_eq!(v["units"]["stonewave-sshd"], false);
    }
}
