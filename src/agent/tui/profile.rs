//! `<agent>.profile.json` next to the key and state files: what the menu's wizard decided for an agent's station (rooms, working folder,
//! where the key comes from). It is only a memory of choices: the station itself is always started with explicit command-line options
//! (by the menu, or by the service file), and nothing here widens what a station may do. There is never a key in it.

use std::path::{Path, PathBuf};

use serde_json::{Value, json};

use crate::agent::dirs::Dirs;
use crate::agent::key::atomic_write;

pub const PROFILE_VERSION: u64 = 1;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Profile {
    pub agent: String,
    /// `claude-code` (the only engine a station can wake so far).
    pub engine: String,
    /// Address of Wave's API.
    pub wave_url: String,
    /// The name of the environment variable the key comes from; `None` = the key file `agent connect` made.
    pub key_env: Option<String>,
    /// Words that start the station through a secret store, for example `secrets-run run --env=prod --`. Only with `key_env`.
    pub launcher: Vec<String>,
    pub workdir: String,
    /// Ids of the rooms to serve.
    pub rooms: Vec<String>,
    /// Names of those rooms, for the screen only.
    pub room_names: Vec<String>,
}

/// A word of a command that may go into a service file: no quotes, no spaces, no characters a shell or systemd would read.
pub fn plain_word(s: &str) -> bool {
    !s.is_empty()
        && s.len() <= 200
        && s.chars()
            .all(|c| c.is_ascii_alphanumeric() || "-_./=:@+,".contains(c))
}

/// A full path: on this system, or written the Unix way (what a Linux server's service file holds).
pub fn absolute(p: &str) -> bool {
    Path::new(p).is_absolute() || p.starts_with('/')
}

pub fn plain_id(s: &str) -> bool {
    (1..=80).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

fn text_ok(s: &str, max: usize) -> bool {
    !s.is_empty() && s.chars().count() <= max && !s.chars().any(crate::agent::key::is_invisible)
}

impl Profile {
    pub fn to_json(&self) -> Value {
        json!({
            "version": PROFILE_VERSION,
            "stonewave": crate::VERSION,
            "agent": self.agent,
            "engine": self.engine,
            "wave_url": self.wave_url,
            "key_env": self.key_env,
            "launcher": self.launcher,
            "workdir": self.workdir,
            "rooms": self.rooms,
            "room_names": self.room_names,
        })
    }

    pub fn from_json(v: &Value) -> Option<Profile> {
        if v.get("version")?.as_u64()? != PROFILE_VERSION {
            return None;
        }
        let s = |k: &str| v.get(k)?.as_str().map(str::to_owned);
        let list = |k: &str| -> Option<Vec<String>> {
            v.get(k)?
                .as_array()?
                .iter()
                .map(|x| x.as_str().map(str::to_owned))
                .collect()
        };
        let agent = s("agent")?;
        if !crate::connect::is_agent_slug(&agent) {
            return None;
        }
        let key_env = match v.get("key_env") {
            Some(Value::String(n)) => {
                if !crate::agent::station_cli::env_name_ok(n) {
                    return None;
                }
                Some(n.clone())
            }
            _ => None,
        };
        let launcher = list("launcher")?;
        if !launcher.iter().all(|w| plain_word(w)) || (key_env.is_none() && !launcher.is_empty()) {
            return None;
        }
        let rooms = list("rooms")?;
        if rooms.is_empty() || !rooms.iter().all(|r| plain_id(r)) {
            return None;
        }
        let workdir = s("workdir")?;
        if !text_ok(&workdir, 400) || !absolute(&workdir) {
            return None;
        }
        let wave_url = s("wave_url")?;
        if !text_ok(&wave_url, 300) {
            return None;
        }
        // only an engine a station can wake, spelled the way this tool spells it
        let engine = s("engine")?;
        if crate::agent::engine::Engine::parse(&engine)
            .is_none_or(|e| e == crate::agent::engine::Engine::Other)
        {
            return None;
        }
        // names are for the screen only, but they are shown: no control, bidirectional or zero-width characters
        let room_names = list("room_names").unwrap_or_default();
        if room_names.len() > rooms.len() || !room_names.iter().all(|n| text_ok(n, 80)) {
            return None;
        }
        Some(Profile {
            agent,
            engine,
            wave_url,
            key_env,
            launcher,
            workdir,
            rooms,
            room_names,
        })
    }

    pub fn path(dirs: &Dirs, agent: &str) -> PathBuf {
        dirs.config.join(format!("{agent}.profile.json"))
    }

    pub fn save(&self, dirs: &Dirs) -> std::io::Result<()> {
        let mut text = serde_json::to_string_pretty(&self.to_json()).expect("a profile is JSON");
        text.push('\n');
        std::fs::create_dir_all(&dirs.config)?;
        atomic_write(&Profile::path(dirs, &self.agent), text.as_bytes(), 0o600)
    }

    pub fn load(dirs: &Dirs, agent: &str) -> Option<Profile> {
        let text = std::fs::read_to_string(Profile::path(dirs, agent)).ok()?;
        let p = Profile::from_json(&serde_json::from_str(&text).ok()?)?;
        (p.agent == agent).then_some(p)
    }

    pub fn remove(dirs: &Dirs, agent: &str) {
        let _ = std::fs::remove_file(Profile::path(dirs, agent));
    }
}

/// Agents that have a profile file.
pub fn list(dirs: &Dirs) -> Vec<Profile> {
    let Ok(rd) = std::fs::read_dir(&dirs.config) else {
        return Vec::new();
    };
    let mut found: Vec<Profile> = rd
        .flatten()
        .filter_map(|e| {
            let name = e.file_name().into_string().ok()?;
            let agent = name.strip_suffix(".profile.json")?;
            crate::connect::is_agent_slug(agent).then(|| Profile::load(dirs, agent))?
        })
        .collect();
    found.sort_by(|a, b| a.agent.cmp(&b.agent));
    found
}

#[cfg(test)]
pub fn sample(agent: &str) -> Profile {
    Profile {
        agent: agent.into(),
        engine: "claude-code".into(),
        wave_url: "https://wave.test".into(),
        key_env: Some("DEMO_WAVE_KEY".into()),
        launcher: vec![
            "/usr/local/bin/secrets-run".into(),
            "run".into(),
            "--env=prod".into(),
            "--".into(),
        ],
        workdir: "/home/alice/agent-station/work".into(),
        rooms: vec!["room-1".into(), "room-2".into()],
        room_names: vec!["Team room".into(), "Company room".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::key::testdir::TempDir;

    #[test]
    fn a_profile_round_trips_and_holds_no_key() {
        let t = TempDir::new("profile");
        let d = Dirs::under(t.path());
        let p = sample("demo");
        p.save(&d).unwrap();
        assert_eq!(Profile::load(&d, "demo"), Some(p.clone()));
        assert_eq!(list(&d), vec![p]);
        let text = std::fs::read_to_string(Profile::path(&d, "demo")).unwrap();
        assert!(!text.contains("wak_"));
    }

    #[test]
    fn foreign_or_unsafe_files_are_not_profiles() {
        let good = sample("demo").to_json();
        assert!(Profile::from_json(&good).is_some());
        for (k, v) in [
            ("agent", json!("../evil")),
            ("version", json!(9)),
            ("rooms", json!([])),
            ("rooms", json!(["a b"])),
            ("key_env", json!("not a name")),
            ("launcher", json!(["secrets-run", "run; rm -rf /"])),
            ("workdir", json!("relative/path")),
            ("engine", json!("not-an-engine")),
            ("engine", json!("other")),
            ("room_names", json!(["Team room", "Evil \u{202E}name"])),
            ("room_names", json!(["a\u{200B}b"])),
            ("room_names", json!(["a", "b", "c"])),
        ] {
            let mut j = good.clone();
            j[k] = v;
            assert!(Profile::from_json(&j).is_none(), "{k}");
        }
        // a launcher makes no sense without a variable to fill
        let mut j = good.clone();
        j["key_env"] = Value::Null;
        assert!(Profile::from_json(&j).is_none());
    }

    #[test]
    fn a_profile_of_another_agent_is_not_loaded_under_this_name() {
        let t = TempDir::new("profile2");
        let d = Dirs::under(t.path());
        sample("demo").save(&d).unwrap();
        std::fs::copy(Profile::path(&d, "demo"), Profile::path(&d, "other")).unwrap();
        assert!(Profile::load(&d, "other").is_none());
    }

    #[test]
    fn words_for_a_service_file_are_plain() {
        assert!(plain_word("--env=prod"));
        assert!(plain_word("/usr/local/bin/secrets-run"));
        for bad in [
            "", "a b", "a;b", "$HOME", "a`b`", "a\"b", "a|b", "a&b", "a\nb", "a'b", "%h",
        ] {
            assert!(!plain_word(bad), "{bad:?}");
        }
    }
}
