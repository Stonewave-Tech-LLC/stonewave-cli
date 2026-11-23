//! The menu as a state machine: keys and results go in, effects (things to do on the machine) come out. Nothing in here touches the
//! terminal, the disk or the network, so every screen and every step of the wizard is tested by pressing keys.
//!
//! The menu only drives commands that exist (`stonewave agent connect`, `agent station run ...`, `agent disconnect`, ...). It never
//! does by itself what those commands check: every wake, every limit and every folder check stays where it is.

use crate::agent::dirs::Os;
use crate::agent::engine::Engine;
use crate::connect::{Account, is_agent_slug};

use super::profile::{Profile, plain_word};
use super::service::{self, ServiceState};

// ------------------------------------------------------------------ input

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Key {
    Up,
    Down,
    Left,
    Right,
    Home,
    End,
    PageUp,
    PageDown,
    Enter,
    Esc,
    Tab,
    BackTab,
    Backspace,
    Delete,
    Char(char),
    CtrlC,
    CtrlU,
}

/// One line of text being typed.
#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct Input {
    chars: Vec<char>,
    pub cursor: usize,
}

const INPUT_MAX: usize = 300;

impl Input {
    pub fn with(text: &str) -> Input {
        let chars: Vec<char> = text
            .chars()
            .filter(|c| !c.is_control())
            .take(INPUT_MAX)
            .collect();
        Input {
            cursor: chars.len(),
            chars,
        }
    }

    pub fn value(&self) -> String {
        self.chars.iter().collect()
    }

    pub fn is_empty(&self) -> bool {
        self.chars.is_empty()
    }

    pub fn chars(&self) -> &[char] {
        &self.chars
    }

    /// Returns whether the key was an editing key.
    pub fn edit(&mut self, key: Key) -> bool {
        match key {
            Key::Char(c) if !c.is_control() => {
                if self.chars.len() < INPUT_MAX {
                    self.chars.insert(self.cursor, c);
                    self.cursor += 1;
                }
            }
            Key::Backspace => {
                if self.cursor > 0 {
                    self.cursor -= 1;
                    self.chars.remove(self.cursor);
                }
            }
            Key::Delete => {
                if self.cursor < self.chars.len() {
                    self.chars.remove(self.cursor);
                }
            }
            Key::Left => self.cursor = self.cursor.saturating_sub(1),
            Key::Right => self.cursor = (self.cursor + 1).min(self.chars.len()),
            Key::Home => self.cursor = 0,
            Key::End => self.cursor = self.chars.len(),
            Key::CtrlU => {
                self.chars.clear();
                self.cursor = 0;
            }
            _ => return false,
        }
        true
    }
}

// ------------------------------------------------------------------ what the menu knows about this machine

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRow {
    pub engine: Engine,
    pub found: bool,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyState {
    /// The key file `agent connect` made.
    File,
    FileMissing,
    /// Taken from an environment variable of the station's own process.
    Env {
        name: String,
        set_here: bool,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub name: String,
    pub engine: String,
    pub key: KeyState,
    /// A state file of `agent connect` exists (the agent was signed in from this computer).
    pub connected: bool,
    pub running: bool,
    pub paused: bool,
    pub service: ServiceState,
    pub profile: Option<Profile>,
    pub wave_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Snapshot {
    pub engines: Vec<EngineRow>,
    pub agents: Vec<AgentRow>,
    pub root: bool,
    pub os: Os,
    /// This program, as a full path.
    pub exe: String,
    pub home: String,
    pub config_dir: String,
    pub state_dir: String,
    /// Where Wave probably is: `STONEWAVE_WAVE_URL`, or the address of an agent connected here.
    pub wave_hint: Option<String>,
    /// Claude Code as a full path, when it was found.
    pub claude: Option<String>,
    pub path_env: Option<String>,
    pub host: String,
}

impl Snapshot {
    pub fn agent(&self, name: &str) -> Option<&AgentRow> {
        self.agents.iter().find(|a| a.name == name)
    }

    pub fn engine(&self, e: Engine) -> Option<&EngineRow> {
        self.engines.iter().find(|r| r.engine == e)
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Room {
    pub id: String,
    pub name: String,
}

// ------------------------------------------------------------------ effects and their results

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Effect {
    Refresh,
    /// Runs this program with these words (`stonewave <args>`), collects what it writes.
    Capture {
        title: String,
        args: Vec<String>,
    },
    /// Runs it with the terminal handed over: the menu steps aside, the person sees and answers it directly.
    Interactive {
        args: Vec<String>,
    },
    FetchRooms {
        agent: String,
        key_env: Option<String>,
        wave_url: String,
    },
    SaveProfile(Profile),
    InstallService(Profile),
    ServiceControl {
        agent: String,
        what: &'static str,
    },
    RemoveService {
        agent: String,
    },
    /// Stops and removes the service (when there is one) and deletes the profile.
    Forget {
        agent: String,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Event {
    Snapshot(Box<Snapshot>),
    Output { code: i32, lines: Vec<String> },
    Returned { code: i32 },
    Rooms(Result<Vec<Room>, String>),
    Done(Result<Vec<String>, String>),
}

/// What the menu is waiting for: how the next result is to be read.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Await {
    Nothing,
    Connect,
    Rooms,
    /// The profile is being saved; the preview follows.
    SaveForPreview(Profile),
    Preview,
    Test,
    Install,
    /// A command whose output goes on a screen of its own.
    Show(String),
    /// Something that just needs a line saying it worked.
    Told(String),
    Foreground,
    /// `agent disconnect` is running; what is left of the agent is removed after it.
    Disconnect(String),
}

// ------------------------------------------------------------------ screens

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Level {
    Ok,
    Warn,
    Bad,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum AgentAction {
    Status,
    CheckWave,
    SetUpStation,
    RunHere,
    Pause,
    Resume,
    StopService,
    StartService,
    InstallService,
    RemoveService,
    Disconnect,
}

impl AgentAction {
    pub fn label(&self) -> &'static str {
        match self {
            AgentAction::Status => "Status der Station",
            AgentAction::CheckWave => "Verbindung zu Wave prüfen",
            AgentAction::SetUpStation => "Station einrichten …",
            AgentAction::RunHere => "Station jetzt hier starten (Vordergrund)",
            AgentAction::Pause => "Pausieren",
            AgentAction::Resume => "Fortsetzen",
            AgentAction::StopService => "Station stoppen (Dienst anhalten)",
            AgentAction::StartService => "Dienst starten",
            AgentAction::InstallService => "Dauerhaft starten (Dienst einrichten) …",
            AgentAction::RemoveService => "Dienst entfernen",
            AgentAction::Disconnect => "Trennen …",
        }
    }
}

/// What can be done with an agent right now, in this order.
pub fn agent_actions(a: &AgentRow, os: Os) -> Vec<AgentAction> {
    let mut v = vec![AgentAction::Status];
    if a.connected {
        v.push(AgentAction::CheckWave);
    }
    if a.profile.is_none() {
        if a.engine == "claude-code" {
            v.push(AgentAction::SetUpStation);
        }
    } else {
        v.push(AgentAction::RunHere);
        v.push(if a.paused {
            AgentAction::Resume
        } else {
            AgentAction::Pause
        });
        match a.service {
            ServiceState::Installed { active: true } => {
                v.push(AgentAction::StopService);
                v.push(AgentAction::RemoveService);
            }
            ServiceState::Installed { active: false } => {
                v.push(AgentAction::StartService);
                v.push(AgentAction::RemoveService);
            }
            ServiceState::None => {
                if service::supported(os) {
                    v.push(AgentAction::InstallService);
                }
            }
        }
        v.push(AgentAction::SetUpStation);
    }
    v.push(AgentAction::Disconnect);
    v
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Screen {
    Main {
        cursor: usize,
    },
    Agent {
        name: String,
        cursor: usize,
    },
    Wizard(Box<Wizard>),
    Output {
        title: String,
        lines: Vec<String>,
        scroll: usize,
    },
    Confirm(Box<Confirm>),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Confirm {
    pub title: String,
    pub lines: Vec<String>,
    /// 0 = no, 1 = yes.
    pub cursor: usize,
    /// First visible line of a text that is longer than the screen.
    pub scroll: usize,
    pub yes: Effect,
    pub await_: Await,
}

/// How many entries the main screen has besides the agents: connect, status, quit.
pub const MAIN_EXTRAS: usize = 3;

// ------------------------------------------------------------------ the wizard

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    /// Sign in with the browser (a new agent key from Wave).
    New,
    /// An agent that exists in Wave already, with its key in a secret store.
    Existing,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
pub enum Step {
    Mode,
    Engine,
    Name,
    WaveUrl,
    KeySource,
    SignIn,
    StationAsk,
    Rooms,
    Workdir,
    Limits,
    Preview,
    Test,
    Service,
    Done,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RoomChoice {
    pub id: String,
    pub name: String,
    pub on: bool,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Wizard {
    pub mode: Mode,
    pub step: Step,
    /// The step the wizard was opened at (Esc there closes it).
    pub entry: Step,
    pub engine: Option<Engine>,
    pub name: Input,
    pub wave_url: Input,
    pub key_env: Input,
    pub launcher: Input,
    pub workdir: Input,
    /// Which text field has the focus (key source step: 0 = variable, 1 = secret-store command).
    pub field: usize,
    pub cursor: usize,
    pub rooms: Vec<RoomChoice>,
    pub rooms_loaded: bool,
    pub output: Vec<String>,
    pub scroll: usize,
    pub message: Option<(Level, String)>,
    pub signed_in: bool,
    pub test_ok: bool,
    pub notes: Vec<String>,
}

impl Wizard {
    fn blank(mode: Mode, step: Step) -> Wizard {
        Wizard {
            mode,
            step,
            entry: step,
            engine: None,
            name: Input::default(),
            wave_url: Input::default(),
            key_env: Input::with("WAVE_AGENT_KEY"),
            launcher: Input::default(),
            workdir: Input::default(),
            field: 0,
            cursor: 0,
            rooms: Vec::new(),
            rooms_loaded: false,
            output: Vec::new(),
            scroll: 0,
            message: None,
            signed_in: false,
            test_ok: false,
            notes: Vec::new(),
        }
    }

    pub fn start() -> Wizard {
        Wizard::blank(Mode::New, Step::Mode)
    }

    /// The station setup of an agent that exists already: starts at the rooms.
    pub fn for_agent(a: &AgentRow, snap: &Snapshot) -> Wizard {
        let mut w = Wizard::blank(
            if a.connected {
                Mode::New
            } else {
                Mode::Existing
            },
            Step::Rooms,
        );
        w.engine = Some(Engine::ClaudeCode);
        w.name = Input::with(&a.name);
        w.signed_in = a.connected;
        let wave = a
            .profile
            .as_ref()
            .map(|p| p.wave_url.clone())
            .or_else(|| a.wave_url.clone())
            .or_else(|| snap.wave_hint.clone())
            .unwrap_or_default();
        w.wave_url = Input::with(&wave);
        if let Some(p) = &a.profile {
            w.key_env = Input::with(p.key_env.as_deref().unwrap_or("WAVE_AGENT_KEY"));
            w.launcher = Input::with(&p.launcher.join(" "));
            w.workdir = Input::with(&p.workdir);
        } else {
            w.workdir = Input::with(&default_workdir(snap, &a.name));
        }
        w
    }

    pub fn agent(&self) -> String {
        self.name.value().trim().to_owned()
    }

    /// The key variable of this agent, or none when its key file is used.
    pub fn key_env_name(&self) -> Option<String> {
        (self.mode == Mode::Existing).then(|| self.key_env.value().trim().to_owned())
    }

    pub fn launcher_words(&self) -> Vec<String> {
        if self.mode == Mode::Existing {
            self.launcher
                .value()
                .split_whitespace()
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        }
    }

    pub fn chosen_rooms(&self) -> Vec<&RoomChoice> {
        self.rooms.iter().filter(|r| r.on).collect()
    }

    /// The profile this wizard describes, once the rooms are chosen.
    pub fn profile(&self) -> Profile {
        let chosen = self.chosen_rooms();
        Profile {
            agent: self.agent(),
            engine: self.engine.unwrap_or(Engine::ClaudeCode).name().to_owned(),
            wave_url: self.wave_url.value().trim().to_owned(),
            key_env: self.key_env_name(),
            launcher: self.launcher_words(),
            workdir: self.workdir.value().trim().to_owned(),
            rooms: chosen.iter().map(|r| r.id.clone()).collect(),
            room_names: chosen.iter().map(|r| r.name.clone()).collect(),
        }
    }

    fn next_step(&self) -> Step {
        match (self.step, self.mode) {
            (Step::Mode, _) => Step::Engine,
            (Step::Engine, _) => Step::Name,
            (Step::Name, Mode::New) => Step::SignIn,
            (Step::Name, Mode::Existing) => Step::WaveUrl,
            (Step::WaveUrl, _) => Step::KeySource,
            (Step::KeySource, _) => Step::Rooms,
            (Step::SignIn, _) => {
                if self.engine == Some(Engine::ClaudeCode) {
                    Step::StationAsk
                } else {
                    Step::Done
                }
            }
            (Step::StationAsk, _) => Step::Rooms,
            (Step::Rooms, _) => Step::Workdir,
            (Step::Workdir, _) => Step::Limits,
            (Step::Limits, _) => Step::Preview,
            (Step::Preview, _) => Step::Test,
            (Step::Test, _) => Step::Service,
            (Step::Service, _) | (Step::Done, _) => Step::Done,
        }
    }

    fn prev_step(&self) -> Option<Step> {
        if self.step == self.entry {
            return None;
        }
        Some(match (self.step, self.mode) {
            (Step::Mode, _) => return None,
            (Step::Engine, _) => Step::Mode,
            (Step::Name, _) => Step::Engine,
            (Step::WaveUrl, _) => Step::Name,
            (Step::KeySource, _) => Step::WaveUrl,
            // after signing in there is no way back to signing in
            (Step::SignIn, _) => Step::Name,
            (Step::StationAsk, _) => return None,
            (Step::Rooms, Mode::New) => Step::StationAsk,
            (Step::Rooms, Mode::Existing) => Step::KeySource,
            (Step::Workdir, _) => Step::Rooms,
            (Step::Limits, _) => Step::Workdir,
            (Step::Preview, _) => Step::Limits,
            (Step::Test, _) => Step::Preview,
            (Step::Service, _) => Step::Test,
            (Step::Done, _) => return None,
        })
    }
}

pub fn default_workdir(snap: &Snapshot, agent: &str) -> String {
    let sep = if snap.os == Os::Windows { '\\' } else { '/' };
    format!(
        "{}{sep}wave-agents{sep}{agent}",
        snap.home.trim_end_matches(['/', '\\'])
    )
}

/// The words of a command for one station: the options the menu shows and the service file holds.
pub fn station_command(p: &Profile, claude: Option<&str>, extra: &[&str]) -> Vec<String> {
    service::station_args(p, claude, extra)
}

fn short_name(e: Engine) -> &'static str {
    match e {
        Engine::ClaudeCode => "claude",
        Engine::Codex => "codex",
        Engine::Grok => "grok",
        Engine::Other => "agent",
    }
}

fn free_name(snap: &Snapshot, base: &str) -> String {
    let mut n = base.to_owned();
    let mut i = 2;
    while snap.agent(&n).is_some() {
        n = format!("{base}-{i}");
        i += 1;
    }
    n
}

/// Whether the engine can be connected here and woken by a station.
pub fn engine_choosable(e: Engine, mode: Mode) -> bool {
    match mode {
        Mode::New => matches!(e, Engine::ClaudeCode | Engine::Codex | Engine::Grok),
        Mode::Existing => e == Engine::ClaudeCode,
    }
}

pub fn engine_note(e: Engine, mode: Mode) -> &'static str {
    match (e, mode) {
        (Engine::ClaudeCode, _) => "Claude Code: anbinden und per Station wecken",
        (Engine::Codex, Mode::New) | (Engine::Grok, Mode::New) => {
            "anbinden; die Station dafür kommt noch"
        }
        (Engine::Codex, Mode::Existing) | (Engine::Grok, Mode::Existing) => {
            "die Station dafür kommt noch"
        }
        _ => "",
    }
}

pub fn install_hint(e: Engine) -> &'static str {
    match e {
        Engine::ClaudeCode => "npm install -g @anthropic-ai/claude-code",
        Engine::Codex => "npm install -g @openai/codex",
        Engine::Grok => "Installation siehe Anleitung von xAI",
        Engine::Other => "",
    }
}

// ------------------------------------------------------------------ the app

#[derive(Debug, Clone)]
pub struct App {
    pub snap: Snapshot,
    pub stack: Vec<Screen>,
    pub notice: Option<(Level, String)>,
    pub busy: Option<String>,
    pub quit: bool,
    /// While the banner plays: where the crest of the wave is.
    pub crest: Option<f32>,
    /// Whether an environment variable is set in the menu's own shell (never its value).
    pub env_probe: fn(&str) -> bool,
    awaiting: Await,
}

pub fn real_env_probe(name: &str) -> bool {
    std::env::var_os(name).is_some_and(|v| !v.is_empty())
}

fn plain_text(s: &str) -> String {
    s.chars().filter(|c| !c.is_control()).collect()
}

impl App {
    pub fn new(snap: Snapshot) -> App {
        App {
            snap,
            stack: vec![Screen::Main { cursor: 0 }],
            notice: None,
            busy: None,
            quit: false,
            crest: None,
            env_probe: real_env_probe,
            awaiting: Await::Nothing,
        }
    }

    /// Whether the screen on top shows what the machine says (the main screen and an agent's page).
    pub fn shows_agents(&self) -> bool {
        matches!(self.top(), Screen::Main { .. } | Screen::Agent { .. })
    }

    pub fn top(&self) -> &Screen {
        self.stack.last().expect("the main screen is always there")
    }

    fn say(&mut self, level: Level, text: &str) {
        self.notice = Some((level, plain_text(text)));
    }

    // ---------------------------------------------------------------- keys

    pub fn on_key(&mut self, key: Key) -> Vec<Effect> {
        if key == Key::CtrlC {
            self.quit = true;
            return vec![];
        }
        if self.busy.is_some() {
            return vec![];
        }
        self.notice = None;
        let Some(screen) = self.stack.pop() else {
            self.quit = true;
            return vec![];
        };
        let fx = match screen {
            Screen::Main { cursor } => self.key_main(cursor, key),
            Screen::Agent { name, cursor } => self.key_agent(name, cursor, key),
            Screen::Wizard(w) => self.key_wizard(*w, key),
            Screen::Output {
                title,
                lines,
                scroll,
            } => self.key_output(title, lines, scroll, key),
            Screen::Confirm(c) => self.key_confirm(*c, key),
        };
        if self.stack.is_empty() {
            self.quit = true;
        }
        fx
    }

    fn move_cursor(cursor: usize, len: usize, key: Key) -> usize {
        match key {
            Key::Up | Key::Char('k') => cursor.saturating_sub(1),
            Key::Down | Key::Char('j') => (cursor + 1).min(len.saturating_sub(1)),
            Key::Home => 0,
            Key::End => len.saturating_sub(1),
            _ => cursor,
        }
    }

    fn key_main(&mut self, cursor: usize, key: Key) -> Vec<Effect> {
        let n = if self.snap.root {
            1
        } else {
            self.snap.agents.len() + MAIN_EXTRAS
        };
        if matches!(key, Key::Char('q' | 'Q') | Key::Esc) {
            return vec![];
        }
        let cursor = App::move_cursor(cursor, n, key);
        if key != Key::Enter {
            self.stack.push(Screen::Main { cursor });
            return vec![];
        }
        if self.snap.root {
            self.stack.push(Screen::Main { cursor });
            return vec![];
        }
        let agents = self.snap.agents.len();
        if cursor < agents {
            let name = self.snap.agents[cursor].name.clone();
            self.stack.push(Screen::Main { cursor });
            self.stack.push(Screen::Agent { name, cursor: 0 });
            return vec![];
        }
        self.stack.push(Screen::Main { cursor });
        match cursor - agents {
            0 => self.stack.push(Screen::Wizard(Box::new(Wizard::start()))),
            1 => {
                let lines = super::view::status_lines(&self.snap);
                self.stack.push(Screen::Output {
                    title: "Status".into(),
                    lines,
                    scroll: 0,
                });
            }
            _ => {
                self.stack.pop();
            }
        }
        vec![]
    }

    fn key_output(
        &mut self,
        title: String,
        lines: Vec<String>,
        scroll: usize,
        key: Key,
    ) -> Vec<Effect> {
        let scroll = match key {
            Key::Up | Key::Char('k') => scroll.saturating_sub(1),
            Key::Down | Key::Char('j') => scroll + 1,
            Key::PageUp => scroll.saturating_sub(10),
            Key::PageDown => scroll + 10,
            Key::Home => 0,
            Key::End => usize::MAX / 2,
            Key::Enter | Key::Esc | Key::Char('q') => return vec![],
            _ => scroll,
        };
        self.stack.push(Screen::Output {
            title,
            lines,
            scroll,
        });
        vec![]
    }

    fn confirm(&mut self, title: String, lines: Vec<String>, yes: Effect, await_: Await) {
        self.stack.push(Screen::Confirm(Box::new(Confirm {
            title,
            lines,
            cursor: 0,
            scroll: 0,
            yes,
            await_,
        })));
    }

    fn key_confirm(&mut self, mut c: Confirm, key: Key) -> Vec<Effect> {
        match key {
            Key::Esc | Key::Char('n' | 'N' | 'q') => return vec![],
            Key::Char('y' | 'Y' | 'j' | 'J') => return self.confirmed(c),
            Key::PageUp => c.scroll = c.scroll.saturating_sub(5),
            Key::PageDown => c.scroll += 5,
            Key::Up | Key::Down | Key::Left | Key::Right | Key::Tab | Key::BackTab => {
                c.cursor = 1 - c.cursor
            }
            Key::Enter if c.cursor == 1 => return self.confirmed(c),
            // Enter on "Nein" (the default) is a no
            Key::Enter => return vec![],
            _ => {}
        }
        self.stack.push(Screen::Confirm(Box::new(c)));
        vec![]
    }

    fn confirmed(&mut self, c: Confirm) -> Vec<Effect> {
        if !matches!(c.yes, Effect::Interactive { .. }) {
            self.busy = Some("Einen Moment …".into());
        }
        self.awaiting = c.await_;
        vec![c.yes]
    }

    fn key_agent(&mut self, name: String, cursor: usize, key: Key) -> Vec<Effect> {
        let Some(row) = self.snap.agent(&name).cloned() else {
            // the agent is gone (disconnected): back to the main screen
            return vec![];
        };
        let actions = agent_actions(&row, self.snap.os);
        if matches!(key, Key::Esc | Key::Char('q' | 'Q') | Key::Left) {
            return vec![];
        }
        let cursor =
            App::move_cursor(cursor, actions.len(), key).min(actions.len().saturating_sub(1));
        self.stack.push(Screen::Agent { name, cursor });
        if key != Key::Enter {
            return vec![];
        }
        let action = actions[cursor].clone();
        self.do_action(&row, action)
    }

    fn capture(&mut self, title: &str, args: Vec<String>, then: Await, busy: &str) -> Vec<Effect> {
        self.awaiting = then;
        self.busy = Some(busy.to_owned());
        vec![Effect::Capture {
            title: title.to_owned(),
            args,
        }]
    }

    fn do_action(&mut self, row: &AgentRow, action: AgentAction) -> Vec<Effect> {
        let name = row.name.clone();
        let claude = self.snap.claude.clone();
        let words = |a: &[&str]| -> Vec<String> { a.iter().map(|s| (*s).to_owned()).collect() };
        match action {
            AgentAction::Status => {
                let title = format!("Status von {name}");
                let args = words(&["agent", "station", "status", "--agent", &name]);
                self.capture(
                    &title,
                    args,
                    Await::Show(title.clone()),
                    "Status wird gelesen …",
                )
            }
            AgentAction::CheckWave => {
                let title = format!("Verbindung von {name} zu Wave");
                let args = words(&["agent", "status", "--agent", &name]);
                self.capture(
                    &title,
                    args,
                    Await::Show(title.clone()),
                    "Wave wird gefragt …",
                )
            }
            AgentAction::SetUpStation => {
                let w = Wizard::for_agent(row, &self.snap);
                let fx = self.fetch_rooms(&w);
                self.stack.push(Screen::Wizard(Box::new(w)));
                fx
            }
            AgentAction::RunHere => {
                let Some(p) = &row.profile else { return vec![] };
                let mut args = vec!["agent".to_owned()];
                args.extend(station_command(p, claude.as_deref(), &[]));
                let mut lines = vec![
                    "Die Station läuft in diesem Terminal und wacht über den Posteingang von Wave."
                        .to_owned(),
                    "Strg-C beendet sie und bringt dich zurück ins Menü.".to_owned(),
                    String::new(),
                    format!("Räume: {}", p.room_names.join(", ")),
                ];
                if let (
                    Some(n),
                    KeyState::Env {
                        set_here: false, ..
                    },
                ) = (&p.key_env, &row.key)
                {
                    lines.push(String::new());
                    lines.push(format!(
                        "Achtung: die Variable {n} ist in dieser Shell nicht gesetzt. Beende das Menü und starte es durch den Secret-Store-Befehl."
                    ));
                }
                self.confirm(
                    format!("Station von {name} jetzt starten?"),
                    lines,
                    Effect::Interactive { args },
                    Await::Foreground,
                );
                vec![]
            }
            AgentAction::Pause => {
                let said = "Pausiert: es startet kein neuer Weckruf (ein laufender endet normal).";
                self.confirm(
                    format!("Station von {name} pausieren?"),
                    vec!["Die Station startet keinen neuen Weckruf mehr, bis du sie fortsetzt. Ein Weckruf, der gerade läuft, endet normal.".to_owned()],
                    Effect::Capture { title: said.into(), args: words(&["agent", "station", "pause", "--agent", &name]) },
                    Await::Told(said.into()),
                );
                vec![]
            }
            AgentAction::Resume => {
                self.confirm(
                    format!("Station von {name} fortsetzen?"),
                    vec!["Die Station weckt den Agenten wieder.".to_owned()],
                    Effect::Capture {
                        title: "Fortgesetzt.".into(),
                        args: words(&["agent", "station", "resume", "--agent", &name]),
                    },
                    Await::Told("Fortgesetzt.".into()),
                );
                vec![]
            }
            AgentAction::StopService => {
                self.confirm(
                    format!("Station von {name} stoppen?"),
                    vec!["Der Dienst wird angehalten; ein laufender Weckruf wird beendet. Eingerichtet bleibt er: \"Dienst starten\" nimmt ihn wieder auf.".to_owned()],
                    Effect::ServiceControl { agent: name, what: "stop" },
                    Await::Told("Der Dienst ist angehalten.".into()),
                );
                vec![]
            }
            AgentAction::StartService => {
                self.confirm(
                    format!("Dienst von {name} starten?"),
                    vec!["Der eingerichtete Dienst wird gestartet.".to_owned()],
                    Effect::ServiceControl {
                        agent: name,
                        what: "start",
                    },
                    Await::Told("Der Dienst läuft.".into()),
                );
                vec![]
            }
            AgentAction::InstallService => {
                let Some(p) = &row.profile else { return vec![] };
                match self.install_confirm(p) {
                    Ok((title, lines)) => self.confirm(
                        title,
                        lines,
                        Effect::InstallService(p.clone()),
                        Await::Told("Der Dienst läuft.".into()),
                    ),
                    Err(e) => self.say(Level::Bad, &e),
                }
                vec![]
            }
            AgentAction::RemoveService => {
                self.confirm(
                    format!("Dienst von {name} entfernen?"),
                    vec!["Der Dienst wird angehalten und seine Datei gelöscht. Die Einrichtung des Agenten und der Schlüssel bleiben.".to_owned()],
                    Effect::RemoveService { agent: name },
                    Await::Told("Der Dienst ist entfernt.".into()),
                );
                vec![]
            }
            AgentAction::Disconnect => {
                let mut lines = Vec::new();
                if matches!(row.service, ServiceState::Installed { .. }) {
                    lines.push("Der Dienst wird angehalten und entfernt.".to_owned());
                }
                let (yes, await_) = if row.connected {
                    lines.push("Der Agent wird in Wave abgemeldet und sein Schlüssel auf diesem Computer gelöscht (`stonewave agent disconnect`). Danach wird alles Übrige gelöscht.".to_owned());
                    (
                        Effect::Interactive {
                            args: words(&["agent", "disconnect", "--agent", &name, "--yes"]),
                        },
                        Await::Disconnect(name.clone()),
                    )
                } else {
                    lines.push("Die Einrichtung des Agenten auf diesem Computer wird gelöscht. In Wave und im Secret-Store bleibt er unverändert.".to_owned());
                    (
                        Effect::Forget {
                            agent: name.clone(),
                        },
                        Await::Told(format!("{name} ist getrennt.")),
                    )
                };
                lines.push("Die Merkdatei der Station-Einrichtung wird gelöscht.".to_owned());
                self.confirm(format!("{name} trennen?"), lines, yes, await_);
                vec![]
            }
        }
    }

    /// The title and the lines of the question before a service is set up: the whole file, as it will be written.
    fn install_confirm(&self, p: &Profile) -> Result<(String, Vec<String>), String> {
        if p.key_env.is_some() && p.launcher.is_empty() {
            return Err("Der Dienst braucht den Befehl deines Secret-Stores, der den Schlüssel setzt. Richte die Station neu ein und trage ihn ein.".into());
        }
        let unit = service::systemd_unit(
            p,
            &self.snap.exe,
            self.snap.claude.as_deref(),
            self.snap.path_env.as_deref(),
        );
        let mut lines = vec![
            "Das Menü schreibt diese Datei und startet sie (`systemctl --user`, ohne Root):"
                .to_owned(),
            String::new(),
        ];
        lines.extend(unit.lines().map(str::to_owned));
        Ok((format!("Station von {} dauerhaft starten?", p.agent), lines))
    }

    // ---------------------------------------------------------------- the wizard

    fn key_wizard(&mut self, mut w: Wizard, key: Key) -> Vec<Effect> {
        let mut fx = Vec::new();
        if key == Key::Esc || (key == Key::Left && !wizard_has_text(&w)) {
            // back one step; where the wizard was opened, it closes
            return match w.prev_step() {
                Some(prev) => {
                    w.message = None;
                    w.step = prev;
                    w.cursor = 0;
                    self.stack.push(Screen::Wizard(Box::new(w)));
                    vec![]
                }
                // the wizard closes; what it made (an agent, a profile) shows on the screen below
                None => vec![Effect::Refresh],
            };
        }
        match w.step {
            Step::Mode => {
                w.cursor = App::move_cursor(w.cursor, 2, key);
                if key == Key::Enter {
                    w.mode = if w.cursor == 0 {
                        Mode::New
                    } else {
                        Mode::Existing
                    };
                    self.advance(&mut w, &mut fx);
                }
            }
            Step::Engine => {
                let rows = engine_rows();
                w.cursor = App::move_cursor(w.cursor, rows.len(), key);
                if key == Key::Enter {
                    let e = rows[w.cursor];
                    let found = self.snap.engine(e).is_some_and(|r| r.found);
                    if !engine_choosable(e, w.mode) {
                        w.message = Some((
                            Level::Warn,
                            format!(
                                "{} lässt sich auf diesem Weg noch nicht anbinden.",
                                e.label()
                            ),
                        ));
                    } else if !found {
                        w.message = Some((
                            Level::Warn,
                            format!(
                                "{} ist hier nicht installiert. Installieren: {}",
                                e.label(),
                                install_hint(e)
                            ),
                        ));
                    } else {
                        w.engine = Some(e);
                        w.name = Input::with(&free_name(&self.snap, short_name(e)));
                        self.advance(&mut w, &mut fx);
                    }
                }
            }
            Step::Name => {
                if key == Key::Enter {
                    let n = w.agent();
                    if !is_agent_slug(&n) {
                        w.message = Some((Level::Bad, "Ein Name hat nur Kleinbuchstaben, Ziffern und Bindestriche (zum Beispiel demo).".into()));
                    } else if self.snap.agent(&n).is_some() {
                        w.message = Some((
                            Level::Bad,
                            "Diesen Namen gibt es hier schon. Wähle einen anderen.".into(),
                        ));
                    } else {
                        w.workdir = Input::with(&default_workdir(&self.snap, &n));
                        if w.wave_url.is_empty() {
                            w.wave_url = Input::with(self.snap.wave_hint.as_deref().unwrap_or(""));
                        }
                        self.advance(&mut w, &mut fx);
                    }
                } else {
                    w.message = None;
                    w.name.edit(key);
                }
            }
            Step::WaveUrl => {
                if key == Key::Enter {
                    match Account::parse_as("Wave", w.wave_url.value().trim()) {
                        Ok(a) => {
                            w.wave_url = Input::with(a.base());
                            self.advance(&mut w, &mut fx);
                        }
                        Err(e) => {
                            w.message = Some((Level::Bad, format!("Diese Adresse geht nicht: {e}")))
                        }
                    }
                } else {
                    w.message = None;
                    w.wave_url.edit(key);
                }
            }
            Step::KeySource => match key {
                Key::Tab | Key::Down | Key::BackTab | Key::Up => w.field = 1 - w.field,
                Key::Enter => {
                    let var = w.key_env.value().trim().to_owned();
                    if !crate::agent::station_cli::env_name_ok(&var) {
                        w.message = Some((Level::Bad, "Das ist der NAME der Umgebungsvariable (Großbuchstaben, Ziffern, Unterstriche), nicht der Schlüssel.".into()));
                        w.field = 0;
                    } else if !w.launcher_words().iter().all(|x| plain_word(x)) {
                        w.message = Some((Level::Bad, "Im Befehl sind nur Buchstaben, Ziffern und - _ . / = : @ + , erlaubt, getrennt durch Leerzeichen.".into()));
                        w.field = 1;
                    } else {
                        self.advance(&mut w, &mut fx);
                    }
                }
                _ => {
                    w.message = None;
                    if w.field == 0 {
                        w.key_env.edit(key);
                    } else {
                        w.launcher.edit(key);
                    }
                }
            },
            Step::SignIn => {
                if key == Key::Enter {
                    let engine = w.engine.unwrap_or(Engine::ClaudeCode).name().to_owned();
                    let args = [
                        "agent",
                        "connect",
                        "--engine",
                        &engine,
                        "--name",
                        &w.agent(),
                        "--no-station",
                    ]
                    .iter()
                    .map(|s| (*s).to_owned())
                    .collect();
                    self.awaiting = Await::Connect;
                    fx.push(Effect::Interactive { args });
                }
            }
            Step::StationAsk => {
                w.cursor = App::move_cursor(w.cursor, 2, key);
                if key == Key::Enter {
                    if w.cursor == 0 {
                        self.advance(&mut w, &mut fx);
                    } else {
                        w.step = Step::Done;
                        w.notes.push(
                            "Die Station kannst du später auf der Seite des Agenten einrichten."
                                .into(),
                        );
                    }
                }
            }
            Step::Rooms => {
                if !w.rooms_loaded {
                    // Enter or R tries again after a failure
                    if matches!(key, Key::Enter | Key::Char('r' | 'R')) {
                        w.message = None;
                        fx = self.fetch_rooms(&w);
                    }
                } else {
                    w.cursor = App::move_cursor(w.cursor, w.rooms.len(), key);
                    match key {
                        Key::Char(' ') => {
                            if let Some(r) = w.rooms.get_mut(w.cursor) {
                                r.on = !r.on;
                            }
                            w.message = None;
                        }
                        Key::Enter => {
                            if w.chosen_rooms().is_empty() {
                                w.message = Some((
                                    Level::Warn,
                                    "Wähle mindestens einen Raum aus (Leertaste).".into(),
                                ));
                            } else {
                                self.advance(&mut w, &mut fx);
                            }
                        }
                        _ => {}
                    }
                }
            }
            Step::Workdir => {
                if key == Key::Enter {
                    let p = w.workdir.value().trim().to_owned();
                    if p.is_empty() || !super::profile::absolute(&p) {
                        w.message = Some((
                            Level::Bad,
                            "Gib einen vollständigen Pfad an (beginnt mit / oder einem Laufwerk)."
                                .into(),
                        ));
                    } else {
                        w.workdir = Input::with(&p);
                        self.advance(&mut w, &mut fx);
                    }
                } else {
                    w.message = None;
                    w.workdir.edit(key);
                }
            }
            Step::Limits => {
                if key == Key::Enter {
                    let p = w.profile();
                    self.advance(&mut w, &mut fx);
                    self.awaiting = Await::SaveForPreview(p.clone());
                    self.busy = Some("Die Einrichtung wird gemerkt …".into());
                    fx.push(Effect::SaveProfile(p));
                }
            }
            Step::Preview => match key {
                Key::Up | Key::Char('k') => w.scroll = w.scroll.saturating_sub(1),
                Key::Down | Key::Char('j') => w.scroll += 1,
                Key::PageUp => w.scroll = w.scroll.saturating_sub(10),
                Key::PageDown => w.scroll += 10,
                Key::Enter if w.message.is_none() => self.advance(&mut w, &mut fx),
                _ => {}
            },
            Step::Test => match key {
                Key::Enter => {
                    let p = w.profile();
                    self.awaiting = Await::Test;
                    fx.push(Effect::Interactive {
                        args: with_agent(station_command(
                            &p,
                            self.snap.claude.as_deref(),
                            &["--once"],
                        )),
                    });
                }
                Key::Char('s' | 'S') => {
                    w.test_ok = false;
                    self.advance(&mut w, &mut fx);
                }
                _ => {}
            },
            Step::Service => self.key_service(&mut w, key, &mut fx),
            Step::Done => {
                if matches!(key, Key::Enter | Key::Char('q')) {
                    return vec![Effect::Refresh];
                }
            }
        }
        self.stack.push(Screen::Wizard(Box::new(w)));
        fx
    }

    /// Moves to the next step. Arriving at the rooms, they are fetched from Wave.
    fn advance(&mut self, w: &mut Wizard, fx: &mut Vec<Effect>) {
        w.message = None;
        w.cursor = 0;
        w.field = 0;
        w.scroll = 0;
        w.step = w.next_step();
        if w.step == Step::Rooms && !w.rooms_loaded {
            fx.extend(self.fetch_rooms(w));
        }
    }

    fn fetch_rooms(&mut self, w: &Wizard) -> Vec<Effect> {
        self.awaiting = Await::Rooms;
        self.busy = Some("Die Räume werden von Wave geholt …".into());
        vec![Effect::FetchRooms {
            agent: w.agent(),
            key_env: w.key_env_name(),
            wave_url: w.wave_url.value().trim().to_owned(),
        }]
    }

    /// Whether a service can be set up for what the wizard has decided.
    pub fn service_possible(&self, p: &Profile) -> bool {
        service::supported(self.snap.os) && !(p.key_env.is_some() && p.launcher.is_empty())
    }

    fn key_service(&mut self, w: &mut Wizard, key: Key, fx: &mut Vec<Effect>) {
        let p = w.profile();
        if !self.service_possible(&p) {
            if key == Key::Enter {
                self.advance(w, fx);
            }
            return;
        }
        w.cursor = App::move_cursor(w.cursor, 2, key);
        if key == Key::Enter {
            if w.cursor == 0 {
                self.awaiting = Await::Install;
                self.busy = Some("Der Dienst wird eingerichtet …".into());
                fx.push(Effect::InstallService(p));
            } else {
                w.notes.push("Kein Dienst: die Station läuft nur, wenn du sie startest (Seite des Agenten, \"Station jetzt hier starten\").".into());
                self.advance(w, fx);
            }
        }
    }

    // ---------------------------------------------------------------- results

    pub fn on_event(&mut self, ev: Event) -> Vec<Effect> {
        let awaiting = std::mem::replace(&mut self.awaiting, Await::Nothing);
        if !matches!(ev, Event::Snapshot(_)) {
            self.busy = None;
        }
        match ev {
            Event::Snapshot(s) => {
                self.awaiting = awaiting;
                self.snap = *s;
                self.fix_cursors();
            }
            Event::Output { code, lines } => match awaiting {
                Await::Preview => {
                    if let Some(w) = self.wizard() {
                        w.output = lines;
                        w.scroll = 0;
                        if code != 0 {
                            w.message = Some((Level::Bad, "Die Vorschau meldet ein Problem (siehe oben). Mit Esc zurück und ändern.".into()));
                        }
                    }
                }
                Await::Show(title) => self.stack.push(Screen::Output {
                    title,
                    lines,
                    scroll: 0,
                }),
                Await::Told(msg) => {
                    if code == 0 {
                        self.say(Level::Ok, &msg);
                    } else {
                        let first = lines
                            .iter()
                            .find(|l| !l.trim().is_empty())
                            .cloned()
                            .unwrap_or_default();
                        self.say(Level::Bad, &format!("Das hat nicht geklappt: {first}"));
                    }
                    return vec![Effect::Refresh];
                }
                _ => {}
            },
            Event::Returned { code } => match awaiting {
                Await::Connect => {
                    let ok = code == 0;
                    if let Some(w) = self.wizard() {
                        if ok {
                            w.signed_in = true;
                            w.message = None;
                            w.step = w.next_step();
                            w.cursor = 0;
                            if w.step == Step::Done {
                                w.notes.push("Die Station gibt es bisher nur für Claude Code; für diesen Agenten kommt sie noch.".into());
                            }
                        } else {
                            w.message = Some((
                                Level::Bad,
                                format!(
                                    "Die Anmeldung wurde nicht abgeschlossen (Code {code}). Enter versucht es noch einmal."
                                ),
                            ));
                        }
                    }
                    return vec![Effect::Refresh];
                }
                Await::Test => {
                    if let Some(w) = self.wizard() {
                        if code == 0 {
                            w.test_ok = true;
                            w.message = None;
                            w.step = w.next_step();
                            w.cursor = 0;
                        } else {
                            w.message = Some((
                                Level::Bad,
                                format!(
                                    "Der Test meldet ein Problem (Code {code}). Enter wiederholt ihn, s überspringt ihn, Esc geht zurück."
                                ),
                            ));
                        }
                    }
                    return vec![Effect::Refresh];
                }
                Await::Foreground => {
                    self.say(
                        if code == 0 { Level::Ok } else { Level::Warn },
                        "Die Station ist beendet.",
                    );
                    return vec![Effect::Refresh];
                }
                Await::Disconnect(agent) => {
                    if code == 0 {
                        // what is left of the agent here goes after the sign-off in Wave
                        self.awaiting = Await::Told(format!("{agent} ist getrennt."));
                        self.busy = Some("Einen Moment …".into());
                        return vec![Effect::Forget { agent }];
                    }
                    self.say(
                        Level::Bad,
                        "Das Trennen ist nicht gelungen; es wurde nichts weiter gelöscht.",
                    );
                    return vec![Effect::Refresh];
                }
                _ => {}
            },
            Event::Rooms(r) => {
                if let Some(w) = self.wizard() {
                    match r {
                        Ok(rooms) if rooms.is_empty() => {
                            w.message = Some((Level::Warn, "Der Agent darf noch keinen Raum lesen. Gib ihm in Wave Zugriff auf Räume und drücke dann Enter.".into()));
                        }
                        Ok(rooms) => {
                            w.rooms = rooms
                                .into_iter()
                                .map(|r| RoomChoice {
                                    id: r.id,
                                    name: r.name,
                                    on: false,
                                })
                                .collect();
                            w.rooms_loaded = true;
                            w.cursor = 0;
                            w.message = None;
                        }
                        Err(e) => {
                            w.message =
                                Some((Level::Bad, format!("{e} Enter versucht es noch einmal.")))
                        }
                    }
                }
            }
            Event::Done(r) => match awaiting {
                Await::Install => {
                    if let Some(w) = self.wizard() {
                        match r {
                            Ok(notes) => {
                                w.notes.extend(notes);
                                w.message = None;
                                w.step = Step::Done;
                            }
                            Err(e) => w.message = Some((Level::Bad, e)),
                        }
                    }
                    return vec![Effect::Refresh];
                }
                Await::Told(msg) => {
                    match r {
                        Ok(notes) => {
                            let extra = notes.first().cloned().unwrap_or_default();
                            self.say(
                                Level::Ok,
                                &if extra.is_empty() {
                                    msg
                                } else {
                                    format!("{msg} {extra}")
                                },
                            );
                        }
                        Err(e) => self.say(Level::Bad, &e),
                    }
                    return vec![Effect::Refresh];
                }
                Await::SaveForPreview(p) => match r {
                    Ok(_) => {
                        self.awaiting = Await::Preview;
                        self.busy = Some("Vorschau wird erstellt …".into());
                        return vec![Effect::Capture {
                            title: "Vorschau".into(),
                            args: with_agent(station_command(
                                &p,
                                self.snap.claude.as_deref(),
                                &["--dry-run"],
                            )),
                        }];
                    }
                    Err(e) => {
                        if let Some(w) = self.wizard() {
                            w.message = Some((Level::Bad, e));
                        }
                    }
                },
                _ => {
                    if let Err(e) = r {
                        self.say(Level::Bad, &e);
                    }
                    return vec![Effect::Refresh];
                }
            },
        }
        vec![]
    }

    fn wizard(&mut self) -> Option<&mut Wizard> {
        match self.stack.last_mut() {
            Some(Screen::Wizard(w)) => Some(w),
            _ => None,
        }
    }

    /// After the agents changed: cursors and open agent pages must point at something that exists.
    fn fix_cursors(&mut self) {
        let n = self.snap.agents.len() + MAIN_EXTRAS;
        let snap = self.snap.clone();
        self.stack.retain(|s| match s {
            Screen::Agent { name, .. } => snap.agent(name).is_some(),
            _ => true,
        });
        for s in &mut self.stack {
            match s {
                Screen::Main { cursor } => *cursor = (*cursor).min(n - 1),
                Screen::Agent { name, cursor } => {
                    if let Some(row) = snap.agent(name) {
                        let len = agent_actions(row, snap.os).len();
                        *cursor = (*cursor).min(len - 1);
                    }
                }
                _ => {}
            }
        }
    }
}

/// `agent ...` goes in front of `station ...` words the model builds.
fn with_agent(mut station: Vec<String>) -> Vec<String> {
    station.insert(0, "agent".into());
    station
}

fn wizard_has_text(w: &Wizard) -> bool {
    matches!(
        w.step,
        Step::Name | Step::WaveUrl | Step::KeySource | Step::Workdir
    )
}

/// The engines the wizard lists, in this order.
pub fn engine_rows() -> Vec<Engine> {
    vec![Engine::ClaudeCode, Engine::Codex, Engine::Grok]
}
