//! The coding agents Wave can be connected to, and how each one learns about Wave.
//!
//! The key is never part of a registration: Claude Code and Grok get the header `Authorization: Bearer ${WAVE_AGENT_KEY}` with the
//! variable name spelled out (they fill it in from their environment), Codex gets `bearer_token_env_var = "WAVE_AGENT_KEY"`.
//! `stonewave agent run -- <engine>` sets the variable for that one process.

use std::path::PathBuf;

use super::codex;
use super::dirs::Dirs;
use super::exec::Exec;
use super::key::scrub;

/// The name of the server entry in every engine.
pub const SERVER_NAME: &str = "wave";
/// The variable the engines read the key from.
pub const KEY_VAR: &str = "WAVE_AGENT_KEY";
/// Literally this text goes into the engine's configuration; nothing expands it here.
pub const HEADER: &str = "Authorization: Bearer ${WAVE_AGENT_KEY}";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Engine {
    ClaudeCode,
    Codex,
    Grok,
    Other,
}

pub const ALL: [Engine; 4] = [
    Engine::ClaudeCode,
    Engine::Codex,
    Engine::Grok,
    Engine::Other,
];

impl Engine {
    pub fn parse(s: &str) -> Option<Engine> {
        match s {
            "claude-code" | "claude" => Some(Engine::ClaudeCode),
            "codex" => Some(Engine::Codex),
            "grok" => Some(Engine::Grok),
            "other" => Some(Engine::Other),
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Engine::ClaudeCode => "claude-code",
            Engine::Codex => "codex",
            Engine::Grok => "grok",
            Engine::Other => "other",
        }
    }

    pub fn label(self) -> &'static str {
        match self {
            Engine::ClaudeCode => "Claude Code",
            Engine::Codex => "Codex",
            Engine::Grok => "Grok Build",
            Engine::Other => "another agent",
        }
    }

    /// The program on the PATH; none for `other`.
    pub fn program(self) -> Option<&'static str> {
        match self {
            Engine::ClaudeCode => Some("claude"),
            Engine::Codex => Some("codex"),
            Engine::Grok => Some("grok"),
            Engine::Other => None,
        }
    }
}

/// Engines whose program is on the PATH.
pub fn detect(exec: &dyn Exec) -> Vec<Engine> {
    ALL.iter()
        .copied()
        .filter(|e| e.program().is_some_and(|p| exec.which(p).is_some()))
        .collect()
}

/// The program of an engine, or what is missing. `other` needs none.
pub fn locate(engine: Engine, exec: &dyn Exec) -> Result<Option<PathBuf>, String> {
    match engine.program() {
        None => Ok(None),
        Some(p) => exec.which(p).map(Some).ok_or_else(|| {
            format!(
                "`{p}` was not found on your PATH, so {} cannot be set up. Install it (or open a shell where it works) and run this again, or use `--engine other`.",
                engine.label()
            )
        }),
    }
}

fn strings(parts: &[&str]) -> Vec<String> {
    parts.iter().map(|s| (*s).to_owned()).collect()
}

/// The arguments that add the server (Claude Code, Grok). None for the other engines.
pub fn add_args(engine: Engine, mcp_url: &str) -> Option<Vec<String>> {
    match engine {
        Engine::ClaudeCode => Some(strings(&[
            "mcp",
            "add",
            "--transport",
            "http",
            "--scope",
            "user",
            SERVER_NAME,
            mcp_url,
            "--header",
            HEADER,
        ])),
        Engine::Grok => Some(strings(&[
            "mcp",
            "add",
            "--transport",
            "http",
            "-H",
            HEADER,
            SERVER_NAME,
            mcp_url,
        ])),
        Engine::Codex | Engine::Other => None,
    }
}

pub fn remove_args(engine: Engine) -> Option<Vec<String>> {
    match engine {
        Engine::ClaudeCode => Some(strings(&["mcp", "remove", SERVER_NAME, "--scope", "user"])),
        Engine::Grok => Some(strings(&["mcp", "remove", SERVER_NAME])),
        Engine::Codex | Engine::Other => None,
    }
}

/// What a registration did.
#[derive(Debug, PartialEq, Eq)]
pub enum Registered {
    Added,
    /// There was a `wave` entry, it was removed first.
    Replaced,
    /// Nothing is written for this engine (`other`).
    NothingToWrite,
}

#[derive(Debug, PartialEq, Eq)]
pub enum RegisterError {
    /// A `wave` server that is not ours is in the way (Codex).
    Foreign(PathBuf),
    Failed(String),
}

fn failed(msg: impl AsRef<str>) -> RegisterError {
    RegisterError::Failed(scrub(msg.as_ref()))
}

/// Tells the engine about Wave. `program` is where the engine's program is (not used for Codex and `other`).
pub fn register(
    engine: Engine,
    exec: &dyn Exec,
    program: Option<&std::path::Path>,
    mcp_url: &str,
    dirs: &Dirs,
) -> Result<Registered, RegisterError> {
    match engine {
        Engine::Other => Ok(Registered::NothingToWrite),
        Engine::Codex => {
            let cfg = dirs.codex_config();
            match codex::install(&cfg, mcp_url) {
                Ok(codex::Done::Replaced) => Ok(Registered::Replaced),
                Ok(_) => Ok(Registered::Added),
                Err(codex::FileError::Problem(codex::Problem::Foreign)) => {
                    Err(RegisterError::Foreign(cfg))
                }
                Err(codex::FileError::Problem(codex::Problem::Damaged)) => Err(failed(format!(
                    "{} has a stonewave begin or end marker without its partner; fix or remove the marker lines and run this again",
                    cfg.display()
                ))),
                Err(codex::FileError::Io(e)) => Err(failed(format!("{}: {e}", cfg.display()))),
            }
        }
        Engine::ClaudeCode | Engine::Grok => {
            let program = program
                .ok_or_else(|| failed("the program of the engine is not known"))?
                .to_string_lossy()
                .into_owned();
            let add = add_args(engine, mcp_url).expect("this engine has an add command");
            let first = exec
                .run(&program, &add)
                .map_err(|e| failed(format!("cannot run {}: {e}", engine.name())))?;
            if first.ok {
                return Ok(Registered::Added);
            }
            // Claude Code refuses a name that is taken. Removing a `wave` entry only works when there is one.
            let rm = remove_args(engine).expect("this engine has a remove command");
            let removed = exec.run(&program, &rm).map(|o| o.ok).unwrap_or(false);
            if !removed {
                return Err(failed(format!(
                    "`{} mcp add` failed: {}",
                    engine.program().unwrap_or("engine"),
                    first.problem()
                )));
            }
            let second = exec
                .run(&program, &add)
                .map_err(|e| failed(format!("cannot run {}: {e}", engine.name())))?;
            if second.ok {
                Ok(Registered::Replaced)
            } else {
                Err(failed(format!(
                    "`{} mcp add` failed after removing the old `wave` entry: {}",
                    engine.program().unwrap_or("engine"),
                    second.problem()
                )))
            }
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum Unregistered {
    Removed,
    NotThere,
}

/// Removes the `wave` entry this CLI made.
pub fn unregister(
    engine: Engine,
    exec: &dyn Exec,
    program: Option<&std::path::Path>,
    dirs: &Dirs,
) -> Result<Unregistered, String> {
    match engine {
        Engine::Other => Ok(Unregistered::NotThere),
        Engine::Codex => match codex::uninstall(&dirs.codex_config()) {
            Ok(true) => Ok(Unregistered::Removed),
            Ok(false) => Ok(Unregistered::NotThere),
            Err(codex::FileError::Problem(_)) => Err(format!(
                "{} has a damaged stonewave block; remove the marker lines by hand",
                dirs.codex_config().display()
            )),
            Err(codex::FileError::Io(e)) => Err(e),
        },
        Engine::ClaudeCode | Engine::Grok => {
            let program = program
                .ok_or_else(|| {
                    format!(
                        "`{}` was not found on your PATH, so its `wave` entry cannot be removed",
                        engine.program().unwrap_or("engine")
                    )
                })?
                .to_string_lossy()
                .into_owned();
            let rm = remove_args(engine).expect("this engine has a remove command");
            let out = exec
                .run(&program, &rm)
                .map_err(|e| scrub(&format!("cannot run {}: {e}", engine.name())))?;
            if out.ok {
                Ok(Unregistered::Removed)
            } else {
                // `remove` of a missing entry fails too; that is not a problem for us
                let p = out.problem();
                if p.to_lowercase().contains("no mcp server")
                    || p.to_lowercase().contains("not found")
                    || p.to_lowercase().contains("no server")
                {
                    Ok(Unregistered::NotThere)
                } else {
                    Err(scrub(&p))
                }
            }
        }
    }
}

/// Is the `wave` entry there? Read from the engine's own files, without starting it (it could connect somewhere).
/// None: cannot tell.
pub fn entry_present(engine: Engine, dirs: &Dirs) -> Option<bool> {
    match engine {
        Engine::Other => None,
        Engine::Codex => Some(
            std::fs::read_to_string(dirs.codex_config())
                .map(|t| codex::has_block(&t))
                .unwrap_or(false),
        ),
        Engine::ClaudeCode => {
            let t = std::fs::read_to_string(dirs.claude_config()).ok()?;
            let v: serde_json::Value = serde_json::from_str(&t).ok()?;
            Some(
                v.get("mcpServers")
                    .and_then(|m| m.get(SERVER_NAME))
                    .is_some(),
            )
        }
        Engine::Grok => {
            let t = std::fs::read_to_string(dirs.grok_config()).ok()?;
            Some(t.lines().any(|l| {
                let l = l.trim();
                l.starts_with('[') && l.contains(SERVER_NAME) && l.contains("mcp")
            }))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::exec::fake::{Fake, failed as fail_out};
    use super::super::key::testdir::TempDir;
    use super::*;

    const URL: &str = "https://wave.test/mcp";

    #[test]
    fn engine_names_round_trip() {
        for e in ALL {
            assert_eq!(Engine::parse(e.name()), Some(e));
        }
        assert_eq!(Engine::parse("claude"), Some(Engine::ClaudeCode));
        assert_eq!(Engine::parse("cursor"), None);
    }

    #[test]
    fn claude_and_grok_arguments_are_exact_and_hold_no_key() {
        let c = add_args(Engine::ClaudeCode, URL).unwrap();
        assert_eq!(
            c,
            [
                "mcp",
                "add",
                "--transport",
                "http",
                "--scope",
                "user",
                "wave",
                "https://wave.test/mcp",
                "--header",
                "Authorization: Bearer ${WAVE_AGENT_KEY}"
            ]
        );
        let g = add_args(Engine::Grok, URL).unwrap();
        assert_eq!(
            g,
            [
                "mcp",
                "add",
                "--transport",
                "http",
                "-H",
                "Authorization: Bearer ${WAVE_AGENT_KEY}",
                "wave",
                "https://wave.test/mcp"
            ]
        );
        assert_eq!(
            remove_args(Engine::ClaudeCode).unwrap(),
            ["mcp", "remove", "wave", "--scope", "user"]
        );
        assert_eq!(
            remove_args(Engine::Grok).unwrap(),
            ["mcp", "remove", "wave"]
        );
        assert!(add_args(Engine::Codex, URL).is_none());
        assert!(c.iter().chain(&g).all(|a| !a.contains("wak_")));
    }

    #[test]
    fn register_runs_one_add_per_engine_through_the_runner() {
        let t = TempDir::new("reg");
        let dirs = Dirs::under(t.path());
        for engine in [Engine::ClaudeCode, Engine::Grok] {
            let fake = Fake::new(&[engine.program().unwrap()]);
            let program = locate(engine, &fake).unwrap();
            let r = register(engine, &fake, program.as_deref(), URL, &dirs).unwrap();
            assert_eq!(r, Registered::Added);
            let lines = fake.lines();
            assert_eq!(lines.len(), 1, "{lines:?}");
            let p = engine.program().unwrap();
            assert!(
                lines[0].starts_with(&format!("/fake/bin/{p} mcp add --transport http")),
                "{lines:?}"
            );
            assert!(
                lines[0].contains("Bearer ${WAVE_AGENT_KEY}"),
                "literal, not expanded: {lines:?}"
            );
        }
    }

    #[test]
    fn an_existing_entry_is_removed_first_and_said() {
        let t = TempDir::new("reg2");
        let dirs = Dirs::under(t.path());
        let fake = Fake::new(&["claude"]).with(|_, args| {
            // the first add fails (name taken), everything else works
            if args.first().map(String::as_str) == Some("mcp")
                && args.get(1).map(String::as_str) == Some("add")
            {
                static FIRST: std::sync::atomic::AtomicBool =
                    std::sync::atomic::AtomicBool::new(true);
                if FIRST.swap(false, std::sync::atomic::Ordering::SeqCst) {
                    return Some(fail_out("MCP server wave already exists in user config"));
                }
            }
            None
        });
        let program = locate(Engine::ClaudeCode, &fake).unwrap();
        let r = register(Engine::ClaudeCode, &fake, program.as_deref(), URL, &dirs).unwrap();
        assert_eq!(r, Registered::Replaced);
        let lines = fake.lines();
        assert_eq!(lines.len(), 3, "{lines:?}");
        assert!(
            lines[1].ends_with("mcp remove wave --scope user"),
            "{lines:?}"
        );
        assert!(lines[2].contains("mcp add"));
    }

    #[test]
    fn a_failing_add_with_nothing_to_remove_reports_the_reason() {
        let t = TempDir::new("reg3");
        let dirs = Dirs::under(t.path());
        let fake = Fake::new(&["grok"]).with(|_, _| Some(fail_out("boom wak_secretsecretsecret")));
        let program = locate(Engine::Grok, &fake).unwrap();
        let e = register(Engine::Grok, &fake, program.as_deref(), URL, &dirs).unwrap_err();
        let RegisterError::Failed(m) = e else {
            panic!("{e:?}")
        };
        assert!(m.contains("boom"), "{m}");
        assert!(!m.contains("secretsecret"), "{m}");
    }

    #[test]
    fn missing_program_is_said_before_anything_is_written() {
        let fake = Fake::new(&[]);
        let e = locate(Engine::Grok, &fake).unwrap_err();
        assert!(e.contains("`grok` was not found"), "{e}");
        assert_eq!(locate(Engine::Other, &fake), Ok(None));
        assert!(detect(&fake).is_empty());
        let fake = Fake::new(&["codex", "grok"]);
        assert_eq!(detect(&fake), vec![Engine::Codex, Engine::Grok]);
    }

    #[test]
    fn codex_goes_through_the_file_and_foreign_tables_stop_the_step() {
        let t = TempDir::new("reg4");
        let dirs = Dirs::under(t.path());
        let fake = Fake::new(&[]);
        assert_eq!(
            register(Engine::Codex, &fake, None, URL, &dirs).unwrap(),
            Registered::Added
        );
        assert!(fake.lines().is_empty());
        assert!(codex::has_block(
            &std::fs::read_to_string(dirs.codex_config()).unwrap()
        ));
        assert_eq!(entry_present(Engine::Codex, &dirs), Some(true));
        assert_eq!(
            unregister(Engine::Codex, &fake, None, &dirs).unwrap(),
            Unregistered::Removed
        );
        assert_eq!(entry_present(Engine::Codex, &dirs), Some(false));
        std::fs::write(dirs.codex_config(), "[mcp_servers.wave]\nurl = \"x\"\n").unwrap();
        assert!(matches!(
            register(Engine::Codex, &fake, None, URL, &dirs),
            Err(RegisterError::Foreign(_))
        ));
    }

    #[test]
    fn unregister_uses_the_remove_commands_and_tolerates_a_missing_entry() {
        let t = TempDir::new("unreg");
        let dirs = Dirs::under(t.path());
        let fake = Fake::new(&["claude"]);
        let program = locate(Engine::ClaudeCode, &fake).unwrap();
        assert_eq!(
            unregister(Engine::ClaudeCode, &fake, program.as_deref(), &dirs).unwrap(),
            Unregistered::Removed
        );
        assert_eq!(
            fake.lines(),
            vec!["/fake/bin/claude mcp remove wave --scope user"]
        );
        let fake = Fake::new(&["grok"]).with(|_, _| Some(fail_out("No MCP server named wave")));
        let program = locate(Engine::Grok, &fake).unwrap();
        assert_eq!(
            unregister(Engine::Grok, &fake, program.as_deref(), &dirs).unwrap(),
            Unregistered::NotThere
        );
        let fake = Fake::new(&["grok"]).with(|_, _| Some(fail_out("permission denied")));
        assert!(unregister(Engine::Grok, &fake, program.as_deref(), &dirs).is_err());
    }

    #[test]
    fn entry_present_reads_the_engine_files_only() {
        let t = TempDir::new("present");
        let dirs = Dirs::under(t.path());
        assert_eq!(entry_present(Engine::ClaudeCode, &dirs), None);
        std::fs::create_dir_all(&dirs.home).unwrap();
        std::fs::write(
            dirs.claude_config(),
            r#"{"mcpServers":{"wave":{"type":"http"}}}"#,
        )
        .unwrap();
        assert_eq!(entry_present(Engine::ClaudeCode, &dirs), Some(true));
        std::fs::write(dirs.claude_config(), r#"{"mcpServers":{}}"#).unwrap();
        assert_eq!(entry_present(Engine::ClaudeCode, &dirs), Some(false));
        assert_eq!(entry_present(Engine::Other, &dirs), None);
    }
}
