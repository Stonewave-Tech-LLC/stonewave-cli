//! `stonewave agent station ...`: the station that wakes a coding agent by continuing its session.
//!
//! ```text
//! stonewave agent station run [--agent <name>] [--workdir <folder>] [--rooms "A,B"] [--key-env NAME] [--wave <url>] [--dry-run] [--once]
//! stonewave agent station pause | resume | status [--agent <name>]
//! ```

use std::path::PathBuf;
use std::process::ExitCode;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::dirs::Dirs;
use super::exec::{Exec, System};
use super::key::scrub;
use super::spawn::{RealSpawn, Spawn};
use super::station_run::{self, Config, Deps, KeySource, Report, Station};
use crate::connect::{Account, Curl, Http};

pub const HELP: &str = "\
Usage: stonewave agent station <command> [options]

The station keeps one long session per room for an agent and continues it when a person of the company mentions the agent in Wave.
It starts Claude Code with no command tools and one MCP server (Wave), in the agent's own working folder, with limits for every wake.

Commands:
  run        Watch Wave's inbox and wake the agent (stays running; Ctrl-C stops it)
  pause      Stop starting new wakes (the station keeps running); `resume` ends the pause
  resume     End a pause
  status     Show the sessions, the wakes of the last day and whether it is paused

Options for run:
  --agent <name>          The agent (default: the only one connected here)
  --wave <url>            Address of Wave's API (or STONEWAVE_WAVE_URL; else the one `agent connect` saved)
  --key-env <NAME>        Take the agent's key from this environment variable (for example from a secret-store command), never from a file
  --workdir <folder>      The agent's own folder, the only place it can read and write (default ~/wave-agents/<agent>)
  --rooms \"A,B\"           The rooms to serve (required; names, short names or ids; the names become ids once, at the start)
  --poll-secs <n>         Seconds between looks at the inbox (default 15)
  --timeout-min <n>       Minutes one wake may take (default 20)
  --max-turns <n>         Turns one wake may take (default 40)
  --max-budget-usd <x>    Cost cap of one wake in dollars, an estimate by Claude Code; `none` for no cap (default 2)
  --per-hour <n>          Wakes per hour at most (default 10)
  --per-day <n>           Wakes per day at most (default 50)
  --session-max-wakes <n> End a room's session after this many wakes, with a handover note (default 200)
  --claude <path>         Claude Code program (default: `claude` on the PATH)
  --model <name>          Model for the wakes (default: Claude Code's own)
  --dry-run               Show the exact command of a wake and everything around it; start nothing, ask nobody
  --once                  Look at the inbox once and stop

Wakes only come from people of the company, never from agents or text of outside origin.
";

#[derive(Debug, Default)]
pub struct Args {
    pub sub: String,
    pub agent: Option<String>,
    pub wave: Option<String>,
    pub key_env: Option<String>,
    pub workdir: Option<PathBuf>,
    pub rooms: Vec<String>,
    pub poll_secs: Option<u64>,
    pub timeout_min: Option<u64>,
    pub max_turns: Option<u32>,
    pub budget: Option<Option<f64>>,
    pub per_hour: Option<u32>,
    pub per_day: Option<u32>,
    pub session_max_wakes: Option<u32>,
    pub claude: Option<PathBuf>,
    pub model: Option<String>,
    pub dry_run: bool,
    pub once: bool,
}

pub(crate) fn env_name_ok(s: &str) -> bool {
    (1..=100).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_')
        && !s.starts_with(|c: char| c.is_ascii_digit())
}

pub fn parse(raw: &[String]) -> Result<Args, String> {
    let mut a = Args::default();
    let mut it = raw.iter().cloned();
    while let Some(arg) = it.next() {
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .filter(|v| !v.is_empty())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        let num = |name: &str, v: String| {
            v.parse::<u64>()
                .ok()
                .filter(|n| *n >= 1)
                .ok_or_else(|| format!("{name}: a whole number of at least 1"))
        };
        match flag.as_str() {
            "-h" | "--help" | "help" => {
                a.sub = "help".into();
                return Ok(a);
            }
            "--agent" => {
                let v = value("--agent")?;
                if !crate::connect::is_agent_slug(&v) {
                    return Err(
                        "--agent: an agent name has lower case letters, digits and dashes".into(),
                    );
                }
                a.agent = Some(v);
            }
            "--wave" => a.wave = Some(value("--wave")?),
            "--key-env" => {
                let v = value("--key-env")?;
                if !env_name_ok(&v) {
                    return Err("--key-env: the NAME of an environment variable (capital letters, digits, underscores), not the key".into());
                }
                a.key_env = Some(v);
            }
            "--workdir" => a.workdir = Some(PathBuf::from(value("--workdir")?)),
            "--rooms" => {
                a.rooms = value("--rooms")?
                    .split(',')
                    .map(|s| s.trim().to_owned())
                    .filter(|s| !s.is_empty())
                    .collect();
                if a.rooms.is_empty()
                    || a.rooms
                        .iter()
                        .any(|r| r.chars().count() > 80 || r.chars().any(char::is_control))
                {
                    return Err("--rooms: room names separated by commas".into());
                }
            }
            "--poll-secs" => {
                a.poll_secs = Some(num("--poll-secs", value("--poll-secs")?)?.clamp(5, 3600))
            }
            "--timeout-min" => {
                a.timeout_min = Some(num("--timeout-min", value("--timeout-min")?)?.min(240))
            }
            "--max-turns" => {
                a.max_turns = Some(num("--max-turns", value("--max-turns")?)?.min(500) as u32)
            }
            "--max-budget-usd" => {
                let v = value("--max-budget-usd")?;
                a.budget = Some(if v == "none" {
                    None
                } else {
                    Some(
                        v.parse::<f64>()
                            .ok()
                            .filter(|x| x.is_finite() && *x > 0.0 && *x <= 1000.0)
                            .ok_or("--max-budget-usd: a number above 0 (or `none`)")?,
                    )
                });
            }
            "--per-hour" => {
                a.per_hour = Some(num("--per-hour", value("--per-hour")?)?.min(10_000) as u32)
            }
            "--per-day" => {
                a.per_day = Some(num("--per-day", value("--per-day")?)?.min(100_000) as u32)
            }
            "--session-max-wakes" => {
                a.session_max_wakes = Some(
                    num("--session-max-wakes", value("--session-max-wakes")?)?.min(100_000) as u32,
                )
            }
            "--claude" => a.claude = Some(PathBuf::from(value("--claude")?)),
            "--model" => {
                let v = value("--model")?;
                if v.len() > 80
                    || !v
                        .chars()
                        .all(|c| c.is_ascii_alphanumeric() || "-_.:[]".contains(c))
                {
                    return Err("--model: a model name".into());
                }
                a.model = Some(v);
            }
            "--dry-run" => a.dry_run = true,
            "--once" => a.once = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            word if a.sub.is_empty() => {
                a.sub = match word {
                    "run" | "pause" | "resume" | "status" => word.to_owned(),
                    other => return Err(format!("unknown command {other}")),
                }
            }
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    if a.sub.is_empty() {
        a.sub = "help".into();
    }
    if a.sub != "run"
        && (a.dry_run
            || a.once
            || a.wave.is_some()
            || a.key_env.is_some()
            || a.workdir.is_some()
            || !a.rooms.is_empty()
            || a.claude.is_some())
    {
        return Err(format!("these options belong to `run`, not to `{}`", a.sub));
    }
    Ok(a)
}

static STOP: AtomicBool = AtomicBool::new(false);

#[cfg(unix)]
extern "C" fn on_signal(_: i32) {
    STOP.store(true, Ordering::SeqCst);
}

/// While the station runs, Ctrl-C and SIGTERM only ask it to stop (a wake that is going is ended too); afterwards the signals work as before,
/// so the menu can be left with Ctrl-C again.
struct Signals;

impl Signals {
    fn install() -> Signals {
        #[cfg(unix)]
        {
            // SAFETY: the handler only stores into an atomic.
            unsafe {
                libc::signal(libc::SIGINT, on_signal as extern "C" fn(i32) as usize);
                libc::signal(libc::SIGTERM, on_signal as extern "C" fn(i32) as usize);
            }
        }
        Signals
    }
}

impl Drop for Signals {
    fn drop(&mut self) {
        #[cfg(unix)]
        {
            // SAFETY: restoring the default action.
            unsafe {
                libc::signal(libc::SIGINT, libc::SIG_DFL);
                libc::signal(libc::SIGTERM, libc::SIG_DFL);
            }
        }
    }
}

fn say(line: &str) {
    eprintln!("station: {}", scrub(line));
}

fn short(id: &str) -> String {
    id.chars().take(8).collect()
}

fn print_report(rep: &Report, last_wait: &mut Option<String>) {
    for n in &rep.notes {
        say(n);
    }
    for (room, how) in &rep.woke {
        say(&format!("woke the session of room {} ({how})", short(room)));
    }
    if !rep.skipped.is_empty() && rep.waiting.is_none() {
        let mut counts: Vec<(&str, usize)> = Vec::new();
        for s in &rep.skipped {
            match counts.iter_mut().find(|(k, _)| k == s) {
                Some(c) => c.1 += 1,
                None => counts.push((s, 1)),
            }
        }
        for (why, n) in counts {
            say(&format!("did not wake for {n} item(s): {why}"));
        }
    }
    if rep.waiting != *last_wait {
        if let Some(w) = &rep.waiting {
            say(&format!("waiting: {w}"));
        } else if last_wait.is_some() {
            say("running again");
        }
        *last_wait = rep.waiting.clone();
    }
}

pub fn main(raw: &[String]) -> ExitCode {
    let args = match parse(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stonewave agent station: {e}\nTry `stonewave agent station --help`.");
            return ExitCode::from(2);
        }
    };
    if args.sub == "help" {
        print!("{HELP}");
        return ExitCode::SUCCESS;
    }
    if super::running_as_root() {
        eprintln!("stonewave agent station: run this as your normal user, not as root.");
        return ExitCode::from(2);
    }
    let dirs = match Dirs::system() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("stonewave agent station: {e}");
            return ExitCode::from(2);
        }
    };
    match command(&args, &dirs) {
        Ok(code) => code,
        Err(e) => {
            eprintln!("stonewave agent station: {}", scrub(&e));
            ExitCode::from(2)
        }
    }
}

fn agent_of(args: &Args, dirs: &Dirs) -> Result<String, String> {
    match &args.agent {
        Some(a) => Ok(a.clone()),
        None => super::manage::pick_agent(dirs, None, true),
    }
}

/// Builds the configuration from the flags, the saved state of `agent connect` and the environment.
pub fn build_config(
    args: &Args,
    dirs: &Dirs,
    env: &dyn Fn(&str) -> Option<String>,
) -> Result<Config, String> {
    let agent = agent_of(args, dirs)?;
    let saved = super::state::State::load(&dirs.state_file(&agent)).ok();
    let wave_raw = args
        .wave
        .clone()
        .or_else(|| env("STONEWAVE_WAVE_URL").filter(|v| !v.is_empty()))
        .or_else(|| {
            saved
                .as_ref()
                .map(|s| s.wave_url.clone())
                .filter(|v| !v.is_empty())
        })
        .ok_or("no address of Wave: give --wave <url> (or STONEWAVE_WAVE_URL)")?;
    let wave = Account::parse_as("Wave", &wave_raw).map_err(|e| e.to_string())?;
    let mcp_url = super::wave::mcp_address(&wave, None).map_err(|e| e.to_string())?;
    let key = match &args.key_env {
        Some(n) => KeySource::Env(n.clone()),
        None => KeySource::File(dirs.key_file(&agent)),
    };
    let workdir = args
        .workdir
        .clone()
        .unwrap_or_else(|| dirs.home.join("wave-agents").join(&agent));
    let mut cfg = Config::new(&agent, &mcp_url, key, workdir);
    cfg.rooms = args.rooms.clone();
    if let Some(n) = args.poll_secs {
        cfg.poll = Duration::from_secs(n);
    }
    if let Some(n) = args.timeout_min {
        cfg.timeout = Duration::from_secs(n * 60);
    }
    if let Some(n) = args.max_turns {
        cfg.settings.max_turns = n;
    }
    if let Some(b) = args.budget {
        cfg.settings.budget_usd = b;
    }
    if let Some(n) = args.per_hour {
        cfg.per_hour = n;
    }
    if let Some(n) = args.per_day {
        cfg.per_day = n;
    }
    if let Some(n) = args.session_max_wakes {
        cfg.session_max_wakes = n;
    }
    // a relative path would be looked up from the room folder after the station changes into it: make it a full one now
    cfg.program = args
        .claude
        .as_ref()
        .map(|p| std::fs::canonicalize(p).unwrap_or_else(|_| p.clone()));
    cfg.settings.model = args.model.clone();
    Ok(cfg)
}

fn now_secs() -> u64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0, |d| d.as_secs())
}

fn command(args: &Args, dirs: &Dirs) -> Result<ExitCode, String> {
    let env_get = |k: &str| std::env::var(k).ok();
    match args.sub.as_str() {
        "pause" | "resume" => {
            let agent = agent_of(args, dirs)?;
            let p = station_run::paths(dirs, &agent);
            std::fs::create_dir_all(&dirs.state).map_err(|e| {
                format!(
                    "cannot create {}: {}",
                    super::dirs::text(&dirs.state),
                    e.kind()
                )
            })?;
            if args.sub == "pause" {
                std::fs::write(&p.paused, b"paused\n")
                    .map_err(|e| format!("cannot pause: {}", e.kind()))?;
                println!(
                    "paused: a running station starts no new wake (a wake already running finishes). `stonewave agent station resume` ends the pause."
                );
            } else {
                let _ = std::fs::remove_file(&p.paused);
                println!("resumed.");
            }
            Ok(ExitCode::SUCCESS)
        }
        "status" => {
            let agent = agent_of(args, dirs)?;
            let p = station_run::paths(dirs, &agent);
            print!("{}", status_text(&agent, &p, now_secs()));
            Ok(ExitCode::SUCCESS)
        }
        _ => run(args, dirs, &env_get),
    }
}

pub fn status_text(agent: &str, p: &station_run::Paths, now: u64) -> String {
    let st = station_run::StationState::load(&p.state);
    let ss = super::sessions::Sessions::load(&p.sessions);
    let mut o = format!("agent {agent}\n");
    o.push_str(&format!(
        "paused: {}\n",
        if p.paused.exists() { "yes" } else { "no" }
    ));
    let n = |secs: u64| {
        st.runs
            .iter()
            .filter(|t| now.saturating_sub(**t) < secs)
            .count()
    };
    o.push_str(&format!(
        "wakes: {} in the last hour, {} in the last day\n",
        n(3600),
        n(86_400)
    ));
    o.push_str(&format!(
        "inbox position: {}\n",
        st.next_seq
            .map_or("not started yet".to_owned(), |s| s.to_string())
    ));
    if ss.rooms.is_empty() {
        o.push_str("sessions: none yet\n");
    }
    for (room, r) in &ss.rooms {
        o.push_str(&format!(
            "session room {}: {} wake(s){}{}\n",
            short(room),
            r.wakes,
            r.session
                .as_ref()
                .map_or(", no session now".to_owned(), |s| format!(
                    ", session {}",
                    short(s)
                )),
            if r.note.is_some() {
                ", handover note waiting"
            } else {
                ""
            }
        ));
    }
    o.push_str(&format!("wake log: {}\n", super::dirs::text(&p.runs_log)));
    o
}

/// Claude Code must know the flags the station gives it: ask it for its version before the first wake.
fn check_claude(cfg: &Config, which: &dyn Fn(&str) -> Option<PathBuf>) -> Result<(), String> {
    let program = super::spawn::find_program(cfg.program.as_deref(), "claude", which).ok_or(
        "Claude Code (`claude`) is not on the PATH; install it or give its path with --claude",
    )?;
    let out = System
        .run(&program.display().to_string(), &["--version".to_owned()])
        .map_err(|e| {
            format!(
                "cannot start Claude Code ({}): {}",
                super::dirs::text(&program),
                e.kind()
            )
        })?;
    if !out.ok || !super::claude::version_ok(&out.stdout) {
        let v = crate::connect::tame(out.stdout.lines().next().unwrap_or("").trim(), 40);
        let m = super::claude::MIN_VERSION;
        return Err(format!(
            "Claude Code `{v}` is too old or not Claude Code: the station needs {}.{}.{} or newer (`claude update`)",
            m.0, m.1, m.2
        ));
    }
    Ok(())
}

fn run(
    args: &Args,
    dirs: &Dirs,
    env_get: &dyn Fn(&str) -> Option<String>,
) -> Result<ExitCode, String> {
    let cfg = build_config(args, dirs, env_get)?;
    if cfg.rooms.is_empty() {
        return Err("name the rooms to serve with --rooms \"Room A,Room B\" (a name, a short name or an id); a station that answers in every room it can read is not the default".into());
    }
    let paths = station_run::paths(dirs, &cfg.agent);
    let key = if args.dry_run {
        // a dry run needs to know the key exists, not to use it
        station_run::load_key(&cfg.key, env_get).unwrap_or_else(|_| {
            super::key::Key::parse("wak_dry-run-placeholder").expect("a valid shape")
        })
    } else {
        station_run::load_key(&cfg.key, env_get)?
    };
    let mcp = Account::parse_as("Wave", cfg.mcp_url.trim_end_matches("/mcp"))
        .map_err(|e| e.to_string())?;
    let http = Curl {
        allow_plain_http: !mcp.is_https(),
    };
    let which = |n: &str| System.which(n);
    // `vars_os`: a variable that is not valid text must not stop the station; it is simply not passed on
    let env_all = || {
        std::env::vars_os()
            .filter_map(|(k, v)| Some((k.into_string().ok()?, v.into_string().ok()?)))
            .collect::<Vec<_>>()
    };
    let now = now_secs;
    let spawn: &dyn Spawn = &RealSpawn;
    STOP.store(false, Ordering::SeqCst);
    let deps = Deps {
        http: &http as &dyn Http,
        spawn,
        now: &now,
        env: &env_all,
        which: &which,
        stop: &STOP,
    };
    let agent = cfg.agent.clone();
    let mut st = Station::new(cfg, deps, dirs.clone(), paths, key);
    if args.dry_run {
        print!("{}", st.dry_run_text());
        // a dry run that would be refused for real says so, and ends with an error: the menu saves nothing after one
        let mut refused = !st.can_plan();
        let key_file = match &st.cfg.key {
            KeySource::File(p) => Some(p.as_path()),
            KeySource::Env(_) => None,
        };
        match super::spawn::find_program(st.cfg.program.as_deref(), "claude", &which) {
            Some(p) if p.is_file() => {}
            _ => {
                println!(
                    "\nrefused: Claude Code (`claude`) was not found; install it or give its path with --claude"
                );
                refused = true;
            }
        }
        if let Err(e) = station_run::check_workdir_place(&st.cfg.workdir, dirs, key_file) {
            println!("\nrefused: the working folder {}", scrub(&e));
            refused = true;
        }
        match &st.cfg.key {
            KeySource::Env(n) if env_get(n).is_none() => {
                println!("note: {n} is not set in this shell; the real run needs it")
            }
            KeySource::File(p) if !p.exists() => println!(
                "note: the key file does not exist yet; `stonewave agent connect` makes it"
            ),
            _ => {}
        }
        return Ok(if refused {
            ExitCode::from(1)
        } else {
            ExitCode::SUCCESS
        });
    }
    check_claude(&st.cfg, &which)?;
    let warnings = station_run::prepare(&st)?;
    for w in &warnings {
        say(&format!("warning: {w}"));
    }
    // one station per agent: a second one would share the inbox position and wake twice
    let _lock = station_run::Lock::acquire(&st.paths.lock)?;
    if let Some(pid) = station_run::previous_wake_running(&st.paths.child) {
        return Err(format!(
            "a wake of an earlier station is still running (process {pid}); let it end, or end it yourself, then start again"
        ));
    }
    let _signals = Signals::install();
    say(&format!(
        "started for agent {agent}; one session per room is continued when a person of the company mentions it"
    ));
    let mut last_wait: Option<String> = None;
    let mut errors = 0u32;
    loop {
        let rep = st.cycle();
        print_report(&rep, &mut last_wait);
        if let Some(f) = &rep.fatal {
            say(&format!("stopped: {f}"));
            return Ok(ExitCode::from(1));
        }
        if args.once {
            return Ok(ExitCode::SUCCESS);
        }
        // only trouble on the way to Wave makes the station wait longer and longer; a pause or a limit is looked at again at the normal pace
        errors = if rep.backoff { (errors + 1).min(6) } else { 0 };
        let mut pause = st.cfg.poll * 2u32.pow(errors.min(5));
        if let Some(s) = rep.wait_secs {
            pause = pause.max(Duration::from_secs(s.min(3600)));
        }
        let pause = pause.min(Duration::from_secs(3600));
        let mut waited = Duration::ZERO;
        while waited < pause {
            if STOP.load(Ordering::SeqCst) {
                say("stopped");
                return Ok(ExitCode::SUCCESS);
            }
            std::thread::sleep(Duration::from_millis(500));
            waited += Duration::from_millis(500);
        }
        if STOP.load(Ordering::SeqCst) {
            say("stopped");
            return Ok(ExitCode::SUCCESS);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Args, String> {
        parse(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn run_options() {
        let a = p(&[
            "run",
            "--agent",
            "demo",
            "--key-env",
            "MY_AGENT_KEY",
            "--rooms",
            "Team room, Company room",
            "--max-budget-usd",
            "none",
            "--dry-run",
        ])
        .unwrap();
        assert_eq!(a.sub, "run");
        assert_eq!(a.rooms, vec!["Team room", "Company room"]);
        assert_eq!(a.budget, Some(None));
        assert!(a.dry_run);
    }

    #[test]
    fn the_key_itself_is_refused_as_a_value() {
        assert!(p(&["run", "--key-env", "wak_abc_def"]).is_err());
        assert!(p(&["run", "--key-env", "my key"]).is_err());
        assert!(p(&["run", "--key-env", "1BAD"]).is_err());
    }

    #[test]
    fn numbers_and_places_are_checked() {
        assert!(p(&["run", "--per-hour", "0"]).is_err());
        assert!(p(&["run", "--max-budget-usd", "-1"]).is_err());
        assert!(p(&["run", "--model", "a b"]).is_err());
        assert!(p(&["pause", "--dry-run"]).is_err());
        assert!(p(&["deploy"]).is_err());
        assert_eq!(p(&[]).unwrap().sub, "help");
        assert_eq!(
            p(&["run", "--timeout-min", "9999"]).unwrap().timeout_min,
            Some(240)
        );
    }
}
