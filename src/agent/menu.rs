//! The menu (`stonewave` on a terminal, or `stonewave agents`): which coding agents are installed here, which are connected to Wave, and the
//! next step for each (F-136, D-254). It only lists and starts the commands that exist (`agent connect`, `agent status`, `agent station ...`);
//! it keeps no state and writes nothing itself.

use std::io::{BufRead, Write};
use std::path::PathBuf;
use std::process::ExitCode;

use super::dirs::Dirs;
use super::engine::{self, Engine};
use super::exec::{Exec, System};
use super::key::scrub;
use super::state;

/// Coding agents that are found and named, but not connected by this tool yet.
const FOUND_ONLY: &[(&str, &str)] = &[("Gemini CLI", "gemini")];

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EngineRow {
    pub engine: Engine,
    pub program: Option<PathBuf>,
    pub version: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct AgentRow {
    pub name: String,
    pub engine: String,
    pub key_ok: bool,
    pub station_started: bool,
    pub paused: bool,
}

/// First plain line of `--version`, or none.
fn version_of(exec: &dyn Exec, program: &std::path::Path) -> Option<String> {
    let o = exec
        .run(&program.display().to_string(), &["--version".to_owned()])
        .ok()?;
    if !o.ok {
        return None;
    }
    let line = o.stdout.lines().find(|l| !l.trim().is_empty())?;
    Some(crate::connect::tame(line.trim(), 40))
}

pub fn detect(exec: &dyn Exec) -> Vec<EngineRow> {
    engine::ALL
        .iter()
        .filter(|e| **e != Engine::Other)
        .map(|e| {
            let program = e.program().and_then(|p| exec.which(p));
            let version = program.as_ref().and_then(|p| version_of(exec, p));
            EngineRow {
                engine: *e,
                program,
                version,
            }
        })
        .collect()
}

pub fn agents(dirs: &Dirs) -> Vec<AgentRow> {
    state::list(&dirs.config)
        .into_iter()
        .filter_map(|(name, path)| {
            let st = state::State::load(&path).ok()?;
            let p = super::station_run::paths(dirs, &name);
            Some(AgentRow {
                key_ok: dirs.key_file(&name).exists(),
                station_started: p.state.exists(),
                paused: p.paused.exists(),
                engine: st.engine,
                name,
            })
        })
        .collect()
}

/// The screen as text: pure, so a test can read it.
pub fn render(
    rows: &[EngineRow],
    found_only: &[(String, bool)],
    agents: &[AgentRow],
    is_root: bool,
) -> String {
    let mut o = String::from("Stonewave · coding agents on this computer\n\n");
    if is_root {
        o.push_str("  You are running as root. Agents are connected as your normal user: close this and run `stonewave agents` without sudo.\n");
        o.push_str(
            "  To connect this SERVER to Wave (module Systems), run: sudo stonewave connect\n",
        );
        return o;
    }
    for (i, r) in rows.iter().enumerate() {
        match &r.program {
            Some(_) => o.push_str(&format!(
                "  {}  {:<12} found{}\n",
                i + 1,
                r.engine.label(),
                r.version
                    .as_ref()
                    .map_or(String::new(), |v| format!(" ({v})"))
            )),
            None => o.push_str(&format!(
                "  {}  {:<12} not found on this computer\n",
                i + 1,
                r.engine.label()
            )),
        }
    }
    for (name, found) in found_only {
        o.push_str(&format!(
            "     {:<12} {}\n",
            name,
            if *found {
                "found; connecting it comes next"
            } else {
                "not found"
            }
        ));
    }
    o.push('\n');
    if agents.is_empty() {
        o.push_str("  Nothing is connected to Wave yet. Pick a number to connect that program.\n");
    } else {
        o.push_str("  Connected to Wave:\n");
        for a in agents {
            let station = if a.paused {
                "station paused"
            } else if a.station_started {
                "station has run"
            } else {
                "station not started"
            };
            o.push_str(&format!(
                "    {}  ({}){}  · {station}\n",
                a.name,
                a.engine,
                if a.key_ok {
                    ""
                } else {
                    "  · KEY FILE MISSING"
                }
            ));
        }
    }
    o.push_str("\n  [number] connect that program     [c] check the line to Wave     [d] show what a wake would run\n");
    o.push_str("  [s] start the station here (it keeps running; Ctrl-C stops it)     [q] quit\n");
    o
}

/// What the menu starts. Tests replace it.
pub trait Actions {
    fn run(&mut self, args: &[String]) -> u8;
}

pub struct Real;

impl Actions for Real {
    fn run(&mut self, args: &[String]) -> u8 {
        match super::main(args) {
            c if c == ExitCode::SUCCESS => 0,
            _ => 1,
        }
    }
}

/// One conversation: shows the screen, reads a line, starts what was chosen, shows the screen again. Ends on `q`, on the end of the input, or
/// after one screen when the input is not a terminal.
pub fn converse(
    exec: &dyn Exec,
    dirs: &Dirs,
    is_root: bool,
    interactive: bool,
    input: &mut dyn BufRead,
    out: &mut dyn Write,
    act: &mut dyn Actions,
) -> std::io::Result<()> {
    loop {
        let rows = detect(exec);
        let found_only: Vec<(String, bool)> = FOUND_ONLY
            .iter()
            .map(|(n, p)| ((*n).to_owned(), exec.which(p).is_some()))
            .collect();
        let ags = agents(dirs);
        write!(out, "{}", render(&rows, &found_only, &ags, is_root))?;
        if !interactive || is_root {
            return Ok(());
        }
        write!(out, "\n> ")?;
        out.flush()?;
        let mut line = String::new();
        if input.read_line(&mut line)? == 0 {
            return Ok(());
        }
        let choice = line.trim().to_lowercase();
        let one_agent = if ags.len() == 1 {
            Some(ags[0].name.clone())
        } else {
            None
        };
        let need_agent = |out: &mut dyn Write| -> std::io::Result<Option<String>> {
            match &one_agent {
                Some(a) => Ok(Some(a.clone())),
                None => {
                    writeln!(
                        out,
                        "\n  {}",
                        if ags.is_empty() {
                            "Connect a program first."
                        } else {
                            "Several agents are connected here: use `stonewave agent station run --agent <name>`."
                        }
                    )?;
                    Ok(None)
                }
            }
        };
        match choice.as_str() {
            "q" | "quit" => return Ok(()),
            "" => {}
            "c" => {
                if let Some(a) = need_agent(out)? {
                    act.run(&["status".into(), "--agent".into(), a]);
                }
            }
            "d" => {
                if let Some(a) = need_agent(out)? {
                    act.run(&[
                        "station".into(),
                        "run".into(),
                        "--agent".into(),
                        a,
                        "--dry-run".into(),
                    ]);
                }
            }
            "s" => {
                if let Some(a) = need_agent(out)? {
                    writeln!(
                        out,
                        "\n  Starting the station for {a}. Ctrl-C stops it; to keep it running, use a service (see docs/station.md).\n"
                    )?;
                    act.run(&["station".into(), "run".into(), "--agent".into(), a]);
                }
            }
            n => match n
                .parse::<usize>()
                .ok()
                .and_then(|i| rows.get(i.wrapping_sub(1)))
            {
                Some(r) if r.program.is_some() => {
                    act.run(&["connect".into(), "--engine".into(), r.engine.name().into()]);
                }
                Some(r) => writeln!(
                    out,
                    "\n  {} is not installed here. Install it first, then come back.",
                    r.engine.label()
                )?,
                None => writeln!(
                    out,
                    "\n  I did not understand `{}`. Type a number, c, d, s or q.",
                    scrub(n).chars().take(20).collect::<String>()
                )?,
            },
        }
        writeln!(out)?;
    }
}

/// `stonewave` with no words on a terminal, or `stonewave agents`.
pub fn main(interactive: bool) -> ExitCode {
    let dirs = match Dirs::system() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("stonewave: {e}");
            return ExitCode::from(2);
        }
    };
    let stdin = std::io::stdin();
    let mut input = stdin.lock();
    let mut out = std::io::stdout();
    match converse(
        &System,
        &dirs,
        super::running_as_root(),
        interactive,
        &mut input,
        &mut out,
        &mut Real,
    ) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("stonewave: {}", scrub(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::agent::exec::fake::{Fake, ok_out};

    struct Rec(Vec<Vec<String>>);
    impl Actions for Rec {
        fn run(&mut self, args: &[String]) -> u8 {
            self.0.push(args.to_vec());
            0
        }
    }

    fn dirs(label: &str) -> Dirs {
        let d = std::env::temp_dir().join(format!("stonewave-menu-{label}-{}", std::process::id()));
        let _ = std::fs::remove_dir_all(&d);
        Dirs::under(&d)
    }

    fn say(
        input: &str,
        exec: &Fake,
        d: &Dirs,
        root: bool,
        interactive: bool,
    ) -> (String, Vec<Vec<String>>) {
        let mut inp = std::io::Cursor::new(input.as_bytes().to_vec());
        let mut out = Vec::new();
        let mut rec = Rec(vec![]);
        converse(exec, d, root, interactive, &mut inp, &mut out, &mut rec).unwrap();
        (String::from_utf8(out).unwrap(), rec.0)
    }

    fn fake() -> Fake {
        Fake::new(&["claude", "gemini"]).with(|p, a| {
            if p.contains("claude") && a == ["--version"] {
                Some(ok_out("2.1.321 (Claude Code)\n"))
            } else {
                None
            }
        })
    }

    #[test]
    fn it_lists_what_is_installed_and_what_is_not() {
        let d = dirs("list");
        let (screen, acts) = say("", &fake(), &d, false, false);
        assert!(
            screen.contains("Claude Code") && screen.contains("found (2.1.321 (Claude Code))"),
            "{screen}"
        );
        assert!(screen.contains("Codex") && screen.contains("not found on this computer"));
        assert!(screen.contains("Gemini CLI") && screen.contains("connecting it comes next"));
        assert!(screen.contains("Nothing is connected to Wave yet"));
        assert!(
            acts.is_empty(),
            "a screen that is not interactive starts nothing"
        );
    }

    #[test]
    fn a_number_connects_that_program_and_a_missing_one_says_so() {
        let d = dirs("connect");
        let (screen, acts) = say("1\n2\nq\n", &fake(), &d, false, true);
        assert_eq!(
            acts,
            vec![vec![
                "connect".to_owned(),
                "--engine".into(),
                "claude-code".into()
            ]]
        );
        assert!(screen.contains("Codex is not installed here"), "{screen}");
    }

    #[test]
    fn check_and_station_need_exactly_one_connected_agent() {
        let d = dirs("one");
        let (screen, acts) = say("d\nq\n", &fake(), &d, false, true);
        assert!(
            acts.is_empty() && screen.contains("Connect a program first."),
            "{screen}"
        );
        let st = state::sample("agent-demo");
        std::fs::create_dir_all(&d.config).unwrap();
        st.save(&d.state_file("agent-demo")).unwrap();
        let (screen, acts) = say("d\nc\nx\nq\n", &fake(), &d, false, true);
        assert!(
            screen.contains("agent-demo") && screen.contains("KEY FILE MISSING"),
            "{screen}"
        );
        assert_eq!(
            acts[0],
            vec!["station", "run", "--agent", "agent-demo", "--dry-run"]
        );
        assert_eq!(acts[1], vec!["status", "--agent", "agent-demo"]);
        assert!(screen.contains("I did not understand `x`"));
    }

    #[test]
    fn as_root_it_connects_nothing_and_points_to_the_server_command() {
        let d = dirs("root");
        let (screen, acts) = say("1\n", &fake(), &d, true, true);
        assert!(screen.contains("run as root") || screen.contains("running as root"));
        assert!(screen.contains("sudo stonewave connect"));
        assert!(acts.is_empty());
    }

    #[test]
    fn the_end_of_the_input_ends_the_menu() {
        let d = dirs("eof");
        let (_, acts) = say("", &fake(), &d, false, true);
        assert!(acts.is_empty());
    }
}
