//! A user service that keeps a station running: Linux (systemd `--user`) and macOS (a launchd agent in `~/Library/LaunchAgents`). No root,
//! no sudo.
//!
//! The service file holds nothing but explicit command-line options of `stonewave agent station run`: the same ones the menu shows before it
//! writes the file. The key is never in it: either the station reads the key file `agent connect` made, or the service starts the station
//! through a secret-store command (`launcher`) that puts the key into the environment of that one process.
//!
//! Windows has no service installer yet: the menu says so and starts the station in the terminal instead.

use std::path::PathBuf;

use crate::agent::dirs::{Dirs, Os, text};
use crate::agent::exec::Exec;
use crate::agent::key::{atomic_write, scrub};
use crate::agent::station::{systemd_env, systemd_exec_word, xml_escape};

use super::profile::{Profile, plain_word};

pub fn unit_name(agent: &str) -> String {
    format!("stonewave-station-{agent}.service")
}

/// The launchd label of a station's agent (macOS).
pub fn label(agent: &str) -> String {
    format!("tech.stonewave.station.{agent}")
}

/// Whether this system has a service installer here.
pub fn supported(os: Os) -> bool {
    matches!(os, Os::Linux | Os::Mac)
}

/// The command that manages services on this system, for the texts of the menu.
pub fn manager(os: Os) -> &'static str {
    if os == Os::Mac {
        "launchctl"
    } else {
        "systemctl --user"
    }
}

/// The file that is written, as the menu names it.
pub fn file_hint(os: Os, agent: &str) -> String {
    if os == Os::Mac {
        format!("~/Library/LaunchAgents/{}.plist", label(agent))
    } else {
        format!("~/.config/systemd/user/{}", unit_name(agent))
    }
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

/// The PATH of the shell the menu runs in, without what a service must not inherit: empty entries and relative ones (they would mean
/// "wherever the service happens to be") and anything with control characters.
pub fn clean_path(path_env: Option<&str>, os: Os) -> Option<String> {
    let sep = if os == Os::Windows { ';' } else { ':' };
    let kept: Vec<&str> = path_env?
        .split(sep)
        .filter(|e| {
            !e.is_empty()
                && super::profile::absolute(e)
                && !e.chars().any(crate::agent::key::is_invisible)
        })
        .collect();
    (!kept.is_empty()).then(|| kept.join(&sep.to_string()))
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
    if let Some(path) = clean_path(path_env, Os::Linux) {
        env.push_str(&format!("Environment={}\n", systemd_env("PATH", &path)));
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

/// The launchd agent for macOS: the same command, started at login and again when it ends with an error. No key in it; `log` is where
/// launchd appends what the station says (ids and counts, never message text).
pub fn launchd_plist(
    p: &Profile,
    exe: &str,
    claude: Option<&str>,
    path_env: Option<&str>,
    log: &str,
) -> String {
    let s = |v: &str| format!("<string>{}</string>", xml_escape(v));
    let mut o = String::new();
    o.push_str("<?xml version=\"1.0\" encoding=\"UTF-8\"?>\n");
    o.push_str("<!DOCTYPE plist PUBLIC \"-//Apple//DTD PLIST 1.0//EN\" \"http://www.apple.com/DTDs/PropertyList-1.0.dtd\">\n");
    o.push_str("<plist version=\"1.0\">\n<dict>\n");
    o.push_str(&format!("  <key>Label</key>\n  {}\n", s(&label(&p.agent))));
    o.push_str("  <key>ProgramArguments</key>\n  <array>\n");
    for w in exec_words(p, exe, claude) {
        o.push_str(&format!("    {}\n", s(&w)));
    }
    o.push_str("  </array>\n");
    if let Some(path) = clean_path(path_env, Os::Mac) {
        o.push_str("  <key>EnvironmentVariables</key>\n  <dict>\n");
        o.push_str(&format!("    <key>PATH</key>\n    {}\n", s(&path)));
        o.push_str("  </dict>\n");
    }
    o.push_str("  <key>RunAtLoad</key>\n  <true/>\n");
    // started again after an error, left alone after a clean end (a stop)
    o.push_str("  <key>KeepAlive</key>\n  <dict>\n    <key>SuccessfulExit</key>\n    <false/>\n  </dict>\n");
    o.push_str("  <key>ThrottleInterval</key>\n  <integer>30</integer>\n");
    o.push_str(&format!("  <key>StandardOutPath</key>\n  {}\n", s(log)));
    o.push_str(&format!("  <key>StandardErrorPath</key>\n  {}\n", s(log)));
    o.push_str("</dict>\n</plist>\n");
    o
}

/// The text of the service file of this system, whole, as it will be written.
pub fn file_text(
    os: Os,
    p: &Profile,
    exe: &str,
    claude: Option<&str>,
    path_env: Option<&str>,
    log: &str,
) -> String {
    if os == Os::Mac {
        launchd_plist(p, exe, claude, path_env, log)
    } else {
        systemd_unit(p, exe, claude, path_env)
    }
}

/// Where the station's log goes under launchd (systemd keeps it in the journal).
pub fn log_path(dirs: &Dirs, agent: &str) -> PathBuf {
    dirs.state.join(format!("{agent}.station.log"))
}

/// The launcher's first word must be a program that exists (a full path in the file: a service has a short PATH).
pub fn resolve_launcher(
    p: &Profile,
    which: &dyn Fn(&str) -> Option<PathBuf>,
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

/// Where the service file goes.
pub fn unit_path(dirs: &Dirs, agent: &str) -> PathBuf {
    if dirs.os == Os::Mac {
        dirs.launch_agents.join(format!("{}.plist", label(agent)))
    } else {
        dirs.systemd_user.join(unit_name(agent))
    }
}

fn run_ok(exec: &dyn Exec, program: &str, args: &[String]) -> Result<String, String> {
    match exec.run(program, args) {
        Ok(o) if o.ok => Ok(o.stdout),
        Ok(o) => Err(format!(
            "`{program} {}` failed: {}",
            args.join(" "),
            scrub(&o.problem())
        )),
        Err(e) => Err(format!("cannot run {program}: {}", scrub(&e.to_string()))),
    }
}

/// `systemctl --user ...`, one call.
fn systemctl(exec: &dyn Exec, args: &[&str]) -> Result<String, String> {
    let mut all = vec!["--user".to_owned()];
    all.extend(args.iter().map(|s| (*s).to_owned()));
    run_ok(exec, "systemctl", &all)
}

/// The launchd domain of this user.
pub fn gui_domain() -> String {
    format!("gui/{}", current_uid())
}

#[cfg(unix)]
fn current_uid() -> u32 {
    // SAFETY: getuid has no arguments and cannot fail.
    unsafe { libc::getuid() }
}

#[cfg(not(unix))]
fn current_uid() -> u32 {
    0
}

fn launchctl(exec: &dyn Exec, args: &[&str]) -> Result<String, String> {
    let all: Vec<String> = args.iter().map(|s| (*s).to_owned()).collect();
    run_ok(exec, "launchctl", &all)
}

/// Writes the service file and starts it. Returns what to tell the person.
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
            "a service can only be set up on Linux and macOS so far; start the station in a terminal".into(),
        );
    }
    let p = resolve_launcher(p, &|n| exec.which(n))?;
    let file = unit_path(dirs, &p.agent);
    let existed = file.exists();
    let dir = file.parent().map(PathBuf::from).unwrap_or_default();
    std::fs::create_dir_all(&dir)
        .map_err(|e| format!("cannot make {}: {}", text(&dir), e.kind()))?;
    if dirs.os == Os::Mac {
        install_mac(dirs, exec, &p, exe, claude, path_env, &file, existed)
    } else {
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
}

#[allow(clippy::too_many_arguments)]
fn install_mac(
    dirs: &Dirs,
    exec: &dyn Exec,
    p: &Profile,
    exe: &str,
    claude: Option<&str>,
    path_env: Option<&str>,
    file: &std::path::Path,
    existed: bool,
) -> Result<Vec<String>, String> {
    let log = log_path(dirs, &p.agent);
    std::fs::create_dir_all(&dirs.state)
        .map_err(|e| format!("cannot make {}: {}", text(&dirs.state), e.kind()))?;
    // launchd appends to the log; made private first, as everything of the station
    if !log.exists() {
        atomic_write(&log, b"", 0o600)
            .map_err(|e| format!("cannot make {}: {}", text(&log), e.kind()))?;
    }
    atomic_write(
        file,
        launchd_plist(p, exe, claude, path_env, &text(&log)).as_bytes(),
        0o644,
    )
    .map_err(|e| format!("cannot write {}: {}", text(file), e.kind()))?;
    let domain = gui_domain();
    if existed {
        // a changed file is only read again by a fresh load
        let _ = launchctl(exec, &["bootout", &format!("{domain}/{}", label(&p.agent))]);
    }
    launchctl(exec, &["bootstrap", &domain, &text(file)])?;
    Ok(vec![
        format!(
            "wrote {} and started it; it starts again at every login",
            text(file)
        ),
        format!("The station's log: {}", text(&log)),
    ])
}

fn user_name() -> String {
    std::env::var("USER")
        .or_else(|_| std::env::var("LOGNAME"))
        .unwrap_or_default()
}

/// `start` or `stop`.
pub fn control(dirs: &Dirs, exec: &dyn Exec, agent: &str, what: &str) -> Result<(), String> {
    if dirs.os == Os::Mac {
        let domain = gui_domain();
        let target = format!("{domain}/{}", label(agent));
        return match what {
            // unloads the agent: it stays on disk and loads again at the next login or with "start"
            "stop" => launchctl(exec, &["bootout", &target]).map(|_| ()),
            _ => {
                let file = unit_path(dirs, agent);
                // already loaded is fine: it is then only started
                let _ = launchctl(exec, &["bootstrap", &domain, &text(&file)]);
                launchctl(exec, &["kickstart", &target]).map(|_| ())
            }
        };
    }
    systemctl(exec, &[what, &unit_name(agent)]).map(|_| ())
}

/// Stops and removes the service file.
pub fn remove(dirs: &Dirs, exec: &dyn Exec, agent: &str) -> Result<(), String> {
    let file = unit_path(dirs, agent);
    if !file.exists() {
        return Ok(());
    }
    // not running or not known to the manager any more is fine: the file goes either way
    if dirs.os == Os::Mac {
        let _ = launchctl(
            exec,
            &["bootout", &format!("{}/{}", gui_domain(), label(agent))],
        );
        return std::fs::remove_file(&file)
            .map_err(|e| format!("cannot remove {}: {}", text(&file), e.kind()));
    }
    let _ = systemctl(exec, &["disable", "--now", &unit_name(agent)]);
    std::fs::remove_file(&file)
        .map_err(|e| format!("cannot remove {}: {}", text(&file), e.kind()))?;
    systemctl(exec, &["daemon-reload"]).map(|_| ())
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServiceState {
    /// No service file.
    None,
    Installed {
        active: bool,
    },
}

pub fn state(dirs: &Dirs, exec: &dyn Exec, agent: &str) -> ServiceState {
    if !supported(dirs.os) || !unit_path(dirs, agent).exists() {
        return ServiceState::None;
    }
    let active = if dirs.os == Os::Mac {
        exec.run(
            "launchctl",
            &["print".into(), format!("{}/{}", gui_domain(), label(agent))],
        )
        .is_ok_and(|o| o.ok && o.stdout.contains("state = running"))
    } else {
        exec.run(
            "systemctl",
            &["--user".into(), "is-active".into(), unit_name(agent)],
        )
        .is_ok_and(|o| o.ok && o.stdout.trim() == "active")
    };
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

    #[test]
    fn the_path_of_a_service_has_no_relative_and_no_empty_entries() {
        assert_eq!(
            clean_path(
                Some("/usr/bin::bin:./node_modules/.bin:/home/a/.local/bin:"),
                Os::Linux
            ),
            Some("/usr/bin:/home/a/.local/bin".into())
        );
        assert_eq!(clean_path(Some("bin:.::"), Os::Linux), None);
        assert_eq!(clean_path(None, Os::Linux), None);
        let p = sample("demo");
        let u = systemd_unit(&p, "/x/stonewave", None, Some(".:/usr/bin"));
        assert!(u.contains("Environment=\"PATH=/usr/bin\""), "{u}");
        let u = systemd_unit(&p, "/x/stonewave", None, Some("rel:."));
        assert!(!u.contains("Environment="), "{u}");
    }

    fn mac(t: &TempDir) -> Dirs {
        let mut d = Dirs::under(t.path());
        d.os = Os::Mac;
        d
    }

    #[test]
    fn the_launch_agent_starts_the_station_through_the_launcher_and_holds_no_key() {
        let p = sample("demo");
        let x = launchd_plist(
            &p,
            "/Users/a/bin/stonewave",
            Some("/opt/homebrew/bin/claude"),
            Some("/opt/homebrew/bin:/usr/bin:."),
            "/Users/a/.local/state/wave/demo.station.log",
        );
        for needle in [
            "<key>Label</key>\n  <string>tech.stonewave.station.demo</string>",
            "<array>\n    <string>/usr/local/bin/secrets-run</string>\n    <string>run</string>\n    <string>--env=prod</string>\n    <string>--</string>\n    <string>/Users/a/bin/stonewave</string>\n    <string>agent</string>",
            "<key>PATH</key>\n    <string>/opt/homebrew/bin:/usr/bin</string>",
            "<key>RunAtLoad</key>\n  <true/>",
            "<key>SuccessfulExit</key>\n    <false/>",
            "<key>ThrottleInterval</key>\n  <integer>30</integer>",
            "<key>StandardOutPath</key>\n  <string>/Users/a/.local/state/wave/demo.station.log</string>",
            "<string>--key-env</string>",
        ] {
            assert!(x.contains(needle), "{needle}\n{x}");
        }
        assert!(!x.contains("wak_") && !x.contains("WAVE_AGENT_KEY"));
        // text with XML characters stays text
        let mut q = sample("demo");
        q.workdir = "/Users/a/<b>&c".into();
        let x = launchd_plist(&q, "/x/stonewave", None, None, "/log");
        assert!(
            x.contains("/Users/a/&lt;b&gt;&amp;c") && !x.contains("<b>"),
            "{x}"
        );
        assert!(!x.contains("EnvironmentVariables"));
        assert_eq!(
            file_hint(Os::Mac, "demo"),
            "~/Library/LaunchAgents/tech.stonewave.station.demo.plist"
        );
        assert_eq!(manager(Os::Mac), "launchctl");
        assert!(supported(Os::Mac) && !supported(Os::Windows));
    }

    #[test]
    fn a_launch_agent_is_written_then_loaded_and_a_changed_one_is_loaded_again() {
        let t = TempDir::new("mac1");
        let d = mac(&t);
        let exec = Fake::new(&[]);
        let notes = install(
            &d,
            &exec,
            &sample("demo"),
            "/x/stonewave",
            Some("/x/claude"),
            Some("/usr/bin"),
        )
        .unwrap();
        let file = unit_path(&d, "demo");
        assert!(file.exists() && file.starts_with(&d.launch_agents));
        let dom = gui_domain();
        assert_eq!(
            exec.lines(),
            vec![format!("launchctl bootstrap {dom} {}", text(&file))]
        );
        assert!(
            notes.iter().any(|n| n.contains("every login"))
                && notes.iter().any(|n| n.contains("demo.station.log")),
            "{notes:?}"
        );
        // the log is made before launchd appends to it, private
        let log = log_path(&d, "demo");
        assert!(log.exists());
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            assert_eq!(
                std::fs::metadata(&log).unwrap().permissions().mode() & 0o077,
                0
            );
        }
        // again: the old one is unloaded first
        let exec = Fake::new(&[]);
        install(&d, &exec, &sample("demo"), "/x/stonewave", None, None).unwrap();
        assert_eq!(
            exec.lines(),
            vec![
                format!("launchctl bootout {dom}/tech.stonewave.station.demo"),
                format!("launchctl bootstrap {dom} {}", text(&file)),
            ]
        );
        // a refused load is reported
        let bad = Fake::new(&[]).with(|p, a| {
            (p == "launchctl" && a[0] == "bootstrap").then(|| failed("Bootstrap failed: 5"))
        });
        let e = install(&d, &bad, &sample("demo"), "/x/stonewave", None, None).unwrap_err();
        assert!(
            e.contains("launchctl bootstrap") && e.contains("Bootstrap failed"),
            "{e}"
        );
    }

    #[test]
    fn a_launch_agent_is_stopped_started_and_removed_with_launchctl() {
        let t = TempDir::new("mac2");
        let d = mac(&t);
        let exec = Fake::new(&[]).with(|p, a| {
            (p == "launchctl" && a[0] == "print").then(|| ok_out("state = running\n"))
        });
        install(&d, &exec, &sample("demo"), "/x/stonewave", None, None).unwrap();
        assert_eq!(
            state(&d, &exec, "demo"),
            ServiceState::Installed { active: true }
        );
        let dom = gui_domain();
        let file = text(&unit_path(&d, "demo"));
        let exec = Fake::new(&[]);
        control(&d, &exec, "demo", "stop").unwrap();
        control(&d, &exec, "demo", "start").unwrap();
        assert_eq!(
            exec.lines(),
            vec![
                format!("launchctl bootout {dom}/tech.stonewave.station.demo"),
                format!("launchctl bootstrap {dom} {file}"),
                format!("launchctl kickstart {dom}/tech.stonewave.station.demo"),
            ]
        );
        // unloaded: still installed, not active
        let off = Fake::new(&[]).with(|p, a| {
            (p == "launchctl" && a[0] == "print").then(|| failed("Could not find service"))
        });
        assert_eq!(
            state(&d, &off, "demo"),
            ServiceState::Installed { active: false }
        );
        remove(&d, &exec, "demo").unwrap();
        assert!(!unit_path(&d, "demo").exists());
        assert_eq!(state(&d, &exec, "demo"), ServiceState::None);
    }
}
