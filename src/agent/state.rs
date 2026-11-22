//! `~/.config/wave/<agent>.json`: what this CLI made for an agent, so `disconnect` removes exactly that and nothing else.
//! There is never a key in it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use super::key::atomic_write;

pub const STATE_VERSION: u64 = 1;

/// The `wave` entry in an engine.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EntryRec {
    /// `claude-code`, `codex` or `grok`.
    pub kind: String,
    pub name: String,
    /// Codex: the config file the block is in.
    pub file: Option<String>,
}

/// The wake service.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServiceRec {
    /// `systemd`, `launchd` or `schtasks`.
    pub kind: String,
    /// Unit name, label or task name.
    pub name: String,
    /// Files written for it (unit, plist, wrapper).
    pub files: Vec<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct State {
    pub agent: String,
    pub agent_name: String,
    pub agent_id: String,
    pub team: String,
    pub team_name: String,
    pub company: String,
    pub engine: String,
    pub wave_url: String,
    pub mcp_url: String,
    pub account_url: String,
    pub created: String,
    pub key_expires_at: String,
    pub key_file: String,
    pub mcp_entry: Option<EntryRec>,
    pub service: Option<ServiceRec>,
    /// The wake script (shared by all agents on this machine).
    pub script: Option<String>,
    pub wake_log: Option<String>,
}

impl State {
    pub fn to_json(&self) -> Value {
        json!({
            "version": STATE_VERSION,
            "stonewave": crate::VERSION,
            "agent": self.agent,
            "agent_name": self.agent_name,
            "agent_id": self.agent_id,
            "team": self.team,
            "team_name": self.team_name,
            "company": self.company,
            "engine": self.engine,
            "urls": { "wave": self.wave_url, "mcp": self.mcp_url, "account": self.account_url },
            "created": self.created,
            "key_expires_at": self.key_expires_at,
            "made": {
                "key_file": self.key_file,
                "mcp_entry": self.mcp_entry.as_ref().map(|e| json!({"kind": e.kind, "name": e.name, "file": e.file})),
                "service": self.service.as_ref().map(|s| json!({"kind": s.kind, "name": s.name, "files": s.files})),
                "script": self.script,
                "wake_log": self.wake_log,
            }
        })
    }

    pub fn from_json(v: &Value) -> Option<State> {
        if v.get("version")?.as_u64()? != STATE_VERSION {
            return None;
        }
        let s = |p: &[&str]| -> Option<String> {
            let mut c = v;
            for k in p {
                c = c.get(k)?;
            }
            c.as_str().map(str::to_owned)
        };
        let made = v.get("made")?;
        let opt = |k: &str| made.get(k).and_then(Value::as_str).map(str::to_owned);
        let mcp_entry = match made.get("mcp_entry") {
            Some(e) if e.is_object() => Some(EntryRec {
                kind: e.get("kind")?.as_str()?.to_owned(),
                name: e.get("name")?.as_str()?.to_owned(),
                file: e.get("file").and_then(Value::as_str).map(str::to_owned),
            }),
            _ => None,
        };
        let service = match made.get("service") {
            Some(e) if e.is_object() => Some(ServiceRec {
                kind: e.get("kind")?.as_str()?.to_owned(),
                name: e.get("name")?.as_str()?.to_owned(),
                files: e
                    .get("files")?
                    .as_array()?
                    .iter()
                    .filter_map(|f| f.as_str().map(str::to_owned))
                    .collect(),
            }),
            _ => None,
        };
        let agent = s(&["agent"])?;
        if !crate::connect::is_agent_slug(&agent) {
            return None;
        }
        Some(State {
            agent,
            agent_name: s(&["agent_name"])?,
            agent_id: s(&["agent_id"])?,
            team: s(&["team"])?,
            team_name: s(&["team_name"])?,
            company: s(&["company"])?,
            engine: s(&["engine"])?,
            wave_url: s(&["urls", "wave"])?,
            mcp_url: s(&["urls", "mcp"])?,
            account_url: s(&["urls", "account"])?,
            created: s(&["created"])?,
            key_expires_at: s(&["key_expires_at"])?,
            key_file: opt("key_file")?,
            mcp_entry,
            service,
            script: opt("script"),
            wake_log: opt("wake_log"),
        })
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(&self.to_json()).expect("state is JSON");
        text.push('\n');
        atomic_write(path, text.as_bytes(), 0o600)
    }

    pub fn load(path: &Path) -> Result<State, String> {
        let text = std::fs::read_to_string(path).map_err(|e| {
            format!(
                "cannot read {}: {}",
                path.display(),
                super::key::scrub(&e.to_string())
            )
        })?;
        let v: Value = serde_json::from_str(&text)
            .map_err(|_| format!("{} is not a state file of this tool", path.display()))?;
        State::from_json(&v).ok_or_else(|| {
            format!(
                "{} is not a state file of this tool (or from a newer version)",
                path.display()
            )
        })
    }
}

/// The agents that have a state file in `config`, by agent key.
pub fn list(config: &Path) -> Vec<(String, PathBuf)> {
    let mut found = Vec::new();
    let Ok(rd) = std::fs::read_dir(config) else {
        return found;
    };
    for e in rd.flatten() {
        let p = e.path();
        let Some(stem) = p
            .file_name()
            .and_then(|n| n.to_str())
            .and_then(|n| n.strip_suffix(".json"))
        else {
            continue;
        };
        if crate::connect::is_agent_slug(stem) && State::load(&p).is_ok() {
            found.push((stem.to_owned(), p));
        }
    }
    found.sort();
    found
}

/// Agents that have a key file (for `agent run`).
pub fn list_keys(config: &Path) -> Vec<String> {
    let mut found = Vec::new();
    let Ok(rd) = std::fs::read_dir(config) else {
        return found;
    };
    for e in rd.flatten() {
        if let Some(stem) = e
            .file_name()
            .to_str()
            .and_then(|n| n.strip_suffix(".key"))
            .filter(|s| crate::connect::is_agent_slug(s))
        {
            found.push(stem.to_owned());
        }
    }
    found.sort();
    found
}

#[cfg(test)]
pub fn sample(agent: &str) -> State {
    State {
        agent: agent.into(),
        agent_name: "Grok (Mac)".into(),
        agent_id: "11111111-2222-4333-8444-555555555555".into(),
        team: "ops".into(),
        team_name: "Ops".into(),
        company: "acme".into(),
        engine: "grok".into(),
        wave_url: "https://wave.test".into(),
        mcp_url: "https://wave.test/mcp".into(),
        account_url: "https://account.test".into(),
        created: "2026-11-22T10:00:00Z".into(),
        key_expires_at: "2027-02-20T10:00:00Z".into(),
        key_file: format!("/cfg/wave/{agent}.key"),
        mcp_entry: Some(EntryRec {
            kind: "grok".into(),
            name: "wave".into(),
            file: None,
        }),
        service: Some(ServiceRec {
            kind: "systemd".into(),
            name: format!("stonewave-wave-{agent}.service"),
            files: vec![format!("/cfg/systemd/user/stonewave-wave-{agent}.service")],
        }),
        script: Some("/cfg/wave/fleet-watch.mjs".into()),
        wake_log: Some(format!("/state/wave/{agent}.wake.log")),
    }
}

#[cfg(test)]
mod tests {
    use super::super::key::testdir::TempDir;
    use super::*;

    #[test]
    fn round_trip_through_a_file_and_no_key_inside() {
        let t = TempDir::new("state");
        let st = sample("agent-demo");
        let f = t.path().join("agent-demo.json");
        st.save(&f).unwrap();
        assert_eq!(State::load(&f).unwrap(), st);
        let text = std::fs::read_to_string(&f).unwrap();
        assert!(!text.contains("wak_"));
        let mut bare = st.clone();
        bare.mcp_entry = None;
        bare.service = None;
        bare.script = None;
        bare.wake_log = None;
        bare.save(&f).unwrap();
        assert_eq!(State::load(&f).unwrap(), bare);
    }

    #[test]
    fn foreign_json_is_not_a_state_file() {
        assert!(State::from_json(&json!({"hello": 1})).is_none());
        let mut v = sample("agent-demo").to_json();
        v["agent"] = json!("../evil");
        assert!(State::from_json(&v).is_none());
        let mut v = sample("agent-demo").to_json();
        v["version"] = json!(99);
        assert!(State::from_json(&v).is_none());
    }

    #[test]
    fn listing_finds_state_files_and_key_files_by_slug() {
        let t = TempDir::new("list");
        sample("agent-b")
            .save(&t.path().join("agent-b.json"))
            .unwrap();
        sample("agent-a")
            .save(&t.path().join("agent-a.json"))
            .unwrap();
        std::fs::write(t.path().join("other.json"), "{}").unwrap();
        std::fs::write(t.path().join("agent-a.key"), "k").unwrap();
        std::fs::write(t.path().join("Bad Name.key"), "k").unwrap();
        std::fs::write(t.path().join("fleet-watch.mjs"), "x").unwrap();
        let names: Vec<String> = list(t.path()).into_iter().map(|x| x.0).collect();
        assert_eq!(names, vec!["agent-a", "agent-b"]);
        assert_eq!(list_keys(t.path()), vec!["agent-a"]);
    }
}
