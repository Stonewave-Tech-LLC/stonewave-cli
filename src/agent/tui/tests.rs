//! The menu, driven by keys: a scripted machine behind it, a scripted terminal in front.

use std::collections::VecDeque;

use super::model::*;
use super::profile::{self, Profile};
use super::service::ServiceState;
use super::{Console, World, run_effects, view};
use crate::agent::dirs::Os;
use crate::agent::engine::Engine;
use crate::style::ColorMode;

// ------------------------------------------------------------------ the scripted machine

fn snap() -> Snapshot {
    Snapshot {
        engines: vec![
            EngineRow {
                engine: Engine::ClaudeCode,
                found: true,
                version: Some("2.1.321 (Claude Code)".into()),
            },
            EngineRow {
                engine: Engine::Codex,
                found: false,
                version: None,
            },
            EngineRow {
                engine: Engine::Grok,
                found: false,
                version: None,
            },
        ],
        agents: vec![],
        root: false,
        os: Os::Linux,
        exe: "/opt/sw/stonewave".into(),
        home: "/home/neo".into(),
        config_dir: "/home/neo/.config/wave".into(),
        state_dir: "/home/neo/.local/state/wave".into(),
        wave_hint: Some("https://wave.test".into()),
        claude: Some("/usr/bin/claude".into()),
        path_env: Some("/usr/bin".into()),
        host: "box".into(),
    }
}

fn row(name: &str) -> AgentRow {
    AgentRow {
        name: name.into(),
        engine: "claude-code".into(),
        key: KeyState::File,
        connected: true,
        running: false,
        paused: false,
        service: ServiceState::None,
        profile: None,
        wave_url: Some("https://wave.test".into()),
    }
}

#[derive(Default)]
struct Fake {
    snap: Option<Snapshot>,
    log: Vec<String>,
    /// What the connect command leaves behind: an agent with this name.
    sign_in_makes: Option<String>,
    interactive_code: i32,
    capture_code: i32,
    capture_lines: Vec<String>,
    rooms: Option<Result<Vec<Room>, String>>,
    install: Option<Result<Vec<String>, String>>,
    profiles: Vec<Profile>,
}

impl World for Fake {
    fn snapshot(&mut self) -> Snapshot {
        self.log.push("snapshot".into());
        self.snap.clone().unwrap_or_else(snap)
    }

    fn capture(&mut self, args: &[String]) -> (i32, Vec<String>) {
        self.log.push(format!("capture {}", args.join(" ")));
        (self.capture_code, self.capture_lines.clone())
    }

    fn interactive(&mut self, args: &[String]) -> i32 {
        self.log.push(format!("interactive {}", args.join(" ")));
        if args.get(1).map(String::as_str) == Some("connect") && self.interactive_code == 0 {
            if let Some(n) = &self.sign_in_makes {
                let s = self.snap.get_or_insert_with(snap);
                s.agents.push(row(n));
            }
        }
        self.interactive_code
    }

    fn rooms(
        &mut self,
        agent: &str,
        key_env: Option<&str>,
        wave_url: &str,
    ) -> Result<Vec<Room>, String> {
        self.log.push(format!(
            "rooms {agent} {} {wave_url}",
            key_env.unwrap_or("-")
        ));
        self.rooms.clone().unwrap_or_else(|| {
            Ok(vec![
                Room {
                    id: "room-1".into(),
                    name: "Team room".into(),
                },
                Room {
                    id: "room-2".into(),
                    name: "Company room".into(),
                },
                Room {
                    id: "room-3".into(),
                    name: "Random".into(),
                },
            ])
        })
    }

    fn save_profile(&mut self, p: &Profile) -> Result<(), String> {
        self.log.push(format!("save {}", p.agent));
        self.profiles.push(p.clone());
        let s = self.snap.get_or_insert_with(snap);
        match s.agents.iter_mut().find(|a| a.name == p.agent) {
            Some(a) => a.profile = Some(p.clone()),
            None => {
                let mut a = row(&p.agent);
                a.connected = false;
                a.key = KeyState::Env {
                    name: p.key_env.clone().unwrap_or_default(),
                    set_here: true,
                };
                a.profile = Some(p.clone());
                s.agents.push(a);
            }
        }
        Ok(())
    }

    fn install_service(&mut self, p: &Profile) -> Result<Vec<String>, String> {
        self.log.push(format!("install {}", p.agent));
        self.install
            .clone()
            .unwrap_or_else(|| Ok(vec!["wrote the file".into()]))
    }

    fn service_control(&mut self, agent: &str, what: &str) -> Result<(), String> {
        self.log.push(format!("service {what} {agent}"));
        Ok(())
    }

    fn remove_service(&mut self, agent: &str) -> Result<(), String> {
        self.log.push(format!("remove-service {agent}"));
        Ok(())
    }

    fn forget(&mut self, agent: &str) -> Result<(), String> {
        self.log.push(format!("forget {agent}"));
        if let Some(s) = &mut self.snap {
            s.agents.retain(|a| a.name != agent);
        }
        Ok(())
    }
}

#[derive(Default)]
struct Term {
    frames: Vec<Vec<String>>,
    keys: VecDeque<Key>,
    suspended: u32,
    resumed: u32,
    size: (usize, usize),
}

impl Console for Term {
    fn size(&self) -> (usize, usize) {
        if self.size == (0, 0) {
            (80, 30)
        } else {
            self.size
        }
    }

    fn draw(&mut self, lines: &[String]) {
        self.frames.push(lines.to_vec());
    }

    fn next(&mut self, _: std::time::Duration) -> Option<Option<Key>> {
        self.keys.pop_front().map(Some)
    }

    fn suspend(&mut self) {
        self.suspended += 1;
    }

    fn resume(&mut self) {
        self.resumed += 1;
    }
}

struct Rig {
    app: App,
    world: Fake,
    term: Term,
}

fn env_probe(n: &str) -> bool {
    n == "DEMO_KEY"
}

impl Rig {
    fn new() -> Rig {
        Rig::with(snap())
    }

    fn with(s: Snapshot) -> Rig {
        let mut app = App::new(s.clone());
        app.env_probe = env_probe;
        Rig {
            app,
            world: Fake {
                snap: Some(s),
                ..Fake::default()
            },
            term: Term::default(),
        }
    }

    fn press(&mut self, k: Key) -> &mut Rig {
        let fx = self.app.on_key(k);
        run_effects(
            &mut self.app,
            &mut self.world,
            &mut self.term,
            ColorMode::None,
            std::time::Instant::now(),
            fx,
        );
        self
    }

    fn keys(&mut self, ks: &[Key]) -> &mut Rig {
        for k in ks {
            self.press(*k);
        }
        self
    }

    fn text(&mut self, t: &str) -> &mut Rig {
        for c in t.chars() {
            self.press(Key::Char(c));
        }
        self
    }

    fn screen(&self) -> String {
        view::render(&self.app, 100, 40, ColorMode::None, 0.0).join("\n")
    }

    fn calls(&self) -> Vec<String> {
        self.world
            .log
            .iter()
            .filter(|l| *l != "snapshot")
            .cloned()
            .collect()
    }

    fn wizard(&self) -> &Wizard {
        match self.app.top() {
            Screen::Wizard(w) => w,
            other => panic!("not the wizard: {other:?}"),
        }
    }
}

// ------------------------------------------------------------------ text input

#[test]
fn a_text_field_edits_where_the_cursor_is() {
    let mut i = Input::with("abc");
    assert_eq!(i.cursor, 3);
    i.edit(Key::Left);
    i.edit(Key::Char('X'));
    assert_eq!(i.value(), "abXc");
    i.edit(Key::Backspace);
    i.edit(Key::Delete);
    assert_eq!(i.value(), "ab");
    i.edit(Key::Home);
    i.edit(Key::Char('-'));
    assert_eq!(i.value(), "-ab");
    i.edit(Key::CtrlU);
    assert!(i.is_empty() && i.cursor == 0);
    // control characters never get in (a paste with an escape sequence, a newline)
    assert!(!i.edit(Key::Char('\u{1b}')));
    assert!(Input::with("a\nb\u{1b}c").value() == "abc");
    // and the field has a limit
    let long = "x".repeat(1000);
    assert_eq!(Input::with(&long).value().len(), 300);
}

// ------------------------------------------------------------------ main screen

#[test]
fn the_main_screen_lists_agents_and_the_three_fixed_entries() {
    let mut s = snap();
    s.agents = vec![row("claude"), row("demo")];
    let mut r = Rig::with(s);
    let screen = r.screen();
    assert!(
        screen.contains("claude") && screen.contains("demo"),
        "{screen}"
    );
    assert!(
        screen.contains("Agent verbinden")
            && screen.contains("Status")
            && screen.contains("Beenden")
    );
    // down twice: on "Agent verbinden", Enter opens the wizard
    r.keys(&[Key::Down, Key::Down, Key::Enter]);
    assert!(matches!(r.app.top(), Screen::Wizard(_)));
    // Esc on its first step closes it
    r.press(Key::Esc);
    assert!(matches!(r.app.top(), Screen::Main { .. }));
}

#[test]
fn q_and_ctrl_c_quit_and_nothing_is_started() {
    let mut r = Rig::new();
    r.press(Key::Char('q'));
    assert!(r.app.quit);
    let mut r = Rig::new();
    r.keys(&[Key::Enter]); // "Agent verbinden"
    r.press(Key::CtrlC);
    assert!(r.app.quit);
    assert!(r.calls().is_empty());
}

#[test]
fn the_status_screen_needs_no_network_and_lists_what_is_here() {
    let mut s = snap();
    s.agents = vec![row("claude")];
    let mut r = Rig::with(s);
    r.keys(&[Key::Down, Key::Down, Key::Enter]); // Status
    let screen = r.screen();
    assert!(
        screen.contains("Coding-Agenten auf diesem Computer") && screen.contains("2.1.321"),
        "{screen}"
    );
    assert!(screen.contains("Codex") && screen.contains("nicht gefunden"));
    assert!(r.calls().is_empty(), "{:?}", r.calls());
    r.press(Key::Esc);
    assert!(matches!(r.app.top(), Screen::Main { .. }));
}

#[test]
fn as_root_the_menu_connects_nothing() {
    let mut s = snap();
    s.root = true;
    let mut r = Rig::with(s);
    let screen = r.screen();
    assert!(
        screen.contains("root") && screen.contains("sudo stonewave connect"),
        "{screen}"
    );
    r.keys(&[Key::Down, Key::Enter, Key::Enter]);
    assert!(matches!(r.app.top(), Screen::Main { .. }));
    assert!(r.calls().is_empty());
}

// ------------------------------------------------------------------ agent actions

fn with_profile(mut a: AgentRow) -> AgentRow {
    let mut p = profile::sample(&a.name);
    p.key_env = None;
    p.launcher.clear();
    a.profile = Some(p);
    a
}

#[test]
fn the_actions_follow_what_is_set_up() {
    let plain = row("claude");
    let acts = agent_actions(&plain, Os::Linux);
    assert_eq!(
        acts,
        vec![
            AgentAction::Status,
            AgentAction::CheckWave,
            AgentAction::SetUpStation,
            AgentAction::Disconnect
        ]
    );
    let set = with_profile(row("claude"));
    let acts = agent_actions(&set, Os::Linux);
    assert!(
        acts.contains(&AgentAction::RunHere)
            && acts.contains(&AgentAction::Pause)
            && acts.contains(&AgentAction::InstallService)
    );
    assert!(!acts.contains(&AgentAction::Resume) && !acts.contains(&AgentAction::StopService));
    // elsewhere than Linux there is no service to offer
    assert!(!agent_actions(&set, Os::Windows).contains(&AgentAction::InstallService));
    let mut paused = set.clone();
    paused.paused = true;
    assert!(agent_actions(&paused, Os::Linux).contains(&AgentAction::Resume));
    let mut svc = set.clone();
    svc.service = ServiceState::Installed { active: true };
    let acts = agent_actions(&svc, Os::Linux);
    assert!(
        acts.contains(&AgentAction::StopService)
            && acts.contains(&AgentAction::RemoveService)
            && !acts.contains(&AgentAction::InstallService)
    );
    svc.service = ServiceState::Installed { active: false };
    assert!(agent_actions(&svc, Os::Linux).contains(&AgentAction::StartService));
    // an agent that is not Claude Code has no station to set up
    let mut codex = row("codex");
    codex.engine = "codex".into();
    assert!(!agent_actions(&codex, Os::Linux).contains(&AgentAction::SetUpStation));
}

#[test]
fn status_shows_what_the_command_says_on_a_screen_of_its_own() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    r.world.capture_lines = vec!["agent claude".into(), "paused: no".into()];
    r.keys(&[Key::Enter, Key::Enter]); // the agent, "Status der Station"
    assert_eq!(
        r.calls(),
        vec!["capture agent station status --agent claude"]
    );
    let screen = r.screen();
    assert!(
        screen.contains("Status von claude") && screen.contains("paused: no"),
        "{screen}"
    );
    r.press(Key::Esc);
    assert!(matches!(r.app.top(), Screen::Agent { .. }));
}

#[test]
fn pausing_asks_first_and_enter_on_the_default_is_a_no() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    // Pausieren is the 4th action: Status, CheckWave, RunHere, Pause
    r.keys(&[Key::Enter, Key::Down, Key::Down, Key::Down, Key::Enter]);
    assert!(
        matches!(r.app.top(), Screen::Confirm(_)),
        "{:?}",
        r.app.top()
    );
    assert!(r.screen().contains("pausieren"));
    r.press(Key::Enter); // the cursor is on "Nein"
    assert!(matches!(r.app.top(), Screen::Agent { .. }));
    assert!(r.calls().is_empty(), "{:?}", r.calls());
    // again, and say yes
    r.press(Key::Enter);
    assert!(matches!(r.app.top(), Screen::Confirm(_)));
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(
        r.calls(),
        vec!["capture agent station pause --agent claude"]
    );
    assert!(r.screen().contains("Pausiert"), "{}", r.screen());
}

#[test]
fn running_the_station_here_hands_the_terminal_over_and_comes_back() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    r.keys(&[Key::Enter, Key::Down, Key::Down, Key::Enter]); // RunHere
    assert!(r.screen().contains("Strg-C beendet sie"));
    assert_eq!(r.term.suspended, 0);
    r.press(Key::Char('y'));
    assert_eq!((r.term.suspended, r.term.resumed), (1, 1));
    let calls = r.calls();
    assert_eq!(calls.len(), 1);
    assert!(
        calls[0].starts_with(
            "interactive agent station run --agent claude --wave https://wave.test --workdir "
        ) && calls[0].contains("--rooms room-1,room-2")
            && calls[0].contains("--claude /usr/bin/claude"),
        "{calls:?}"
    );
    assert!(!calls[0].contains("--once") && !calls[0].contains("wak_"));
}

#[test]
fn a_service_is_shown_whole_before_it_is_written() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    // Status, CheckWave, RunHere, Pause, InstallService
    r.keys(&[
        Key::Enter,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
    ]);
    let screen = r.screen();
    assert!(
        screen.contains("ExecStart=")
            && screen.contains("room-1,room-2")
            && screen.contains("systemctl --user"),
        "{screen}"
    );
    assert!(!screen.contains("wak_"));
    assert!(r.calls().is_empty(), "nothing is written before the yes");
    r.press(Key::Char('y'));
    assert_eq!(r.calls(), vec!["install claude"]);
    assert!(r.screen().contains("Der Dienst läuft."), "{}", r.screen());
}

#[test]
fn an_env_key_agent_without_a_secret_store_command_gets_no_service() {
    let mut a = row("demo");
    a.connected = false;
    a.key = KeyState::Env {
        name: "DEMO_KEY".into(),
        set_here: true,
    };
    let mut p = profile::sample("demo");
    p.launcher.clear();
    a.profile = Some(p);
    let mut s = snap();
    s.agents = vec![a];
    let mut r = Rig::with(s);
    r.keys(&[Key::Enter, Key::Down, Key::Down, Key::Down, Key::Enter]); // InstallService
    assert!(
        matches!(r.app.top(), Screen::Agent { .. }),
        "no question was asked"
    );
    assert!(r.screen().contains("Secret-Stores"), "{}", r.screen());
    assert!(r.calls().is_empty());
}

#[test]
fn disconnecting_signs_off_first_and_only_then_forgets_the_rest() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    let n = agent_actions(&with_profile(row("claude")), Os::Linux).len();
    let mut ks = vec![Key::Enter];
    ks.extend(std::iter::repeat_n(Key::Down, n - 1));
    ks.push(Key::Enter);
    r.keys(&ks);
    assert!(r.screen().contains("trennen?"), "{}", r.screen());
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(
        r.calls(),
        vec![
            "interactive agent disconnect --agent claude --yes",
            "forget claude"
        ]
    );
    assert!(
        matches!(r.app.top(), Screen::Main { .. }),
        "the page of an agent that is gone closes"
    );
    assert!(r.screen().contains("getrennt"));

    // when the sign-off fails nothing else is deleted
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    r.world.interactive_code = 1;
    r.keys(&ks);
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(
        r.calls(),
        vec!["interactive agent disconnect --agent claude --yes"]
    );
    assert!(r.screen().contains("nicht gelungen"));
}

// ------------------------------------------------------------------ the wizard

#[test]
fn a_new_agent_goes_from_sign_in_to_a_running_service() {
    let mut r = Rig::new();
    r.world.sign_in_makes = Some("claude".into());
    r.keys(&[Key::Enter]); // "Agent verbinden"
    assert_eq!(r.wizard().step, Step::Mode);
    r.press(Key::Enter); // sign in with the browser
    assert_eq!(r.wizard().step, Step::Engine);
    assert!(r.screen().contains("Claude Code") && r.screen().contains("gefunden (2.1.321"));
    // Codex is not installed: refused with the hint, nothing moves
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(r.wizard().step, Step::Engine);
    assert!(
        r.screen().contains("npm install -g @openai/codex"),
        "{}",
        r.screen()
    );
    r.keys(&[Key::Up, Key::Enter]);
    assert_eq!(r.wizard().step, Step::Name);
    assert_eq!(r.wizard().name.value(), "claude", "a free name is proposed");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::SignIn);
    assert!(
        r.screen()
            .contains("stonewave agent connect --engine claude-code --name claude --no-station")
    );
    assert_eq!(
        r.world
            .log
            .iter()
            .filter(|l| l.starts_with("interactive"))
            .count(),
        0,
        "nothing starts before Enter"
    );
    r.press(Key::Enter);
    assert_eq!(
        r.calls(),
        vec!["interactive agent connect --engine claude-code --name claude --no-station"]
    );
    assert_eq!((r.term.suspended, r.term.resumed), (1, 1));
    assert_eq!(r.wizard().step, Step::StationAsk);
    r.press(Key::Enter); // yes
    // the address of Wave: the one the sign-in made, to be confirmed before any room is asked
    assert_eq!(r.wizard().step, Step::WaveUrl);
    assert_eq!(r.wizard().wave_url.value(), "https://wave.test");
    assert!(
        r.calls().iter().all(|c| !c.starts_with("rooms")),
        "{:?}",
        r.calls()
    );
    r.press(Key::Enter);
    // the rooms are fetched with the key file (no variable)
    assert_eq!(r.wizard().step, Step::Rooms);
    assert!(
        r.calls()
            .contains(&"rooms claude - https://wave.test".to_owned()),
        "{:?}",
        r.calls()
    );
    assert!(r.screen().contains("Team room") && r.screen().contains("Random"));
    // nothing chosen: refused
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Rooms);
    assert!(r.screen().contains("mindestens einen Raum"));
    r.keys(&[Key::Char(' '), Key::Down, Key::Char(' '), Key::Enter]);
    assert_eq!(r.wizard().step, Step::Workdir);
    assert_eq!(r.wizard().workdir.value(), "/home/neo/wave-agents/claude");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Limits);
    assert!(r.screen().contains("20 Minuten") && r.screen().contains("keine Befehls-Werkzeuge"));
    // the preview: the dry run first, the profile is saved only after it was clean
    r.world.capture_lines = vec!["station: dry run".into(), "command: claude -p ...".into()];
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Preview);
    let calls = r.calls();
    assert_eq!(calls[3], "save claude");
    assert!(
        calls[2].starts_with("capture agent station run --agent claude --wave https://wave.test --workdir /home/neo/wave-agents/claude --rooms room-1,room-2 --claude /usr/bin/claude --dry-run"),
        "{calls:?}"
    );
    assert!(r.screen().contains("command: claude -p"));
    // the test run hands the terminal over once more
    r.keys(&[Key::Enter]);
    assert_eq!(r.wizard().step, Step::Test);
    r.press(Key::Enter);
    let calls = r.calls();
    assert!(
        calls[4].starts_with("interactive agent station run --agent claude")
            && calls[4].ends_with("--once"),
        "{calls:?}"
    );
    assert_eq!(r.wizard().step, Step::Service);
    let screen = r.screen();
    assert!(
        screen.contains("stonewave-station-claude.service")
            && screen.contains("Environment=\"PATH=")
            && screen.contains("ExecStart=")
            && screen.contains("/opt/sw/stonewave"),
        "the whole file is shown: {screen}"
    );
    // Enter on the default, "Nein": nothing is installed
    r.press(Key::Enter);
    assert!(!r.world.log.iter().any(|l| l.starts_with("install")));
    assert_eq!(r.wizard().step, Step::Done);
    assert!(r.screen().contains("claude ist eingerichtet") && r.screen().contains("Kein Dienst"));
    r.press(Key::Enter);
    assert!(matches!(r.app.top(), Screen::Main { .. }));
}

/// The wizard for a new agent up to the question about the service.
fn new_agent_to_service(r: &mut Rig) {
    r.world.sign_in_makes = Some("claude".into());
    r.keys(&[
        Key::Enter, // Agent verbinden
        Key::Enter, // sign in with the browser
        Key::Enter, // Claude Code
        Key::Enter, // name
        Key::Enter, // sign in
        Key::Enter, // station: yes
        Key::Enter, // the address of Wave
        Key::Char(' '),
        Key::Enter, // rooms
        Key::Enter, // folder
        Key::Enter, // limits
        Key::Enter, // preview
        Key::Enter, // test
    ]);
    assert_eq!(r.wizard().step, Step::Service);
}

#[test]
fn the_service_is_installed_only_after_an_explicit_yes() {
    let mut r = Rig::new();
    new_agent_to_service(&mut r);
    // the cursor starts on "Nein"
    assert_eq!(r.wizard().cursor, 0);
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(
        r.world
            .log
            .iter()
            .filter(|l| *l == "install claude")
            .count(),
        1
    );
    assert_eq!(r.wizard().step, Step::Done);
    assert!(r.screen().contains("wrote the file"), "{}", r.screen());
}

#[test]
fn a_failed_install_stays_on_the_question_with_the_reason() {
    let mut r = Rig::new();
    new_agent_to_service(&mut r);
    r.world.install = Some(Err("`systemctl --user daemon-reload` failed: no bus".into()));
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(r.wizard().step, Step::Service);
    assert!(r.screen().contains("no bus"), "{}", r.screen());
}

#[test]
fn nothing_is_remembered_before_the_dry_run_is_clean() {
    let mut r = Rig::new();
    r.world.capture_code = 1;
    r.world.capture_lines =
        vec!["refused: the working folder cannot be your whole home folder".into()];
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter, // station: yes
        Key::Enter, // the address of Wave
        Key::Char(' '),
        Key::Enter, // rooms
        Key::Enter, // folder
        Key::Enter, // limits: the dry run runs and says no
    ]);
    assert_eq!(r.wizard().step, Step::Preview);
    assert!(
        r.screen().contains("Nichts wurde gemerkt"),
        "{}",
        r.screen()
    );
    assert!(r.world.profiles.is_empty(), "no profile was saved");
    // Enter does not go on, and a profile is not there for a service to be built on
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Preview);
    // a second try that works saves it
    r.world.capture_code = 0;
    r.world.capture_lines = vec!["fine".into()];
    r.press(Key::Char('r'));
    assert_eq!(r.world.profiles.len(), 1);
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Test);
}

#[test]
fn the_wave_address_has_a_default_when_nothing_is_known() {
    let mut s = snap();
    s.wave_hint = None;
    // an existing agent: Name -> address
    let mut r = Rig::with(s.clone());
    r.keys(&[Key::Enter, Key::Down, Key::Enter, Key::Enter, Key::Enter]);
    assert_eq!(r.wizard().step, Step::WaveUrl);
    assert_eq!(r.wizard().wave_url.value(), DEFAULT_WAVE);
    r.press(Key::Enter);
    assert_eq!(
        r.wizard().step,
        Step::KeySource,
        "the default is accepted as it stands"
    );
    // a typed address is not replaced by a proposal when the person comes back
    r.press(Key::Esc);
    r.press(Key::CtrlU);
    r.text("https://wave.mine.test");
    r.keys(&[Key::Enter, Key::Esc]);
    assert_eq!(r.wizard().wave_url.value(), "https://wave.mine.test");
    // a new agent without a hint: the sign-in makes the address, it is shown for confirmation
    let mut r = Rig::with(s);
    r.world.sign_in_makes = Some("claude".into());
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
    ]);
    assert_eq!(r.wizard().step, Step::WaveUrl);
    assert_eq!(
        r.wizard().wave_url.value(),
        "https://wave.test",
        "taken from what the sign-in made"
    );
    // and an agent whose state has no address and nothing else is known falls back to the default
    let mut s = snap();
    s.wave_hint = None;
    s.agents = vec![{
        let mut a = row("claude");
        a.wave_url = None;
        a
    }];
    let mut r = Rig::with(s);
    r.keys(&[Key::Enter, Key::Down, Key::Down, Key::Enter]); // the agent, "Station einrichten"
    assert_eq!(r.wizard().wave_url.value(), DEFAULT_WAVE);
    assert!(
        r.calls().iter().any(|c| c.ends_with(DEFAULT_WAVE)),
        "{:?}",
        r.calls()
    );
}

#[test]
fn a_mac_gets_a_launch_agent_and_the_question_says_so() {
    let mut s = snap();
    s.os = Os::Mac;
    let mut a = with_profile(row("claude"));
    a.profile.as_mut().unwrap().workdir = "/Users/neo/wave-agents/claude".into();
    s.agents = vec![a];
    let mut r = Rig::with(s);
    r.keys(&[
        Key::Enter,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
    ]);
    let screen = r.screen();
    assert!(
        screen.contains("LaunchAgents/tech.stonewave.station.claude.plist")
            && screen.contains("launchctl")
            && screen.contains("<key>Label</key>")
            && !screen.contains("systemctl"),
        "{screen}"
    );
    // the file is longer than the screen: it scrolls, the answers stay
    for _ in 0..8 {
        r.press(Key::PageDown);
    }
    let end = r.screen();
    assert!(
        end.contains("RunAtLoad") && end.contains("Nein") && end.contains("Ja"),
        "{end}"
    );
    r.press(Key::Char('y'));
    assert_eq!(r.calls(), vec!["install claude"]);
}

#[test]
fn every_command_the_wizard_starts_is_one_that_exists_and_carries_no_key() {
    // the whole wizard again, collecting every command it ran
    let mut r = Rig::new();
    r.world.sign_in_makes = Some("claude".into());
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Char(' '),
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
    ]);
    let calls = r.calls();
    assert!(calls.len() >= 4, "{calls:?}");
    for c in &calls {
        let words: Vec<&str> = c.split(' ').collect();
        if matches!(words[0], "capture" | "interactive") {
            assert_eq!(words[1], "agent", "{c}");
            assert!(
                ["connect", "station", "status", "disconnect"].contains(&words[2]),
                "{c}"
            );
        }
        assert!(
            !c.contains("wak_") && !c.contains(';') && !c.contains('|') && !c.contains('`'),
            "{c}"
        );
    }
}

#[test]
fn a_failed_sign_in_stays_on_the_step_and_a_second_try_works() {
    let mut r = Rig::new();
    r.world.interactive_code = 1;
    r.keys(&[Key::Enter, Key::Enter, Key::Enter, Key::Enter, Key::Enter]);
    assert_eq!(r.wizard().step, Step::SignIn);
    assert!(r.screen().contains("nicht abgeschlossen"), "{}", r.screen());
    r.world.interactive_code = 0;
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::StationAsk);
}

#[test]
fn later_leaves_the_station_for_the_agents_own_page() {
    let mut r = Rig::new();
    r.keys(&[Key::Enter, Key::Enter, Key::Enter, Key::Enter, Key::Enter]);
    assert_eq!(r.wizard().step, Step::StationAsk);
    r.keys(&[Key::Down, Key::Enter]);
    assert_eq!(r.wizard().step, Step::Done);
    assert!(r.screen().contains("später auf der Seite des Agenten"));
    assert!(r.calls().iter().all(|c| !c.starts_with("rooms")));
}

#[test]
fn names_are_checked_before_anything_happens() {
    let mut s = snap();
    s.agents = vec![row("claude")];
    let mut r = Rig::with(s);
    r.keys(&[Key::Down, Key::Enter, Key::Enter, Key::Enter]); // wizard, Mode, Engine -> Name
    assert_eq!(r.wizard().step, Step::Name);
    assert_eq!(
        r.wizard().name.value(),
        "claude-2",
        "the proposal is a free name"
    );
    for _ in 0..12 {
        r.press(Key::Backspace);
    }
    r.text("Bad Name!");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Name);
    assert!(r.screen().contains("Kleinbuchstaben"), "{}", r.screen());
    r.press(Key::CtrlU);
    r.text("claude");
    r.press(Key::Enter);
    assert!(r.screen().contains("gibt es hier schon"));
    r.press(Key::CtrlU);
    r.text("other");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::SignIn);
}

#[test]
fn an_existing_agent_with_a_key_in_a_secret_store_never_types_the_key() {
    let mut r = Rig::new();
    // Mode: the second entry; engine Claude Code; name
    r.keys(&[Key::Enter, Key::Down, Key::Enter, Key::Enter]);
    assert_eq!(r.wizard().step, Step::Name);
    assert_eq!(r.wizard().name.value(), "claude");
    r.press(Key::CtrlU);
    r.text("demo");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::WaveUrl);
    assert_eq!(
        r.wizard().wave_url.value(),
        "https://wave.test",
        "from STONEWAVE_WAVE_URL or another agent"
    );
    // a plain-http address is refused
    r.press(Key::CtrlU);
    r.text("http://example.com");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::WaveUrl);
    assert!(
        r.screen().contains("Diese Adresse geht nicht"),
        "{}",
        r.screen()
    );
    r.press(Key::CtrlU);
    r.text("https://wave.stonewavetech.test");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::KeySource);
    // the key itself is refused as a variable name
    r.press(Key::CtrlU);
    r.text("wak_0a1b2c3d-0000-4000-8000-000000000001_secret");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::KeySource);
    assert!(r.screen().contains("NAME der Umgebungsvariable"));
    assert!(!r.screen().contains("wak_0a1b") || r.screen().matches("wak_0a1b").count() <= 1);
    r.press(Key::CtrlU);
    r.text("DEMO_KEY");
    assert!(r.screen().contains("ist in dieser Shell gesetzt"));
    // a launcher with shell characters is refused
    r.press(Key::Tab);
    r.text("secrets-run run; rm -rf /");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::KeySource);
    assert!(r.screen().contains("nur Buchstaben"));
    r.press(Key::CtrlU);
    r.text("/usr/local/bin/secrets-run run --env=prod --");
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Rooms);
    assert!(
        r.calls()
            .contains(&"rooms demo DEMO_KEY https://wave.stonewavetech.test".to_owned()),
        "{:?}",
        r.calls()
    );
    // Room 1 and 2, then on to the end
    r.keys(&[
        Key::Char(' '),
        Key::Down,
        Key::Char(' '),
        Key::Enter,
        Key::Enter,
        Key::Enter,
    ]);
    assert_eq!(r.wizard().step, Step::Preview);
    let saved = r.world.profiles.last().unwrap().clone();
    assert_eq!(saved.key_env.as_deref(), Some("DEMO_KEY"));
    assert_eq!(
        saved.launcher,
        vec!["/usr/local/bin/secrets-run", "run", "--env=prod", "--"]
    );
    let capture = r
        .calls()
        .into_iter()
        .find(|c| c.starts_with("capture"))
        .unwrap();
    assert!(
        capture.contains("--key-env DEMO_KEY") && capture.ends_with("--dry-run"),
        "{capture}"
    );
    assert!(!capture.contains("wak_"));
    // the service of this agent goes through the secret store
    r.keys(&[Key::Enter]);
    r.press(Key::Char('s')); // skip the test
    assert_eq!(r.wizard().step, Step::Service);
    assert!(
        r.screen().contains(
            "ExecStart=\"/usr/local/bin/secrets-run\" \"run\" \"--env=prod\" \"--\" \"/opt/sw/stonewave\""
        ),
        "{}",
        r.screen()
    );
}

#[test]
fn a_failed_room_list_can_be_tried_again_and_an_empty_one_says_why() {
    let mut r = Rig::new();
    r.world.rooms = Some(Err("Wave nimmt den Schlüssel nicht an.".into()));
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
    ]); // ... Rooms
    assert_eq!(r.wizard().step, Step::Rooms);
    assert!(
        r.screen()
            .contains("Wave nimmt den Schlüssel nicht an. Enter versucht es noch einmal."),
        "{}",
        r.screen()
    );
    r.world.rooms = Some(Ok(vec![]));
    r.press(Key::Enter);
    assert!(r.screen().contains("noch keinen Raum lesen"));
    r.world.rooms = None;
    r.press(Key::Enter);
    assert!(r.screen().contains("Company room"));
}

#[test]
fn esc_goes_back_one_step_and_keeps_what_was_chosen() {
    let mut r = Rig::new();
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Char(' '),
        Key::Enter,
    ]);
    assert_eq!(r.wizard().step, Step::Workdir);
    r.press(Key::Esc);
    assert_eq!(r.wizard().step, Step::Rooms);
    assert_eq!(r.wizard().chosen_rooms().len(), 1, "the room stays chosen");
    // no way back into the sign-in once it is done
    r.keys(&[Key::Esc]);
    assert_eq!(r.wizard().step, Step::WaveUrl);
    r.keys(&[Key::Esc]);
    assert_eq!(
        r.wizard().step,
        Step::StationAsk,
        "back from the address is the question about the station, not the sign-in"
    );
    r.keys(&[Key::Esc]);
    assert!(
        matches!(r.app.top(), Screen::Main { .. }),
        "and from there the wizard closes: the sign-in is not repeated"
    );
}

#[test]
fn a_service_cannot_be_set_up_elsewhere_than_on_linux_and_the_wizard_says_how_to_go_on() {
    let mut s = snap();
    s.os = Os::Windows;
    s.home = if cfg!(windows) {
        "C:\\Users\\neo"
    } else {
        "/home/neo"
    }
    .into();
    let mut r = Rig::with(s);
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
    ]);
    assert_eq!(r.wizard().step, Step::Rooms);
    r.keys(&[Key::Char(' '), Key::Enter]);
    assert_eq!(
        r.wizard().workdir.value(),
        format!("{}\\wave-agents\\claude", r.app.snap.home)
    );
    r.keys(&[Key::Enter, Key::Enter, Key::Enter, Key::Char('s')]);
    assert_eq!(r.wizard().step, Step::Service);
    assert!(
        r.screen().contains("noch nicht einrichten"),
        "{}",
        r.screen()
    );
    assert!(!r.calls().iter().any(|c| c.starts_with("install")));
    r.press(Key::Enter);
    assert_eq!(r.wizard().step, Step::Done);
}

#[test]
fn the_station_of_an_agent_already_connected_starts_at_the_rooms() {
    let mut s = snap();
    s.agents = vec![row("claude")];
    let mut r = Rig::with(s);
    r.keys(&[Key::Enter, Key::Down, Key::Down, Key::Enter]); // the agent, "Station einrichten"
    assert_eq!(r.wizard().step, Step::Rooms);
    assert!(
        r.calls()
            .contains(&"rooms claude - https://wave.test".to_owned()),
        "{:?}",
        r.calls()
    );
    r.press(Key::Esc);
    assert!(
        matches!(r.app.top(), Screen::Agent { .. }),
        "Esc where the wizard was opened closes it"
    );
}

// ------------------------------------------------------------------ the loop and the look

#[test]
fn the_loop_draws_every_screen_within_the_terminal_and_ends_on_q() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    for (w, h) in [(50usize, 16usize), (60, 20), (80, 24), (100, 40), (140, 50)] {
        let mut r = Rig::with(s.clone());
        r.term.size = (w, h);
        let mut world = Fake {
            snap: Some(s.clone()),
            ..Fake::default()
        };
        let keys = [
            Key::Down,
            Key::Enter,
            Key::Esc,
            Key::Down,
            Key::Enter,
            Key::Esc,
            Key::Enter,
            Key::Down,
            Key::Enter,
            Key::Esc,
            Key::Char('q'),
        ];
        r.term.keys = keys.into_iter().collect();
        super::run_loop(&mut r.app, &mut world, &mut r.term, ColorMode::None, false);
        assert!(r.app.quit);
        assert!(r.term.frames.len() > 5, "{} frames", r.term.frames.len());
        for f in &r.term.frames {
            assert!(f.len() <= h, "{w}x{h}: {} lines", f.len());
            for l in f {
                assert!(crate::style::width(l) <= w, "{w}x{h}: {l:?}");
            }
        }
    }
}

#[test]
fn a_terminal_that_is_too_small_gets_a_sentence_instead_of_a_broken_screen() {
    let r = Rig::new();
    let lines = view::render(&r.app, 40, 10, ColorMode::None, 0.0);
    assert!(lines.join(" ").contains("zu klein"));
    assert!(view::too_small(40, 10) && !view::too_small(50, 16));
}

#[test]
fn the_banner_plays_once_at_the_start_and_any_key_ends_it() {
    let mut r = Rig::new();
    let mut world = Fake::default();
    r.term.keys = [Key::Down, Key::Char('q')].into_iter().collect();
    super::run_loop(
        &mut r.app,
        &mut world,
        &mut r.term,
        ColorMode::TrueColor,
        true,
    );
    let first = r.term.frames.first().unwrap().join("\n");
    assert!(first.contains("Agenten und ihre Stationen"), "{first}");
    assert!(r.app.crest.is_none(), "the key ended the wave");
}

#[test]
fn the_busy_line_is_drawn_while_wave_is_asked() {
    let mut r = Rig::new();
    r.keys(&[
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
        Key::Enter,
    ]);
    assert!(
        r.term
            .frames
            .iter()
            .any(|f| f.join("\n").contains("Die Räume werden von Wave geholt")),
        "the person sees what is going on while Wave answers"
    );
}

#[test]
fn colour_never_changes_the_width() {
    let mut s = snap();
    s.agents = vec![row("claude")];
    let r = Rig::with(s);
    for mode in [ColorMode::None, ColorMode::Ansi256, ColorMode::TrueColor] {
        for l in view::render(&r.app, 80, 40, mode, 0.5) {
            assert!(crate::style::width(&l) <= 80, "{l:?}");
        }
    }
}

#[test]
fn clip_and_wrap_respect_the_width() {
    assert_eq!(view::clip("abcdef", 4), "abc…");
    assert_eq!(view::clip("abc", 4), "abc");
    let w = view::wrap(
        "ein langer Satz mit einem Wort ohne Ende xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx",
        12,
    );
    assert!(w.iter().all(|l| l.chars().count() <= 12), "{w:?}");
    assert_eq!(view::wrap("a\n\nb", 10), vec!["a", "", "b"]);
}

// ------------------------------------------------------------------ the real machine, in a temporary folder

#[test]
fn the_snapshot_reads_state_profile_key_file_and_lock_from_disk() {
    use crate::agent::dirs::Dirs;
    use crate::agent::exec::fake::{Fake as Machine, ok_out};
    use crate::agent::key::testdir::TempDir;
    let t = TempDir::new("snap");
    let mut d = Dirs::under(t.path());
    d.os = Os::Linux;
    std::fs::create_dir_all(&d.config).unwrap();
    std::fs::create_dir_all(&d.state).unwrap();
    // an agent connected here, with a key file, paused
    crate::agent::state::sample("claude-a")
        .save(&d.state_file("claude-a"))
        .unwrap();
    std::fs::write(d.key_file("claude-a"), "k").unwrap();
    std::fs::write(crate::agent::station_run::paths(&d, "claude-a").paused, "x").unwrap();
    // an agent that exists only as a profile (key in the environment)
    profile::sample("demo").save(&d).unwrap();
    let exec = Machine::new(&["claude"]).with(|p, a| {
        if p.contains("claude") && a == ["--version"] {
            Some(ok_out("2.1.321 (Claude Code)\n"))
        } else {
            None
        }
    });
    let s = super::snapshot(
        &d,
        &exec,
        std::path::Path::new("/opt/sw/stonewave"),
        super::engines(&exec),
    );
    assert_eq!(
        s.agents.iter().map(|a| a.name.as_str()).collect::<Vec<_>>(),
        vec!["claude-a", "demo"]
    );
    let demo = s.agent("demo").unwrap();
    assert!(!demo.connected && demo.profile.is_some());
    assert_eq!(
        demo.key,
        KeyState::Env {
            name: "DEMO_WAVE_KEY".into(),
            set_here: false
        }
    );
    let a = s.agent("claude-a").unwrap();
    assert!(a.connected && a.paused && !a.running && a.key == KeyState::File);
    assert_eq!(
        s.engine(Engine::ClaudeCode).unwrap().version.as_deref(),
        Some("2.1.321 (Claude Code)")
    );
    assert_eq!(s.claude.as_deref(), Some("/fake/bin/claude"));
}

#[test]
fn a_long_question_scrolls_above_its_answers_and_the_answers_stay_on_the_screen() {
    let mut s = snap();
    s.agents = vec![with_profile(row("claude"))];
    let mut r = Rig::with(s);
    r.keys(&[
        Key::Enter,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Down,
        Key::Enter,
    ]);
    assert!(matches!(r.app.top(), Screen::Confirm(_)));
    // a plain 80x24 terminal: the service file does not fit, the question does
    let lines = view::render(&r.app, 80, 24, ColorMode::None, 0.0);
    let text = lines.join(
        "
",
    );
    assert!(
        text.contains("Nein") && text.contains("Ja") && text.contains("▼ mehr"),
        "{text}"
    );
    assert!(
        !text.contains("WantedBy"),
        "the end of the file is further down"
    );
    r.press(Key::PageDown);
    r.press(Key::PageDown);
    let text = view::render(&r.app, 80, 24, ColorMode::None, 0.0).join(
        "
",
    );
    assert!(
        text.contains("WantedBy") && text.contains("Nein") && text.contains("Ja"),
        "{text}"
    );
}

#[test]
fn a_refusal_shows_right_under_the_field_even_on_a_small_terminal() {
    let mut r = Rig::new();
    r.keys(&[Key::Enter, Key::Down, Key::Enter, Key::Enter, Key::Enter]);
    assert_eq!(r.wizard().step, Step::WaveUrl);
    r.press(Key::CtrlU);
    r.text("http://example.com");
    r.press(Key::Enter);
    let lines = view::render(&r.app, 80, 24, ColorMode::None, 0.0);
    let at = lines.iter().position(|l| l.contains("Wave  http")).unwrap();
    assert!(
        lines[at + 1].contains("Diese Adresse geht nicht"),
        "{lines:?}"
    );
}

/// `cargo test print_screens -- --ignored --nocapture` shows the screens as text (for a look at the layout).
#[test]
#[ignore]
fn print_screens() {
    let mut s = snap();
    let mut demo = with_profile(row("demo"));
    demo.running = true;
    demo.service = ServiceState::Installed { active: true };
    s.agents = vec![demo, row("claude")];
    let mut r = Rig::with(s);
    let show = |r: &Rig, title: &str| {
        println!("===== {title}");
        for l in view::render(&r.app, 84, 34, ColorMode::None, 0.0) {
            println!("|{l}");
        }
    };
    show(&r, "main");
    r.press(Key::Enter);
    show(&r, "agent");
    r.press(Key::Esc);
    r.keys(&[Key::Down, Key::Down, Key::Enter]);
    show(&r, "wizard mode");
    r.keys(&[Key::Down, Key::Enter]);
    show(&r, "engine (existing)");
    r.press(Key::Enter);
    show(&r, "name");
    r.press(Key::Enter);
    show(&r, "wave url");
    r.press(Key::Enter);
    show(&r, "key source");
    r.press(Key::Enter);
    show(&r, "rooms");
}
