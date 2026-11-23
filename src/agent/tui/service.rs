//! A user service that keeps a station running (Linux, systemd `--user`; no root, no sudo).
//!
//! The unit holds nothing but explicit command-line options of `stonewave agent station run`: the same ones the menu shows before it
//! writes the file. The key is never in it: either the station reads the key file `agent connect` made, or the unit starts the station
//! through a secret-store command (`launcher`) that puts the key into the environment of that one process.
//!
//! macOS and Windows have no service installer yet: the menu says so and starts the station in the terminal instead.

use crate::agent::dirs::{Dirs, Os, text};
use crate::agent::exec::Exec;
use crate::agent::key::{atomic_write, scrub};
use crate::agent::station::{systemd_env, systemd_exec_word};

use super::profile::{Profile, plain_word};

pub fn unit_name(agent: &str) -> String {
    format!("stonewave-station-{agent}.service")
}

/// Whether this system has a service installer here.
pub fn supported(os: Os) -> bool {
    os == Os::Linux
}

/// The command of a station, as separate words: the program, then `agent station run ...`. With `claude` the full path of Claude Code (a
/// service has a much shorter PATH than a shell).
pub fn station_argv(p: &Profile, exe: &str, claude: Option<&str>) -> Vec<String> {
    let mut a: Vec<String> = [
        exe,
        "agent",
        "station",
        "run",
        "--agent",
        &p.agent,
        "--wave",
        &p.wave_url,
    ]
    .iter()
    .map(|s| (*s).to_owned())
    .collect();
    if let Some(n) = &p.key_env {
        a.push("--key-env".into());
        a.push(n.clone());
    }
    a.push("--workdir".into());
    a.push(p.workdir.clone());
    a.push("--rooms".into());
    a.push(p.rooms.join(","));
    if let Some(c) = claude {
        a.push("--claude".into());
        a.push(c.to_owned());
    }
    a
}

/// What the menu runs for a station in the foreground: the same options without the program.
pub fn station_args(p: &Profile, claude: Option<&str>, extra: &[&str]) -> Vec<String> {
    let mut a = station_argv(p, "x", claude);
    a.remove(0);
    a.remove(0); // "agent" stays out: `station ...` is what `agent::main` takes
    a.extend(extra.iter().map(|s| (*s).to_owned()));
    a
}

/// The whole command with the launcher in front, as it goes after `ExecStart=`.
pub fn exec_words(p: &Profile, exe: &str, claude: Option<&str>) -> Vec<String> {
    let mut words = p.launcher.clone();
    words.extend(station_argv(p, exe, claude));
    words
}

/// The unit file. `path_env` is the PATH of the shell the menu runs in, so a Claude Code installed with node in a user folder still finds
/// its node.
pub fn systemd_unit(
    p: &Profile,
    exe: &str,
    claude: Option<&str>,
    path_env: Option<&str>,
) -> String {
    let words: Vec<String> = exec_words(p, exe, claude)
        .iter()
        .map(|w| systemd_exec_word(w))
        .collect();
    let mut env = String::new();
    if let Some(path) = path_env.filter(|v| !v.is_empty() && !v.chars().any(char::is_control)) {
        env.push_str(&format!("Environment={}\n", systemd_env("PATH", path)));
    }
    format!(
        "[Unit]\n\
Description=Stonewave station for the Wave agent {agent}\n\
After=network-online.target\n\
Wants=network-online.target\n\
\n\
[Service]\n\
Type=simple\n\
Restart=on-failure\n\
RestartSec=30\n\
{env}\
ExecStart={exec}\n\
\n\
[Install]\n\
WantedBy=default.target\n",
        agent = p.agent,
        exec = words.join(" "),
    )
}

/// The launcher's first word must be a program that exists (a full path in the unit: a service has a short PATH).
pub fn resolve_launcher(
    p: &Profile,
    which: &dyn Fn(&str) -> Option<std::path::PathBuf>,
) -> Result<Profile, String> {
    let mut q = p.clone();
    if let Some(first) = p.launcher.first() {
        if !plain_word(first) {
            return Err(
                "the secret-store command has a word that cannot go into a service file".into(),
            );
        }
        let found = if super::profile::absolute(first) {
            Some(first.into())
        } else {
            which(first)
        };
        let found = found.ok_or_else(|| format!("`{first}` was not found on this computer"))?;
        let s = text(&found);
        if !plain_word(&s) {
            return Err(format!(
                "the path of `{first}` has characters that cannot go into a service file"
            ));
        }
        q.launcher[0] = s;
    }
    Ok(q)
}

/// Where the unit file goes.
pub fn unit_path(dirs: &Dirs, agent: &str) -> std::path::PathBuf {
    dirs.systemd_user.join(unit_name(agent))
}

/// `systemctl --user ...`, one call.
fn systemctl(exec: &dyn Exec, args: &[&str]) -> Result<String, String> {
    let mut all = vec!["--user".to_owned()];
    all.extend(args.iter().map(|s| (*s).to_owned()));
    match exec.run("systemctl", &all) {
        Ok(o) if o.ok => Ok(o.stdout),
        Ok(o) => Err(format!(
            "`systemctl --user {}` failed: {}",
            args.join(" "),
            scrub(&o.problem())
        )),
        Err(e) => Err(format!("cannot run systemctl: {}", scrub(&e.to_string()))),
    }
}

/// Writes the unit and starts it. Returns what to tell the person.
pub fn install(
    dirs: &Dirs,
    exec: &dyn Exec,
    p: &Profile,
    exe: &str,
    claude: Option<&str>,
    path_env: Option<&str>,
) -> Result<Vec<String>, String> {
    if !supported(dirs.os) {
        return Err(
            "a service can only be set up on Linux so far; start the station in a terminal".into(),
        );
    }
    let p = resolve_launcher(p, &|n| exec.which(n))?;
    let file = unit_path(dirs, &p.agent);
    let existed = file.exists();
    std::fs::create_dir_all(&dirs.systemd_user)
        .map_err(|e| format!("cannot make {}: {}", text(&dirs.systemd_user), e.kind()))?;
    atomic_write(
        &file,
        systemd_unit(&p, exe, claude, path_env).as_bytes(),
        0o644,
    )
    .map_err(|e| format!("cannot write {}: {}", text(&file), e.kind()))?;
    let name = unit_name(&p.agent);
    systemctl(exec, &["daemon-reload"])?;
    systemctl(exec, &["enable", "--now", &name])?;
    if existed {
        systemctl(exec, &["restart", &name])?;
    }
    let mut notes = vec![format!("wrote {} and started it", text(&file))];
    if let Ok(o) = exec.run(
        "loginctl",
        &[
            "show-user".into(),
            user_name(),
            "-p".into(),
            "Linger".into(),
        ],
    ) {
        if o.ok && o.stdout.contains("Linger=no") {
            notes.push("The service stops when you log out. To keep it running, run once: loginctl enable-linger".into());
        }
    }
    Ok(notes)
}

fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

pub fn control(exec: &dyn Exec, agent: &str, what: &str) -> Result<(), String> {
    systemctl(exec, &[what, &unit_name(agent)]).map(|_| ())
}

/// Stops and removes the unit file.
pub fn remove(dirs: &Dirs, exec: &dyn Exec, agent: &str) -> Result<(), String> {
    let name = unit_name(agent);
    let file = unit_path(dirs, agent);
    if !file.exists() {
        return Ok(());
    }
    // not running or not known to systemd any more is fine: the file goes either way
    let _ = systemctl(exec, &["disable", "--now", &name]);
    std::fs::remove_file(&file)
        .map_err(|e| format!("cannot remove {}: {}", text(&file), e.kind()))?;
    systemctl(exec, &["daemon-reload"]).map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// No unit file.
    None,
    Installed {
        active: bool,
    },
}

pub fn state(dirs: &Dirs, exec: &dyn Exec, agent: &str) -> ServiceState {
    if !supported(dirs.os) || !unit_path(dirs, agent).exists() {
        return ServiceState::None;
    }
    let active = exec
        .run(
            "systemctl",
            &["--user".into(), "is-active".into(), unit_name(agent)],
        )
        .is_ok_and(|o| o.ok && o.stdout.trim() == "active");
    ServiceState::Installed { active }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::exec::fake::{Fake, failed, ok_out};
    use crate::agent::key::testdir::TempDir;
    use crate::agent::tui::profile::sample;

    fn linux(t: &TempDir) -> Dirs {
        let mut d = Dirs::under(t.path());
        d.os = Os::Linux;
        d
    }

    #[test]
    fn the_command_has_every_option_spelled_out_and_no_key() {
        let p = sample("demo");
        let a = station_argv(&p, "/opt/sw/stonewave", Some("/usr/bin/claude"));
        assert_eq!(a[0], "/opt/sw/stonewave");
        let line = a.join(" ");
        assert!(
            line.contains(
                "agent station run --agent demo --wave https://wave.test --key-env DEMO_WAVE_KEY"
            ),
            "{line}"
        );
        assert!(
            line.contains("--rooms room-1,room-2") && line.contains("--claude /usr/bin/claude"),
            "{line}"
        );
        assert!(!line.contains("wak_"));
        // the menu runs the same without the program and without "agent"
        let b = station_args(&p, None, &["--dry-run"]);
        assert_eq!(b[0], "station");
        assert_eq!(b.last().unwrap(), "--dry-run");
        assert!(!b.iter().any(|w| w == "agent"));
    }

    #[test]
    fn the_unit_starts_the_station_through_the_secret_store_and_holds_no_key() {
        let p = sample("demo");
        let u = systemd_unit(
            &p,
            "/home/alice/agent-station/bin/stonewave",
            Some("/home/alice/.local/bin/claude"),
            Some("/usr/bin:/home/alice/.local/bin"),
        );
        let exec = u.lines().find(|l| l.starts_with("ExecStart=")).unwrap();
        assert!(
            exec.starts_with("ExecStart=\"/usr/local/bin/secrets-run\" \"run\" \"--env=prod\" \"--\" \"/home/alice/agent-station/bin/stonewave\" \"agent\" \"station\" \"run\""),
            "{exec}"
        );
        assert!(u.contains("Environment=\"PATH=/usr/bin:/home/alice/.local/bin\""));
        assert!(u.contains("Restart=on-failure") && u.contains("WantedBy=default.target"));
        assert!(!u.contains("wak_") && !u.contains("WAVE_AGENT_KEY"));
        // a key file agent has no launcher
        let mut f = sample("kim");
        f.key_env = None;
        f.launcher.clear();
        let u = systemd_unit(&f, "/x/stonewave", None, None);
        assert!(u.contains("ExecStart=\"/x/stonewave\" \"agent\""), "{u}");
        assert!(!u.contains("Environment="));
    }

    #[test]
    fn specifiers_and_variables_in_a_word_cannot_expand() {
        let mut p = sample("demo");
        p.workdir = "/home/a%h/$HOME".into();
        let u = systemd_unit(&p, "/x/stonewave", None, None);
        assert!(u.contains("\"/home/a%%h/$$HOME\""), "{u}");
    }

    #[test]
    fn the_launcher_must_exist_and_becomes_a_full_path() {
        let mut p = sample("demo");
        p.launcher = vec!["secrets-run".into(), "run".into(), "--".into()];
        let found = resolve_launcher(&p, &|n| {
            (n == "secrets-run").then(|| "/usr/local/bin/secrets-run".into())
        })
        .unwrap();
        assert_eq!(found.launcher[0], "/usr/local/bin/secrets-run");
        assert!(
            resolve_launcher(&p, &|_| None)
                .unwrap_err()
                .contains("not found")
        );
        let q = resolve_launcher(&sample("kim"), &|_| None).unwrap();
        assert_eq!(
            q.launcher,
            sample("kim").launcher,
            "an absolute launcher is kept as given"
        );
    }

    #[test]
    fn install_writes_the_unit_then_starts_it_and_remove_undoes_it() {
        let t = TempDir::new("svc");
        let d = linux(&t);
        let exec = Fake::new(&[]).with(|p, a| {
            if p == "loginctl" {
                Some(ok_out("Linger=no\n"))
            } else if p == "systemctl" && a.contains(&"is-active".to_owned()) {
                Some(ok_out("active\n"))
            } else {
                None
            }
        });
        let notes = install(&d, &exec, &sample("demo"), "/x/stonewave", None, None).unwrap();
        assert!(unit_path(&d, "demo").exists());
        let calls: Vec<String> = exec
            .calls
            .borrow()
            .iter()
            .map(|(p, a)| format!("{p} {}", a.join(" ")))
            .collect();
        assert_eq!(calls[0], "systemctl --user daemon-reload");
        assert_eq!(
            calls[1],
            "systemctl --user enable --now stonewave-station-demo.service"
        );
        assert!(
            notes.iter().any(|n| n.contains("enable-linger")),
            "{notes:?}"
        );
        assert_eq!(
            state(&d, &exec, "demo"),
            ServiceState::Installed { active: true }
        );
        remove(&d, &exec, "demo").unwrap();
        assert!(!unit_path(&d, "demo").exists());
        assert_eq!(state(&d, &exec, "demo"), ServiceState::None);
    }

    #[test]
    fn a_failed_systemctl_is_reported_and_other_systems_are_refused() {
        let t = TempDir::new("svc2");
        let d = linux(&t);
        let exec = Fake::new(&[]).with(|p, _| (p == "systemctl").then(|| failed("no bus")));
        let e = install(&d, &exec, &sample("demo"), "/x/stonewave", None, None).unwrap_err();
        assert!(e.contains("daemon-reload") && e.contains("no bus"), "{e}");
        let mut w = Dirs::under(t.path());
        w.os = Os::Windows;
        assert!(
            install(&w, &Fake::new(&[]), &sample("demo"), "x", None, None)
                .unwrap_err()
                .contains("only be set up on Linux")
        );
    }
}
