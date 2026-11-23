//! `stonewave agent ...`: connect a coding agent (Claude Code, Codex, Grok Build, anything else) on this machine to Wave as an
//! external agent of a company.
//!
//! - `connect`: device code sign-in (`purpose: "agent"`), the key from Wave into a private file, the `wave` entry in the engine
//!   (never with the key in it), and on request a wake service
//! - `status`, `rotate`, `disconnect`, `run`
//!
//! The key never appears on screen, in a log, in a command line, in a config file of an engine or in any `Debug` output.
//! See [`key`] for the type that carries it and the files it lives in.

use std::process::ExitCode;
use std::time::Duration;

use crate::connect::{Account, Curl, Http};
use crate::flow::{Host, Outcome};
use crate::term;

pub mod claude;
pub mod cli;
pub mod codex;
pub mod connect_flow;
pub mod dirs;
pub mod engine;
pub mod exec;
pub mod key;
pub mod manage;
pub mod menu;
pub mod sessions;
pub mod spawn;
pub mod state;
pub mod station;
pub mod station_cli;
pub mod station_run;
pub mod timefmt;
pub mod ui;
pub mod wave;
pub mod winproc;

#[cfg(test)]
mod e2e_tests;
#[cfg(test)]
mod flow_tests;
#[cfg(test)]
pub mod testkit;

/// How long the wait for Wave to set up a new agent goes on.
#[derive(Debug, Clone, Copy)]
pub struct Timing {
    /// Between two tries.
    pub mint_retry: Duration,
    /// In total (the sign-in token itself is good for 15 minutes).
    pub mint_max_wait: Duration,
}

impl Default for Timing {
    fn default() -> Timing {
        Timing {
            mint_retry: Duration::from_secs(5),
            mint_max_wait: Duration::from_secs(10 * 60),
        }
    }
}

/// Makes the HTTP transport for an address (plain `http` only where the address rules allowed it).
pub type MakeHttp<'a> = &'a dyn Fn(&Account) -> Box<dyn Http>;

/// Everything the commands need from the machine, in one place so tests can replace it.
pub struct Ctx<'a> {
    pub dirs: &'a dirs::Dirs,
    pub exec: &'a dyn exec::Exec,
    pub http_for: MakeHttp<'a>,
    pub host: &'a str,
    pub os: &'a str,
    pub timing: Timing,
}

fn env_url(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn real_http(a: &Account) -> Box<dyn Http> {
    Box::new(Curl {
        allow_plain_http: !a.is_https(),
    })
}

/// A key made by root belongs to nobody: refuse.
pub(crate) fn running_as_root() -> bool {
    #[cfg(unix)]
    {
        // SAFETY: geteuid has no arguments and cannot fail.
        unsafe { libc::geteuid() == 0 }
    }
    #[cfg(not(unix))]
    {
        false
    }
}

pub fn main(raw: &[String]) -> ExitCode {
    // `stonewave agent station ...` has its own words and options
    if raw.first().map(String::as_str) == Some("station") {
        return station_cli::main(&raw[1..]);
    }
    let args = match cli::parse(raw) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stonewave agent: {e}\nTry `stonewave agent --help`.");
            return ExitCode::from(2);
        }
    };
    if args.sub == cli::Sub::Help {
        print!("{}", cli::HELP);
        return ExitCode::SUCCESS;
    }
    if running_as_root() {
        eprintln!(
            "stonewave agent: run this as your normal user, not as root (a key made by root would belong to nobody, and no part of this needs sudo)."
        );
        return ExitCode::from(2);
    }
    let dirs = match dirs::Dirs::system() {
        Ok(d) => d,
        Err(e) => {
            eprintln!("stonewave agent: {e}");
            return ExitCode::from(2);
        }
    };
    let host = Host::detect(None, None, None);
    let exec = exec::System;
    let ctx = Ctx {
        dirs: &dirs,
        exec: &exec,
        http_for: &real_http,
        host: &host.name,
        os: &host.os,
        timing: Timing::default(),
    };
    let wave = args.wave.clone().or_else(|| env_url("STONEWAVE_WAVE_URL"));

    match args.sub {
        cli::Sub::Run => return run(&ctx, &args),
        cli::Sub::Status if args.json => {
            println!(
                "{}",
                manage::status_json(&ctx, args.agent.as_deref(), wave.as_deref())
            );
            return ExitCode::SUCCESS;
        }
        _ => {}
    }

    let account = match args.sub {
        cli::Sub::Connect => {
            let raw = args
                .account
                .clone()
                .or_else(|| env_url("STONEWAVE_ACCOUNT_URL"))
                .unwrap_or_else(|| crate::connect::DEFAULT_ACCOUNT.to_owned());
            match Account::parse(&raw) {
                Ok(a) => Some(a),
                Err(e) => {
                    eprintln!("stonewave agent: {e}");
                    return ExitCode::from(2);
                }
            }
        }
        _ => None,
    };

    let mut term = match term::Term::open(&term::Options {
        no_anim: args.no_anim,
        ..term::Options::default()
    }) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("stonewave agent: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let result = {
        let mut ui = ui::TermUi::new(&mut term);
        match args.sub {
            cli::Sub::Connect => connect_flow::connect(
                &ctx,
                &mut ui,
                &connect_flow::ConnectOpts {
                    engine: args.engine,
                    company: args.company.clone(),
                    name: args.name.clone(),
                    no_station: args.no_station,
                    yes: args.yes,
                    account: account.expect("connect has an account address"),
                    wave,
                },
            ),
            cli::Sub::Status => {
                manage::status(&ctx, &mut ui, args.agent.as_deref(), wave.as_deref())
            }
            cli::Sub::Rotate => {
                manage::rotate(&ctx, &mut ui, args.agent.as_deref(), wave.as_deref())
            }
            cli::Sub::Disconnect => manage::disconnect(
                &ctx,
                &mut ui,
                &manage::DisconnectOpts {
                    agent: args.agent.as_deref(),
                    wave: wave.as_deref(),
                    yes: args.yes,
                    keep_remote: args.keep_remote,
                },
            ),
            cli::Sub::Run | cli::Sub::Help => unreachable!("handled above"),
        }
    };
    drop(term);
    match result {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::Declined | Outcome::Failed) => ExitCode::from(1),
        Err(term::Error::Interrupted) => {
            eprintln!(
                "\n  stopped. A key file is only ever written whole, so nothing is half done; `stonewave agent status` shows what exists."
            );
            ExitCode::from(130)
        }
        Err(term::Error::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(term::Error::Io(e)) => {
            eprintln!("stonewave agent: {}", key::scrub(&e.to_string()));
            ExitCode::FAILURE
        }
    }
}

/// `agent run -- <command>`: the key goes into the environment of that one process.
fn run(ctx: &Ctx<'_>, args: &cli::Args) -> ExitCode {
    let plan = match manage::plan_run(ctx.dirs, ctx.exec, args.agent.as_deref(), &args.cmd) {
        Ok(p) => p,
        Err(e) => {
            eprintln!("stonewave agent: {}", key::scrub(&e));
            return ExitCode::from(2);
        }
    };
    if let Some(w) = &plan.warning {
        eprintln!("stonewave agent: warning: {}", key::scrub(w));
    }
    let mut cmd = std::process::Command::new(&plan.program);
    cmd.args(&plan.args).env(engine::KEY_VAR, plan.key.expose());
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        // only returns when the program could not be started
        let e = cmd.exec();
        eprintln!(
            "stonewave agent: cannot start {}: {}",
            plan.program.display(),
            key::scrub(&e.to_string())
        );
        ExitCode::from(127)
    }
    #[cfg(not(unix))]
    {
        match cmd.status() {
            Ok(s) => ExitCode::from(u8::try_from(s.code().unwrap_or(1).clamp(0, 255)).unwrap_or(1)),
            Err(e) => {
                eprintln!(
                    "stonewave agent: cannot start {}: {}",
                    plan.program.display(),
                    key::scrub(&e.to_string())
                );
                ExitCode::from(127)
            }
        }
    }
}
