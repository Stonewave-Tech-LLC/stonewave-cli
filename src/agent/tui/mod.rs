//! The full-screen menu: `stonewave` on a terminal.
//!
//! The banner with its wave, then arrow keys, Enter, Esc and q. The menu lists the agents on this computer and walks through connecting one
//! (and, for Claude Code, setting up its station) in a few steps, each saying what it will do before it does it.
//!
//! It only drives commands that exist: `stonewave agent connect`, `agent station run|status|pause|resume`, `agent disconnect` (started as
//! the same program, so every check they make stays where it is) and, for the service, a user unit written next to your other user units.
//! Without a terminal, with `--plain` or on a terminal that is too small or `dumb`, the plain menu of [`super::menu`] is used instead.
//!
//! - [`model`]: keys and results in, effects out (no terminal, no disk)
//! - [`view`]: the lines of each screen
//! - [`profile`], [`service`]: what the wizard remembers, and the user service
//! - this file: the loop, the terminal, and the real machine behind the effects

use std::io::{self, Write};
use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::{Duration, Instant};

use crossterm::cursor::{Hide, Show};
use crossterm::event::{self, Event as TermEvent, KeyCode, KeyEventKind, KeyModifiers};
use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};

use super::dirs::Dirs;
use super::exec::{Exec, System};
use super::key::scrub;
use super::state;
use super::station_run::{self, KeySource};
use crate::connect::{Account, Curl};
use crate::style::ColorMode;

pub mod model;
pub mod profile;
pub mod service;
pub mod view;

#[cfg(test)]
mod tests;

use model::{App, Effect, Event, Key, KeyState, Room, Snapshot};
use profile::Profile;

// ------------------------------------------------------------------ the machine behind the effects

/// Everything the menu does on the machine. Tests replace it.
pub trait World {
    fn snapshot(&mut self) -> Snapshot;
    /// Runs `stonewave <args>` and collects what it writes.
    fn capture(&mut self, args: &[String]) -> (i32, Vec<String>);
    /// Runs `stonewave <args>` with the terminal. The caller has left the full screen.
    fn interactive(&mut self, args: &[String]) -> i32;
    fn rooms(
        &mut self,
        agent: &str,
        key_env: Option<&str>,
        wave_url: &str,
    ) -> Result<Vec<Room>, String>;
    fn save_profile(&mut self, p: &Profile) -> Result<(), String>;
    fn install_service(&mut self, p: &Profile) -> Result<Vec<String>, String>;
    fn service_control(&mut self, agent: &str, what: &str) -> Result<(), String>;
    fn remove_service(&mut self, agent: &str) -> Result<(), String>;
    fn forget(&mut self, agent: &str) -> Result<(), String>;
}

/// The terminal. Tests replace it with a script of keys.
pub trait Console {
    fn size(&self) -> (usize, usize);
    fn draw(&mut self, lines: &[String]);
    /// The next key, or none when `wait` has passed. A resize returns `Some(None)`.
    fn next(&mut self, wait: Duration) -> Option<Option<Key>>;
    /// Gives the terminal back to a command: leaves the full screen.
    fn suspend(&mut self);
    /// After the command: waits for Enter, then takes the screen again.
    fn resume(&mut self);
}

const FRAME: Duration = Duration::from_millis(50);
const INTRO: Duration = Duration::from_millis(1250);
/// How long the menu waits for a key before it looks at the machine again.
const IDLE_REFRESH: Duration = Duration::from_secs(5);

/// Runs the effects one after the other. What they report goes back into the model, which may ask for more.
fn run_effects(
    app: &mut App,
    world: &mut dyn World,
    con: &mut dyn Console,
    mode: ColorMode,
    started: Instant,
    first: Vec<Effect>,
) {
    let mut queue: std::collections::VecDeque<Effect> = first.into();
    while let Some(fx) = queue.pop_front() {
        // the busy line is drawn before the work starts
        if app.busy.is_some() {
            draw(app, con, mode, started);
        }
        let ev = match fx {
            Effect::Refresh => Event::Snapshot(Box::new(world.snapshot())),
            Effect::Capture { args, .. } => {
                let (code, lines) = world.capture(&args);
                Event::Output { code, lines }
            }
            Effect::Interactive { args } => {
                con.suspend();
                let code = world.interactive(&args);
                con.resume();
                Event::Returned { code }
            }
            Effect::FetchRooms {
                agent,
                key_env,
                wave_url,
            } => Event::Rooms(world.rooms(&agent, key_env.as_deref(), &wave_url)),
            Effect::SaveProfile(p) => Event::Done(world.save_profile(&p).map(|()| Vec::new())),
            Effect::InstallService(p) => Event::Done(world.install_service(&p)),
            Effect::ServiceControl { agent, what } => {
                Event::Done(world.service_control(&agent, what).map(|()| Vec::new()))
            }
            Effect::RemoveService { agent } => {
                Event::Done(world.remove_service(&agent).map(|()| Vec::new()))
            }
            Effect::Forget { agent } => Event::Done(world.forget(&agent).map(|()| Vec::new())),
        };
        queue.extend(app.on_event(ev));
    }
}

fn draw(app: &App, con: &mut dyn Console, mode: ColorMode, started: Instant) {
    let (w, h) = con.size();
    let lines = view::render(app, w, h, mode, started.elapsed().as_secs_f32());
    con.draw(&lines);
}

/// The loop: draw, wait for a key or the next frame, act.
pub fn run_loop(
    app: &mut App,
    world: &mut dyn World,
    con: &mut dyn Console,
    mode: ColorMode,
    animate: bool,
) {
    let started = Instant::now();
    let mut intro = animate.then_some(started);
    while !app.quit {
        app.crest = intro.map(|t0| {
            let t = (t0.elapsed().as_secs_f32() / INTRO.as_secs_f32()).min(1.0);
            let eased = 1.0 - (1.0 - t) * (1.0 - t);
            eased * (crate::banner::WIDTH as f32 + 10.0) - 2.0
        });
        if intro.is_some_and(|t0| t0.elapsed() >= INTRO) {
            intro = None;
            app.crest = None;
        }
        draw(app, con, mode, started);
        let wait = if intro.is_some() || app.busy.is_some() {
            FRAME
        } else {
            IDLE_REFRESH
        };
        match con.next(wait) {
            Some(Some(key)) => {
                // any key ends the banner's wave at once
                intro = None;
                let fx = app.on_key(key);
                run_effects(app, world, con, mode, started, fx);
            }
            // nothing was pressed for a while: a station may have started or stopped meanwhile, so the lists are read again
            None if intro.is_none() && app.busy.is_none() && app.shows_agents() => {
                run_effects(app, world, con, mode, started, vec![Effect::Refresh]);
            }
            Some(None) | None => {}
        }
    }
}

// ------------------------------------------------------------------ the real terminal

/// Whether the full screen is taken (so a panic or a signal knows there is something to give back).
static SCREEN_ON: AtomicBool = AtomicBool::new(false);
/// SIGTERM or SIGHUP arrived: the loop ends and the terminal is restored by the normal way out.
static GOT_SIGNAL: AtomicBool = AtomicBool::new(false);

/// Gives the terminal back: normal screen, cursor, cooked mode. Safe to call twice and from a panic hook.
fn restore_terminal() {
    if SCREEN_ON.swap(false, Ordering::SeqCst) {
        let mut out = io::stdout();
        let _ = crossterm::execute!(out, Show, LeaveAlternateScreen);
        let _ = terminal::disable_raw_mode();
    }
}

#[cfg(unix)]
extern "C" fn on_signal(_: i32) {
    GOT_SIGNAL.store(true, Ordering::SeqCst);
}

/// While the full screen is up, SIGTERM and SIGHUP end the menu through its normal way out (a terminal that is closed or a `kill` would
/// otherwise leave it in raw mode on the alternate screen).
fn catch_signals(on: bool) {
    #[cfg(unix)]
    // SAFETY: the handler only stores into an atomic; `signal` is given a valid handler or the default action.
    unsafe {
        let h = if on {
            on_signal as extern "C" fn(i32) as usize
        } else {
            libc::SIG_DFL
        };
        libc::signal(libc::SIGTERM, h);
        libc::signal(libc::SIGHUP, h);
    }
    #[cfg(not(unix))]
    let _ = on;
}

struct Screen {
    out: io::Stdout,
    last: Vec<String>,
    size: (u16, u16),
    active: bool,
}

impl Screen {
    fn enter() -> io::Result<Screen> {
        terminal::enable_raw_mode()?;
        let mut out = io::stdout();
        if let Err(e) = crossterm::execute!(out, EnterAlternateScreen, Hide) {
            let _ = terminal::disable_raw_mode();
            return Err(e);
        }
        SCREEN_ON.store(true, Ordering::SeqCst);
        catch_signals(true);
        Ok(Screen {
            out,
            last: Vec::new(),
            size: terminal::size().unwrap_or((80, 24)),
            active: true,
        })
    }

    fn leave(&mut self) {
        if self.active {
            restore_terminal();
            catch_signals(false);
            self.active = false;
        }
    }
}

impl Drop for Screen {
    fn drop(&mut self) {
        self.leave();
    }
}

fn map_key(k: crossterm::event::KeyEvent) -> Option<Key> {
    if k.kind == KeyEventKind::Release {
        return None;
    }
    let ctrl = k.modifiers.contains(KeyModifiers::CONTROL);
    let alt = k.modifiers.contains(KeyModifiers::ALT);
    Some(match k.code {
        KeyCode::Char('c' | 'C') if ctrl => Key::CtrlC,
        KeyCode::Char('d' | 'D') if ctrl => Key::CtrlC,
        KeyCode::Char('u' | 'U') if ctrl => Key::CtrlU,
        KeyCode::Char(_) if ctrl || alt => return None,
        KeyCode::Char(c) => Key::Char(c),
        KeyCode::Up => Key::Up,
        KeyCode::Down => Key::Down,
        KeyCode::Left => Key::Left,
        KeyCode::Right => Key::Right,
        KeyCode::Home => Key::Home,
        KeyCode::End => Key::End,
        KeyCode::PageUp => Key::PageUp,
        KeyCode::PageDown => Key::PageDown,
        KeyCode::Enter => Key::Enter,
        KeyCode::Esc => Key::Esc,
        KeyCode::Tab => Key::Tab,
        KeyCode::BackTab => Key::BackTab,
        KeyCode::Backspace => Key::Backspace,
        KeyCode::Delete => Key::Delete,
        _ => return None,
    })
}

impl Console for Screen {
    fn size(&self) -> (usize, usize) {
        (usize::from(self.size.0), usize::from(self.size.1))
    }

    fn draw(&mut self, lines: &[String]) {
        if !self.active {
            return;
        }
        let mut buf = String::new();
        let resized = self.last.len() != lines.len();
        for (i, l) in lines.iter().enumerate() {
            if !resized && self.last.get(i) == Some(l) {
                continue;
            }
            buf.push_str(&format!("\x1b[{};1H{l}\x1b[0m\x1b[K", i + 1));
        }
        if resized {
            buf.push_str(&format!("\x1b[{};1H\x1b[J", lines.len() + 1));
        }
        if !buf.is_empty() {
            let _ = self.out.write_all(buf.as_bytes());
            let _ = self.out.flush();
        }
        self.last = lines.to_vec();
    }

    fn next(&mut self, wait: Duration) -> Option<Option<Key>> {
        // the terminal could not be taken back after a command, or it is gone: there is nothing left to show, so the menu ends
        if !self.active {
            return Some(Some(Key::CtrlC));
        }
        // waited for in short slices, so a signal is noticed within a fifth of a second
        let deadline = Instant::now() + wait;
        loop {
            if GOT_SIGNAL.load(Ordering::SeqCst) {
                return Some(Some(Key::CtrlC));
            }
            let left = deadline.saturating_duration_since(Instant::now());
            let slice = left.min(Duration::from_millis(200));
            match event::poll(slice) {
                Ok(true) => break,
                Ok(false) if left <= slice => return None,
                Ok(false) => {}
                Err(_) => return Some(Some(Key::CtrlC)),
            }
        }
        match event::read() {
            Ok(TermEvent::Key(k)) => map_key(k).map(Some).or(Some(None)),
            Ok(TermEvent::Resize(w, h)) => {
                self.size = (w, h);
                // the whole screen is drawn again
                self.last.clear();
                let _ = write!(self.out, "\x1b[2J");
                Some(None)
            }
            _ => Some(None),
        }
    }

    fn suspend(&mut self) {
        self.leave();
    }

    fn resume(&mut self) {
        // wait for Enter in the normal screen, so what the command printed can be read
        {
            let mut out = io::stdout();
            let _ = write!(out, "\r\n  Enter drücken, um zum Menü zurückzukehren … ");
            let _ = out.flush();
            let mut line = String::new();
            let _ = io::stdin().read_line(&mut line);
        }
        if terminal::enable_raw_mode().is_ok()
            && crossterm::execute!(self.out, EnterAlternateScreen, Hide).is_ok()
        {
            self.active = true;
            SCREEN_ON.store(true, Ordering::SeqCst);
            catch_signals(true);
            self.size = terminal::size().unwrap_or(self.size);
            self.last.clear();
            let _ = write!(self.out, "\x1b[2J");
        }
    }
}

// ------------------------------------------------------------------ the real machine

pub struct Real {
    dirs: Dirs,
    exe: PathBuf,
    /// Which coding agents are installed does not change while the menu is open: asked once.
    engines: Option<Vec<model::EngineRow>>,
}

impl Real {
    fn command(&self, args: &[String]) -> std::process::Command {
        let mut c = std::process::Command::new(&self.exe);
        c.args(args);
        c
    }
}

/// While a command has the terminal, Ctrl-C is for it, not for the menu waiting behind it.
struct CtrlCForChild;

#[cfg(unix)]
extern "C" fn ignore_signal(_: i32) {}

impl CtrlCForChild {
    fn new() -> CtrlCForChild {
        #[cfg(unix)]
        // SAFETY: the handler does nothing; the child starts with the default action again (a handler is reset by exec).
        unsafe {
            libc::signal(libc::SIGINT, ignore_signal as extern "C" fn(i32) as usize);
        }
        #[cfg(windows)]
        // SAFETY: a console handler that swallows Ctrl-C for this process; children do not inherit a handler function.
        unsafe {
            SetConsoleCtrlHandler(Some(swallow_ctrl_c), 1);
        }
        CtrlCForChild
    }
}

impl Drop for CtrlCForChild {
    fn drop(&mut self) {
        #[cfg(unix)]
        // SAFETY: restoring the default action.
        unsafe {
            libc::signal(libc::SIGINT, libc::SIG_DFL);
        }
        #[cfg(windows)]
        // SAFETY: removes the handler added in `new`.
        unsafe {
            SetConsoleCtrlHandler(Some(swallow_ctrl_c), 0);
        }
    }
}

#[cfg(windows)]
unsafe extern "system" {
    fn SetConsoleCtrlHandler(
        handler: Option<unsafe extern "system" fn(u32) -> i32>,
        add: i32,
    ) -> i32;
}

#[cfg(windows)]
unsafe extern "system" fn swallow_ctrl_c(kind: u32) -> i32 {
    // CTRL_C_EVENT = 0: handled here; other events (close, break) keep their normal course
    i32::from(kind == 0)
}

fn tidy_lines(text: &str) -> Vec<String> {
    scrub(text)
        .lines()
        .map(|l| {
            l.chars()
                .filter(|c| !super::key::is_invisible(*c) || *c == '\t')
                .collect::<String>()
        })
        .collect()
}

impl World for Real {
    fn snapshot(&mut self) -> Snapshot {
        let engines = self.engines.get_or_insert_with(|| engines(&System)).clone();
        snapshot(&self.dirs, &System, &self.exe, engines)
    }

    fn capture(&mut self, args: &[String]) -> (i32, Vec<String>) {
        match self
            .command(args)
            .stdin(std::process::Stdio::null())
            .output()
        {
            Ok(o) => {
                let mut text = String::from_utf8_lossy(&o.stdout).into_owned();
                text.push_str(&String::from_utf8_lossy(&o.stderr));
                (o.status.code().unwrap_or(1), tidy_lines(&text))
            }
            Err(e) => (
                1,
                vec![format!(
                    "Das Programm ließ sich nicht starten: {}",
                    e.kind()
                )],
            ),
        }
    }

    fn interactive(&mut self, args: &[String]) -> i32 {
        let _guard = CtrlCForChild::new();
        match self.command(args).status() {
            Ok(s) => s.code().unwrap_or(1),
            Err(e) => {
                eprintln!("stonewave: cannot start the command: {}", e.kind());
                1
            }
        }
    }

    fn rooms(
        &mut self,
        agent: &str,
        key_env: Option<&str>,
        wave_url: &str,
    ) -> Result<Vec<Room>, String> {
        let src = match key_env {
            Some(n) => KeySource::Env(n.to_owned()),
            None => KeySource::File(self.dirs.key_file(agent)),
        };
        if let Some(n) = key_env {
            if !model::real_env_probe(n) {
                return Err(format!(
                    "Die Variable {n} ist in dieser Shell nicht gesetzt. Beende das Menü und starte es durch den Befehl deines Secret-Stores."
                ));
            }
        }
        let key = station_run::load_key(&src, &|n| std::env::var(n).ok())
            .map_err(|e| format!("Der Schlüssel ist nicht lesbar: {}", scrub(&e)))?;
        let acct = Account::parse_as("Wave", wave_url)
            .map_err(|e| format!("Diese Wave-Adresse geht nicht: {e}"))?;
        let mcp = super::wave::mcp_address(&acct, None).map_err(|e| e.to_string())?;
        let http = Curl {
            allow_plain_http: !acct.is_https(),
        };
        station_run::list_rooms(&http, &mcp, &key)
            .map(|v| v.into_iter().map(|(id, name)| Room { id, name }).collect())
            .map_err(|e| match e {
                station_run::WaveErr::Key(_) => "Wave nimmt den Schlüssel nicht an.".to_owned(),
                station_run::WaveErr::Later(m, _) => {
                    format!("Wave ist gerade nicht erreichbar: {}.", scrub(&m))
                }
                station_run::WaveErr::Other(m) => {
                    format!("Wave hat unerwartet geantwortet: {}.", scrub(&m))
                }
            })
    }

    fn save_profile(&mut self, p: &Profile) -> Result<(), String> {
        p.save(&self.dirs)
            .map_err(|e| format!("Die Einrichtung ließ sich nicht speichern: {}", e.kind()))
    }

    fn install_service(&mut self, p: &Profile) -> Result<Vec<String>, String> {
        let claude = System.which("claude").map(|c| super::dirs::text(&c));
        let path = std::env::var("PATH").ok();
        service::install(
            &self.dirs,
            &System,
            p,
            &super::dirs::text(&self.exe),
            claude.as_deref(),
            path.as_deref(),
        )
    }

    fn service_control(&mut self, agent: &str, what: &str) -> Result<(), String> {
        service::control(&self.dirs, &System, agent, what)
    }

    fn remove_service(&mut self, agent: &str) -> Result<(), String> {
        service::remove(&self.dirs, &System, agent)
    }

    fn forget(&mut self, agent: &str) -> Result<(), String> {
        service::remove(&self.dirs, &System, agent)?;
        Profile::remove(&self.dirs, agent);
        Ok(())
    }
}

/// The coding agents that are installed here, with their versions.
pub fn engines(exec: &dyn Exec) -> Vec<model::EngineRow> {
    super::menu::detect(exec)
        .into_iter()
        .map(|r| model::EngineRow {
            engine: r.engine,
            found: r.program.is_some(),
            version: r.version,
        })
        .collect()
}

/// What the menu shows, read from the machine.
pub fn snapshot(
    dirs: &Dirs,
    exec: &dyn Exec,
    exe: &std::path::Path,
    engines: Vec<model::EngineRow>,
) -> Snapshot {
    let profiles = profile::list(dirs);
    let mut names: Vec<String> = state::list(&dirs.config)
        .into_iter()
        .map(|(n, _)| n)
        .collect();
    names.extend(profiles.iter().map(|p| p.agent.clone()));
    names.sort();
    names.dedup();
    let agents: Vec<model::AgentRow> = names
        .into_iter()
        .map(|name| {
            let st = state::State::load(&dirs.state_file(&name)).ok();
            let prof = profiles.iter().find(|p| p.agent == name).cloned();
            let paths = station_run::paths(dirs, &name);
            let key = match prof.as_ref().and_then(|p| p.key_env.clone()) {
                Some(n) => KeyState::Env {
                    set_here: model::real_env_probe(&n),
                    name: n,
                },
                None if dirs.key_file(&name).exists() => KeyState::File,
                None => KeyState::FileMissing,
            };
            model::AgentRow {
                engine: st
                    .as_ref()
                    .map(|s| scrub(&s.engine))
                    .or_else(|| prof.as_ref().map(|p| p.engine.clone()))
                    .unwrap_or_default(),
                key,
                connected: st.is_some(),
                running: station_run::station_running(&paths.lock),
                paused: paths.paused.exists(),
                service: service::state(dirs, exec, &name),
                wave_url: st.as_ref().map(|s| s.wave_url.clone()),
                profile: prof,
                name,
            }
        })
        .collect();
    let wave_hint = std::env::var("STONEWAVE_WAVE_URL")
        .ok()
        .filter(|v| !v.is_empty())
        .or_else(|| {
            agents.iter().find_map(|a| {
                a.wave_url
                    .clone()
                    .or_else(|| a.profile.as_ref().map(|p| p.wave_url.clone()))
            })
        });
    Snapshot {
        engines,
        agents,
        root: super::running_as_root(),
        os: dirs.os,
        exe: super::dirs::text(exe),
        home: super::dirs::text(&dirs.home),
        config_dir: super::dirs::text(&dirs.config),
        state_dir: super::dirs::text(&dirs.state),
        wave_hint,
        claude: exec.which("claude").map(|c| super::dirs::text(&c)),
        path_env: std::env::var("PATH").ok(),
        host: crate::flow::Host::detect(None, None, None).name,
    }
}

/// Whether the full-screen menu can run here.
fn suitable() -> Result<(), &'static str> {
    if std::env::var("TERM").is_ok_and(|t| t.eq_ignore_ascii_case("dumb")) {
        return Err("TERM=dumb");
    }
    match terminal::size() {
        Ok((w, h)) if view::too_small(usize::from(w), usize::from(h)) => {
            Err("the terminal is too small")
        }
        Ok(_) => Ok(()),
        Err(_) => Err("no terminal size"),
    }
}

/// `stonewave` with no words on a terminal. Falls back to the plain menu where the full screen cannot be used.
pub fn main() -> ExitCode {
    if let Err(why) = suitable() {
        eprintln!(
            "stonewave: the full-screen menu is not used here ({why}); showing the plain menu. `stonewave --plain` always shows it."
        );
        return super::menu::main(true);
    }
    let dirs = match Dirs::system() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("stonewave: {e}");
            return ExitCode::from(2);
        }
    };
    let exe = std::env::current_exe()
        .ok()
        .and_then(|p| std::fs::canonicalize(p).ok());
    let Some(exe) = exe else {
        eprintln!("stonewave: cannot find this program's own path");
        return ExitCode::FAILURE;
    };
    let mut world = Real {
        dirs,
        exe,
        engines: None,
    };
    let mut app = App::new(world.snapshot());
    let mode = color_mode();
    let mut screen = match Screen::enter() {
        Ok(s) => s,
        Err(e) => {
            eprintln!("stonewave: cannot take over the terminal: {}", e.kind());
            return super::menu::main(true);
        }
    };
    // a panic gives the terminal back BEFORE its message is printed, so the message can be read
    let previous = std::panic::take_hook();
    std::panic::set_hook(Box::new(move |info| {
        restore_terminal();
        previous(info);
    }));
    let animate = mode != ColorMode::None
        && !std::env::var_os("STONEWAVE_NO_ANIM").is_some_and(|v| !v.is_empty());
    run_loop(&mut app, &mut world, &mut screen, mode, animate);
    drop(screen);
    ExitCode::SUCCESS
}

fn color_mode() -> ColorMode {
    let set = |n: &str| std::env::var_os(n).is_some_and(|v| !v.is_empty());
    let is = |n: &str, v: &str| std::env::var(n).is_ok_and(|x| x.eq_ignore_ascii_case(v));
    if set("NO_COLOR") {
        ColorMode::None
    } else if is("COLORTERM", "truecolor") || is("COLORTERM", "24bit") || set("WT_SESSION") {
        ColorMode::TrueColor
    } else {
        ColorMode::Ansi256
    }
}
