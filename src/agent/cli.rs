//! The command line of `stonewave agent ...`.

use super::engine::Engine;

pub const HELP: &str = "\
Usage: stonewave agent <command> [options]

Connect a coding agent on this machine (Claude Code, Codex, Grok Build, or another one) to Wave as an external agent.
Everything lives in your home folder and runs as you; nothing needs sudo, and root is refused.

Commands:
  connect      Sign in with a code, get the agent's key from Wave, keep it in a private file, add Wave to the
               agent (without the key in its configuration) and, if you want, set up the wake service
  status       What is set up here and what Wave says (--json: one JSON object, no secrets)
  rotate       Replace the key with a new one (the old one stops working); the expiry date stays the same
  disconnect   Revoke the key in Wave, then remove the key file, the `wave` entry and the wake service this tool made
  run          Start a command with the key in its environment: stonewave agent run [--agent <name>] -- claude

Options:
  --engine <claude-code|codex|grok|other>  connect: which agent (default: the one found on your PATH)
  --company <slug>      connect: suggest a company on the confirmation page
  --name <name>         connect: suggest a name for the agent on the confirmation page
  --no-station          connect: do not offer the wake service
  --yes                 connect: set up the wake service without asking; disconnect: do not ask
  --keep-remote         disconnect: only clean up here; the key keeps working in Wave until someone revokes it
  --json                status: machine-readable output
  --agent <name>        status, rotate, disconnect, run: which agent, when more than one is connected here
  --account <url>       connect: address of the account (or STONEWAVE_ACCOUNT_URL); https, http only for this machine
  --wave <url>          address of Wave's API (or STONEWAVE_WAVE_URL; else the account tells); https, http only for this machine
  --no-anim             final states only
  -h, --help            this help

Files (user only: folders 0700, files 0600, on Windows only your account):
  ~/.config/wave/<agent>.key      the key (Windows: %LOCALAPPDATA%\\wave, never the roaming profile)
  ~/.config/wave/<agent>.json     what this tool made, for `disconnect`; no key inside
  ~/.local/state/wave/<agent>.wake.log   the wake service's log (Windows: %LOCALAPPDATA%\\wave)
";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Sub {
    Connect,
    Status,
    Rotate,
    Disconnect,
    Run,
    Help,
}

#[derive(Debug)]
pub struct Args {
    pub sub: Sub,
    pub engine: Option<Engine>,
    pub company: Option<String>,
    pub name: Option<String>,
    pub agent: Option<String>,
    pub account: Option<String>,
    pub wave: Option<String>,
    pub no_station: bool,
    pub yes: bool,
    pub json: bool,
    pub keep_remote: bool,
    pub no_anim: bool,
    /// `run`: the command after `--`.
    pub cmd: Vec<String>,
}

fn slug_ok(s: &str) -> bool {
    (1..=64).contains(&s.len())
        && s.chars()
            .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-')
}

pub fn parse(raw: &[String]) -> Result<Args, String> {
    let mut a = Args {
        sub: Sub::Help,
        engine: None,
        company: None,
        name: None,
        agent: None,
        account: None,
        wave: None,
        no_station: false,
        yes: false,
        json: false,
        keep_remote: false,
        no_anim: false,
        cmd: Vec::new(),
    };
    let mut sub_set = false;
    let mut used: Vec<&'static str> = Vec::new();
    let mut it = raw.iter().cloned();
    while let Some(arg) = it.next() {
        if arg == "--" {
            a.cmd = it.by_ref().collect();
            used.push("--");
            break;
        }
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
        match flag.as_str() {
            "-h" | "--help" | "help" => {
                a.sub = Sub::Help;
                return Ok(a);
            }
            "--engine" => {
                let v = value("--engine")?;
                a.engine = Some(Engine::parse(&v).ok_or_else(|| {
                    format!("--engine: `{v}` is not one of claude-code, codex, grok, other")
                })?);
                used.push("--engine");
            }
            "--company" => {
                let v = value("--company")?;
                if !slug_ok(&v) {
                    return Err("--company: use the company's short name (lower case letters, digits, dashes)".into());
                }
                a.company = Some(v);
                used.push("--company");
            }
            "--name" => {
                let v = value("--name")?;
                if v.chars().count() > 60 || v.chars().any(char::is_control) {
                    return Err("--name: at most 60 characters, no control characters".into());
                }
                a.name = Some(v);
                used.push("--name");
            }
            "--agent" => {
                let v = value("--agent")?;
                if !crate::connect::is_agent_slug(&v) {
                    return Err(
                        "--agent: an agent name has lower case letters, digits and dashes".into(),
                    );
                }
                a.agent = Some(v);
                used.push("--agent");
            }
            "--account" => {
                a.account = Some(value("--account")?);
                used.push("--account");
            }
            "--wave" => {
                a.wave = Some(value("--wave")?);
                used.push("--wave");
            }
            "--no-station" => {
                a.no_station = true;
                used.push("--no-station");
            }
            "--yes" => {
                a.yes = true;
                used.push("--yes");
            }
            "--keep-remote" => {
                a.keep_remote = true;
                used.push("--keep-remote");
            }
            "--json" => {
                a.json = true;
                used.push("--json");
            }
            "--no-anim" => a.no_anim = true,
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            word if !sub_set => {
                a.sub = match word {
                    "connect" => Sub::Connect,
                    "status" => Sub::Status,
                    "rotate" => Sub::Rotate,
                    "disconnect" => Sub::Disconnect,
                    "run" => Sub::Run,
                    other => return Err(format!("unknown command {other}")),
                };
                sub_set = true;
            }
            extra if a.sub == Sub::Run => {
                return Err(format!("unexpected `{extra}`: put the command after `--`"));
            }
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    if !sub_set {
        a.sub = Sub::Help;
        return Ok(a);
    }
    let allowed: &[&str] = match a.sub {
        Sub::Connect => &[
            "--engine",
            "--company",
            "--name",
            "--no-station",
            "--yes",
            "--account",
            "--wave",
        ],
        Sub::Status => &["--json", "--agent", "--wave"],
        Sub::Rotate => &["--agent", "--wave"],
        Sub::Disconnect => &["--yes", "--keep-remote", "--agent", "--wave"],
        Sub::Run => &["--agent", "--"],
        Sub::Help => &[],
    };
    for u in used {
        if !allowed.contains(&u) {
            return Err(format!("{u} does not work with `{}`", sub_name(a.sub)));
        }
    }
    if a.sub == Sub::Run && a.cmd.is_empty() {
        return Err(
            "`run` needs a command after `--`: stonewave agent run [--agent <name>] -- claude"
                .into(),
        );
    }
    Ok(a)
}

fn sub_name(s: Sub) -> &'static str {
    match s {
        Sub::Connect => "connect",
        Sub::Status => "status",
        Sub::Rotate => "rotate",
        Sub::Disconnect => "disconnect",
        Sub::Run => "run",
        Sub::Help => "help",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(a: &[&str]) -> Result<Args, String> {
        parse(&a.iter().map(|s| (*s).to_owned()).collect::<Vec<_>>())
    }

    #[test]
    fn connect_options() {
        let a = p(&[
            "connect",
            "--engine",
            "grok",
            "--company=acme",
            "--name",
            "Grok (Mac)",
            "--no-station",
            "--yes",
            "--wave",
            "https://wave.test",
        ])
        .unwrap();
        assert_eq!(a.sub, Sub::Connect);
        assert_eq!(a.engine, Some(Engine::Grok));
        assert_eq!(a.company.as_deref(), Some("acme"));
        assert_eq!(a.name.as_deref(), Some("Grok (Mac)"));
        assert!(a.no_station && a.yes);
        assert_eq!(a.wave.as_deref(), Some("https://wave.test"));
    }

    #[test]
    fn run_takes_everything_after_the_dashes() {
        let a = p(&[
            "run",
            "--agent",
            "agent-demo",
            "--",
            "claude",
            "--resume",
            "--agent",
            "x",
        ])
        .unwrap();
        assert_eq!(a.sub, Sub::Run);
        assert_eq!(a.agent.as_deref(), Some("agent-demo"));
        assert_eq!(a.cmd, vec!["claude", "--resume", "--agent", "x"]);
        assert!(p(&["run"]).is_err());
        assert!(p(&["run", "claude"]).unwrap_err().contains("after `--`"));
        assert!(p(&["run", "--"]).is_err());
    }

    #[test]
    fn options_only_where_they_mean_something() {
        assert!(p(&["status", "--json"]).unwrap().json);
        assert!(p(&["connect", "--json"]).is_err());
        assert!(p(&["status", "--yes"]).is_err());
        assert!(p(&["rotate", "--engine", "grok"]).is_err());
        assert!(p(&["disconnect", "--keep-remote", "--yes"]).is_ok());
        assert!(p(&["connect", "--keep-remote"]).is_err());
        assert!(p(&["connect", "--account"]).is_err());
        assert!(p(&["connect", "--engine", "cursor"]).is_err());
        assert!(p(&["connect", "--company", "Acme Corp"]).is_err());
        assert!(p(&["connect", "--agent", "BAD"]).is_err());
        assert!(p(&["deploy"]).is_err());
        assert!(p(&["status", "extra"]).is_err());
    }

    #[test]
    fn help_forms() {
        assert_eq!(p(&[]).unwrap().sub, Sub::Help);
        assert_eq!(p(&["--help"]).unwrap().sub, Sub::Help);
        assert_eq!(p(&["connect", "--help"]).unwrap().sub, Sub::Help);
        assert!(HELP.contains("stonewave agent run"));
    }
}
