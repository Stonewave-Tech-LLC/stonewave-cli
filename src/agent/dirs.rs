//! Where the agent commands keep their files. Resolved from the environment in one place, with the environment passed in,
//! so tests point everything into a temporary folder without touching the process environment.
//!
//! - key, state, wake script: `~/.config/wave` (`$XDG_CONFIG_HOME/wave`)
//! - wake log: `~/.local/state/wave` (`$XDG_STATE_HOME/wave`)
//! - Windows: everything in `%LOCALAPPDATA%\wave`. Never the roaming `%APPDATA%`: nothing secret may roam to other machines.

use std::path::{Path, PathBuf};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Os {
    Linux,
    Mac,
    Windows,
}

impl Os {
    pub fn current() -> Os {
        if cfg!(windows) {
            Os::Windows
        } else if cfg!(target_os = "macos") {
            Os::Mac
        } else {
            Os::Linux
        }
    }
}

#[derive(Debug, Clone)]
pub struct Dirs {
    pub os: Os,
    pub home: PathBuf,
    /// Keys, state files and the wake script.
    pub config: PathBuf,
    /// Wake logs.
    pub state: PathBuf,
    /// Codex: `CODEX_HOME` or `~/.codex`.
    pub codex_home: PathBuf,
    pub systemd_user: PathBuf,
    pub launch_agents: PathBuf,
    /// The account name icacls grants access to (Windows only).
    pub win_user: String,
}

fn absolute(v: Option<String>) -> Option<PathBuf> {
    v.filter(|s| !s.trim().is_empty())
        .map(PathBuf::from)
        .filter(|p| p.is_absolute())
}

impl Dirs {
    /// `get` reads one environment variable.
    pub fn resolve(os: Os, get: &dyn Fn(&str) -> Option<String>) -> Result<Dirs, String> {
        let win_user = match (get("USERDOMAIN"), get("USERNAME")) {
            (Some(d), Some(u)) if !d.is_empty() && !u.is_empty() => format!("{d}\\{u}"),
            (_, Some(u)) => u,
            _ => String::new(),
        };
        let home = absolute(get(if os == Os::Windows {
            "USERPROFILE"
        } else {
            "HOME"
        }))
        .ok_or("cannot find your home folder (HOME is not set)")?;
        let codex_home = absolute(get("CODEX_HOME")).unwrap_or_else(|| home.join(".codex"));
        let (config, state, systemd_user) = if os == Os::Windows {
            let local = absolute(get("LOCALAPPDATA")).ok_or(
                "cannot find your local application data folder (LOCALAPPDATA is not set)",
            )?;
            (
                local.join("wave"),
                local.join("wave"),
                home.join(".config").join("systemd").join("user"),
            )
        } else {
            let cfg = absolute(get("XDG_CONFIG_HOME")).unwrap_or_else(|| home.join(".config"));
            let st = absolute(get("XDG_STATE_HOME"))
                .unwrap_or_else(|| home.join(".local").join("state"));
            (
                cfg.join("wave"),
                st.join("wave"),
                cfg.join("systemd").join("user"),
            )
        };
        Ok(Dirs {
            os,
            launch_agents: home.join("Library").join("LaunchAgents"),
            home,
            config,
            state,
            codex_home,
            systemd_user,
            win_user,
        })
    }

    pub fn system() -> Result<Dirs, String> {
        Dirs::resolve(Os::current(), &|k| std::env::var(k).ok())
    }

    /// Everything below `root` (tests).
    #[cfg(test)]
    pub fn under(root: &Path) -> Dirs {
        Dirs {
            os: Os::current(),
            home: root.join("home"),
            config: root.join("config").join("wave"),
            state: root.join("state").join("wave"),
            codex_home: root.join("home").join(".codex"),
            systemd_user: root.join("config").join("systemd").join("user"),
            launch_agents: root.join("home").join("Library").join("LaunchAgents"),
            win_user: "TESTPC\\tester".into(),
        }
    }

    pub fn key_file(&self, agent: &str) -> PathBuf {
        self.config.join(format!("{agent}.key"))
    }

    pub fn state_file(&self, agent: &str) -> PathBuf {
        self.config.join(format!("{agent}.json"))
    }

    pub fn wake_script(&self) -> PathBuf {
        self.config.join("fleet-watch.mjs")
    }

    pub fn wake_log(&self, agent: &str) -> PathBuf {
        self.state.join(format!("{agent}.wake.log"))
    }

    pub fn codex_config(&self) -> PathBuf {
        self.codex_home.join("config.toml")
    }

    pub fn claude_config(&self) -> PathBuf {
        self.home.join(".claude.json")
    }

    pub fn grok_config(&self) -> PathBuf {
        self.home.join(".grok").join("config.toml")
    }
}

/// A path as text for files we write (state, service files). Paths that are not valid UTF-8 are refused earlier.
pub fn text(p: &Path) -> String {
    p.to_string_lossy().into_owned()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn env(pairs: &[(&str, &str)]) -> impl Fn(&str) -> Option<String> {
        let m: HashMap<String, String> = pairs
            .iter()
            .map(|(k, v)| ((*k).to_owned(), (*v).to_owned()))
            .collect();
        move |k| m.get(k).cloned()
    }

    /// An absolute path that exists in form on every system.
    fn abs(rest: &str) -> String {
        let root = if cfg!(windows) { "C:/r" } else { "/r" };
        format!("{root}/{rest}")
    }

    fn p(rest: &str) -> PathBuf {
        PathBuf::from(abs(rest))
    }

    #[test]
    fn unix_defaults_and_xdg() {
        let home = abs("home/ada");
        let d = Dirs::resolve(Os::Linux, &env(&[("HOME", &home)])).unwrap();
        assert_eq!(d.config, p("home/ada/.config/wave"));
        assert_eq!(d.state, p("home/ada/.local/state/wave"));
        assert_eq!(d.codex_home, p("home/ada/.codex"));
        assert_eq!(d.systemd_user, p("home/ada/.config/systemd/user"));
        let (cfg, st, cx) = (abs("x/cfg"), abs("x/st"), abs("x/codex"));
        let d = Dirs::resolve(
            Os::Linux,
            &env(&[
                ("HOME", &home),
                ("XDG_CONFIG_HOME", &cfg),
                ("XDG_STATE_HOME", &st),
                ("CODEX_HOME", &cx),
            ]),
        )
        .unwrap();
        assert_eq!(d.config, p("x/cfg/wave"));
        assert_eq!(d.state, p("x/st/wave"));
        assert_eq!(d.codex_home, p("x/codex"));
        // relative values are not taken
        let d = Dirs::resolve(
            Os::Linux,
            &env(&[("HOME", &home), ("XDG_CONFIG_HOME", "cfg")]),
        )
        .unwrap();
        assert_eq!(d.config, p("home/ada/.config/wave"));
        assert!(Dirs::resolve(Os::Linux, &env(&[])).is_err());
        assert!(Dirs::resolve(Os::Linux, &env(&[("HOME", "rel")])).is_err());
    }

    #[test]
    fn windows_keeps_everything_in_local_appdata_never_in_the_roaming_profile() {
        let d = Dirs::resolve(
            Os::Windows,
            &env(&[
                ("USERPROFILE", "C:\\Users\\ada"),
                ("APPDATA", "C:\\Users\\ada\\AppData\\Roaming"),
                ("LOCALAPPDATA", "C:\\Users\\ada\\AppData\\Local"),
                ("USERDOMAIN", "PC"),
                ("USERNAME", "ada"),
            ]),
        );
        // on a Unix test machine these are not absolute paths; the shape is what is checked
        if cfg!(windows) {
            let d = d.unwrap();
            assert!(d.config.ends_with("Local\\wave"), "{:?}", d.config);
            assert!(d.state.ends_with("Local\\wave"));
            assert!(!d.config.to_string_lossy().contains("Roaming"));
            assert!(!d.state.to_string_lossy().contains("Roaming"));
            assert_eq!(d.win_user, "PC\\ada");
            // the roaming folder is not even looked at: without LOCALAPPDATA there is no place for the key
            let none = Dirs::resolve(
                Os::Windows,
                &env(&[
                    ("USERPROFILE", "C:\\Users\\ada"),
                    ("APPDATA", "C:\\Users\\ada\\AppData\\Roaming"),
                ]),
            );
            assert!(none.unwrap_err().contains("LOCALAPPDATA"));
        }
    }
}
