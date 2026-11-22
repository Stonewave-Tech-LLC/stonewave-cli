//! `stonewave agent ...` as a person or a script sees it: the real binary, a temporary home folder given to the child process
//! through its own environment (the test process' environment is never changed, the real home folder is never touched).

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

const KEY: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_cliTestKeyCliTestKey";

struct Home(PathBuf);

impl Home {
    fn new() -> Home {
        static N: AtomicU32 = AtomicU32::new(0);
        let p = std::env::temp_dir().join(format!(
            "swagent-cli-{}-{}",
            std::process::id(),
            N.fetch_add(1, Ordering::Relaxed)
        ));
        std::fs::create_dir_all(&p).unwrap();
        Home(p)
    }

    fn config(&self) -> PathBuf {
        if cfg!(windows) {
            self.0.join("roaming").join("wave")
        } else {
            self.0.join(".config").join("wave")
        }
    }

    fn command(&self) -> Command {
        let mut c = Command::new(env!("CARGO_BIN_EXE_stonewave"));
        c.env("HOME", &self.0)
            .env("USERPROFILE", &self.0)
            .env("APPDATA", self.0.join("roaming"))
            .env("LOCALAPPDATA", self.0.join("local"))
            .env_remove("XDG_CONFIG_HOME")
            .env_remove("XDG_STATE_HOME")
            .env_remove("CODEX_HOME")
            .env_remove("NO_COLOR")
            .env_remove("STONEWAVE_ACCOUNT_URL")
            .env_remove("STONEWAVE_WAVE_URL")
            .env_remove("WAVE_AGENT_KEY");
        c
    }

    fn put_key(&self, agent: &str) {
        std::fs::create_dir_all(self.config()).unwrap();
        std::fs::write(
            self.config().join(format!("{agent}.key")),
            format!("{KEY}\n"),
        )
        .unwrap();
    }
}

impl Drop for Home {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

fn text(o: &Output) -> String {
    format!(
        "{}{}",
        String::from_utf8_lossy(&o.stdout),
        String::from_utf8_lossy(&o.stderr)
    )
}

#[test]
fn help_lists_the_commands_and_the_files() {
    let h = Home::new();
    let out = h.command().args(["agent", "--help"]).output().unwrap();
    assert!(out.status.success());
    let t = text(&out);
    for w in [
        "connect",
        "status",
        "rotate",
        "disconnect",
        "run",
        "--keep-remote",
        "<agent>.key",
    ] {
        assert!(t.contains(w), "{w}");
    }
    // the main help points here
    let out = h.command().arg("--help").output().unwrap();
    assert!(text(&out).contains("stonewave agent --help"));
}

#[test]
fn bad_words_and_options_are_exit_2() {
    let h = Home::new();
    for args in [
        vec!["agent", "deploy"],
        vec!["agent", "status", "--yes"],
        vec!["agent", "connect", "--engine", "cursor"],
        vec!["agent", "run"],
    ] {
        let out = h.command().args(&args).output().unwrap();
        assert_eq!(out.status.code(), Some(2), "{args:?}: {}", text(&out));
    }
}

#[test]
fn status_json_with_nothing_connected_is_one_object_and_exit_0() {
    let h = Home::new();
    let out = h
        .command()
        .args(["agent", "status", "--json"])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    let t = String::from_utf8(out.stdout).unwrap();
    assert_eq!(t.lines().count(), 1, "{t}");
    assert!(!t.contains('\u{1b}'));
    let v: serde_json::Value = serde_json::from_str(t.trim()).unwrap();
    assert_eq!(v["connected"], false);
    assert!(
        v["reason"]
            .as_str()
            .unwrap()
            .contains("no agent is connected")
    );
    assert!(!h.config().exists(), "reading changes nothing");
}

#[test]
fn run_puts_the_key_into_the_child_only_and_passes_the_exit_code_on() {
    let h = Home::new();
    h.put_key("agent-demo");
    let (shell, flag) = if cfg!(windows) {
        ("cmd", "/c")
    } else {
        ("sh", "-c")
    };
    let echo = if cfg!(windows) {
        "echo %WAVE_AGENT_KEY%"
    } else {
        "printf '%s' \"$WAVE_AGENT_KEY\""
    };
    let out = h
        .command()
        .args(["agent", "run", "--", shell, flag, echo])
        .output()
        .unwrap();
    assert!(out.status.success(), "{}", text(&out));
    assert!(
        String::from_utf8_lossy(&out.stdout).contains(KEY),
        "the child sees the key"
    );
    // the exit code of the child is the exit code of the command
    let code = h
        .command()
        .args([
            "agent",
            "run",
            "--agent",
            "agent-demo",
            "--",
            shell,
            flag,
            "exit 7",
        ])
        .output()
        .unwrap();
    assert_eq!(code.status.code(), Some(7), "{}", text(&code));
    // the key is not in the environment of anything else: a second agent file makes --agent necessary
    h.put_key("agent-other");
    let out = h
        .command()
        .args(["agent", "run", "--", shell, flag, echo])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).contains("--agent"), "{}", text(&out));
    assert!(!text(&out).contains(KEY), "an error never shows the key");
}

#[test]
fn run_without_a_key_or_with_a_missing_program_says_so_and_never_prints_a_key() {
    let h = Home::new();
    let out = h
        .command()
        .args(["agent", "run", "--", "whatever"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).contains("no agent is connected"));
    h.put_key("agent-demo");
    let out = h
        .command()
        .args(["agent", "run", "--", "definitely-not-a-program-xyz"])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(2));
    assert!(text(&out).contains("not found"), "{}", text(&out));
    assert!(!text(&out).contains("cliTestKey"));
}

#[test]
fn connect_with_a_missing_engine_program_stops_before_any_network_and_writes_nothing() {
    let h = Home::new();
    let empty: &Path = &h.0;
    let out = h
        .command()
        .env("PATH", empty)
        .args([
            "agent",
            "connect",
            "--engine",
            "grok",
            "--account",
            "http://127.0.0.1:1",
        ])
        .output()
        .unwrap();
    assert_eq!(out.status.code(), Some(1), "{}", text(&out));
    let t = text(&out);
    assert!(t.contains("`grok` was not found"), "{t}");
    assert!(t.contains("Nothing was changed"));
    assert!(!h.config().exists());
}
