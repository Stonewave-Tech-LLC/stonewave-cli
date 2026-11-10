//! `stonewave`: the Stonewave server CLI, early build.
//!
//! `doctor` is real and read only: it checks whether this server can be
//! connected to Wave. `connect` really signs in with a device code at the
//! Stonewave account (and changes nothing on the machine yet); `connect --demo`,
//! `setup` and `revoke` change the server (with sudo; `--dry-run` only looks). `status` and `connect --demo` are demos of the look that talk to nobody. See `README.md`.

mod b64;
mod banner;
mod connect;
mod daemon;
mod doctor;
mod flow;
mod health;
mod realsys;
mod relay;
mod setup;
mod setup_flow;
mod status;
mod style;
mod term;
mod tunnel;
mod ui;

use std::process::ExitCode;

use flow::Outcome;

pub const VERSION: &str = env!("CARGO_PKG_VERSION");

#[derive(Clone, Copy, PartialEq, Eq, Debug)]
enum Command {
    Connect,
    Status,
    Revoke,
    Banner,
    Doctor,
    Setup,
    Daemon,
    AgentShell,
    Help,
    Version,
}

#[derive(Debug)]
struct Args {
    command: Command,
    term: term::Options,
    host: Option<String>,
    os: Option<String>,
    user: Option<String>,
    seed: Option<u64>,
    /// `--json`: machine-readable output (doctor only), for agents.
    json: bool,
    /// `--demo`: connect plays the old made-up screens and talks to nobody.
    demo: bool,
    /// `--account <url>`: where the account lives (default: STONEWAVE_ACCOUNT_URL, then the Stonewave account).
    account: Option<String>,
    /// `--relay <url>`: where the relay lives (default: STONEWAVE_RELAY_URL, then the Stonewave relay).
    relay: Option<String>,
    /// `--sign-in-only`: connect signs in and stops there (nothing is registered or changed).
    sign_in_only: bool,
    /// `--dry-run`: setup and revoke only show what they would do.
    dry_run: bool,
    /// `--yes`: setup and revoke go ahead without asking.
    yes: bool,
    /// `--trust-file <file>`: setup: admin device keys to trust.
    trust_files: Vec<String>,
    /// `--device <name>`: revoke: only this admin device.
    device: Option<String>,
}

const HELP: &str = "\
Usage: stonewave [command] [options]

Commands:
  doctor         Check whether this server can be connected (real, read only)
  connect        Sign in with your Stonewave account, register this server with Wave and set it up (default; needs sudo)
  setup          Set up this server: agent user, agent SSH, daemon, audit log (needs sudo; --dry-run only looks)
  revoke         Revoke all agent access on this server, also without internet (needs sudo)
  status         The line to Wave, the agent SSH and the last health report (--demo: made-up screens)
  banner         Play the banner

Options:
  --json         doctor: print one JSON object instead of the screen (for agents)
  --account <url>  connect: address of the account (or STONEWAVE_ACCOUNT_URL); https only
  --demo         connect, status: play the made-up screens, talk to nobody
  --relay <url>  connect: address of the Wave relay (or STONEWAVE_RELAY_URL); https only
  --sign-in-only connect: only sign in; register nothing, change nothing
  --dry-run      setup, revoke: show what would change, change nothing
  --yes          connect, setup, revoke: do it without asking
  --trust-file <file>  connect, setup: public keys of admin devices to trust (one per line)
  --device <name>      revoke: only remove this admin device
  --no-anim      Final states only (also NO_COLOR, STONEWAVE_NO_ANIM=1, TERM=dumb, pipes)
  --host <name>  Demo: host name to show
  --os <name>    Demo: OS to show
  --user <name>  Demo: user to show
  -h, --help     This help
  -V, --version  Version

Recording (screenshots and GIFs):
  --autoplay       Answer prompts by itself
  --record <file>  Also write an asciicast v2 file
  --live           Draw in place even when output is not a terminal
  --width <cols>   Width when output is not a terminal (default 80)
  --seed <n>       Same demo code and words every time
";

fn parse(raw: impl IntoIterator<Item = String>) -> Result<Args, String> {
    let mut args = Args {
        command: Command::Connect,
        term: term::Options::default(),
        host: None,
        os: None,
        user: None,
        seed: None,
        json: false,
        demo: false,
        account: None,
        relay: None,
        sign_in_only: false,
        dry_run: false,
        yes: false,
        trust_files: Vec::new(),
        device: None,
    };
    let mut command_set = false;
    let mut it = raw.into_iter();
    while let Some(arg) = it.next() {
        // Accept both `--opt value` and `--opt=value`.
        let (flag, inline) = match arg.split_once('=') {
            Some((f, v)) if f.starts_with("--") => (f.to_owned(), Some(v.to_owned())),
            _ => (arg.clone(), None),
        };
        let mut value = |name: &str| -> Result<String, String> {
            inline
                .clone()
                .or_else(|| it.next())
                .ok_or_else(|| format!("{name} needs a value"))
        };
        match flag.as_str() {
            "-h" | "--help" | "help" => args.command = Command::Help,
            "-V" | "--version" => args.command = Command::Version,
            "--no-anim" => args.term.no_anim = true,
            "--json" => args.json = true,
            "--demo" => args.demo = true,
            "--account" => args.account = Some(value("--account")?),
            "--relay" => args.relay = Some(value("--relay")?),
            "--sign-in-only" => args.sign_in_only = true,
            "--dry-run" => args.dry_run = true,
            "--yes" => args.yes = true,
            "--trust-file" => args.trust_files.push(value("--trust-file")?),
            "--device" => args.device = Some(value("--device")?),
            "--autoplay" => args.term.autoplay = true,
            "--live" => args.term.force_live = true,
            "--record" => args.term.record = Some(value("--record")?.into()),
            "--width" => {
                let v = value("--width")?;
                let w: u16 = v
                    .parse()
                    .map_err(|_| format!("--width: not a number: {v}"))?;
                args.term.width = Some(w.clamp(40, 400));
            }
            "--seed" => {
                let v = value("--seed")?;
                args.seed = Some(
                    v.parse()
                        .map_err(|_| format!("--seed: not a number: {v}"))?,
                );
            }
            "--host" => args.host = Some(value("--host")?),
            "--os" => args.os = Some(value("--os")?),
            "--user" => args.user = Some(value("--user")?),
            s if s.starts_with('-') => return Err(format!("unknown option {s}")),
            word if !command_set => {
                args.command = match word {
                    "connect" => Command::Connect,
                    "status" => Command::Status,
                    "revoke" => Command::Revoke,
                    "banner" => Command::Banner,
                    "doctor" => Command::Doctor,
                    "setup" => Command::Setup,
                    "daemon" => Command::Daemon,
                    "agent-shell" => Command::AgentShell,
                    other => return Err(format!("unknown command {other}")),
                };
                command_set = true;
            }
            extra => return Err(format!("unexpected argument {extra}")),
        }
    }
    if args.json
        && !matches!(
            args.command,
            Command::Doctor | Command::Help | Command::Version
        )
    {
        return Err("--json only works with doctor".into());
    }
    if args.demo && !matches!(args.command, Command::Connect | Command::Status) {
        return Err("--demo only works with connect and status".into());
    }
    if (args.account.is_some() || args.relay.is_some() || args.sign_in_only)
        && args.command != Command::Connect
    {
        return Err("--account, --relay and --sign-in-only only work with connect".into());
    }
    if args.demo
        && (args.account.is_some() || args.relay.is_some() || args.sign_in_only || args.yes)
    {
        return Err("--demo talks to nobody, so --account, --relay, --sign-in-only and --yes make no sense with it".into());
    }
    if args.dry_run && args.command == Command::Connect {
        return Err("connect has no --dry-run: `stonewave setup --dry-run` shows the setup, `connect --sign-in-only` only signs in".into());
    }
    let changes_server = matches!(
        args.command,
        Command::Setup | Command::Revoke | Command::Connect
    );
    if (args.dry_run || args.yes) && !changes_server {
        return Err("--dry-run and --yes only work with connect, setup and revoke".into());
    }
    if !args.trust_files.is_empty() && !matches!(args.command, Command::Setup | Command::Connect) {
        return Err("--trust-file only works with connect and setup".into());
    }
    if args.device.is_some() && args.command != Command::Revoke {
        return Err("--device only works with revoke".into());
    }
    Ok(args)
}

fn env_url(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

fn main() -> ExitCode {
    let args = match parse(std::env::args().skip(1)) {
        Ok(a) => a,
        Err(e) => {
            eprintln!("stonewave: {e}\nTry `stonewave --help`.");
            return ExitCode::from(2);
        }
    };
    match args.command {
        Command::Help => {
            println!(
                "stonewave {VERSION} · early build: doctor is real and read only, the rest is a demo that changes nothing\n"
            );
            print!("{HELP}");
            return ExitCode::SUCCESS;
        }
        Command::Version => {
            println!("stonewave {VERSION} (early build)");
            return ExitCode::SUCCESS;
        }
        Command::Doctor if args.json => {
            let host = flow::Host::detect(args.host, args.os, args.user);
            let checks = doctor::run_all(&doctor::SYSTEM);
            println!("{}", doctor::to_json(&host.name, &checks));
            return if doctor::passed(&checks) {
                ExitCode::SUCCESS
            } else {
                ExitCode::from(1)
            };
        }
        Command::Daemon => return daemon::run_daemon(),
        Command::AgentShell => return daemon::run_agent_shell(),
        _ => {}
    }

    let mut term = match term::Term::open(&args.term) {
        Ok(t) => t,
        Err(e) => {
            eprintln!("stonewave: cannot start: {e}");
            return ExitCode::FAILURE;
        }
    };
    let host = flow::Host::detect(args.host, args.os, args.user);
    let mut rng = flow::Rng::new(args.seed);

    let result = match args.command {
        Command::Connect if args.demo => flow::connect(&mut term, &host, &mut rng),
        Command::Connect => {
            let raw = args
                .account
                .or_else(|| env_url("STONEWAVE_ACCOUNT_URL"))
                .unwrap_or_else(|| connect::DEFAULT_ACCOUNT.to_owned());
            let raw_relay = args
                .relay
                .or_else(|| env_url("STONEWAVE_RELAY_URL"))
                .unwrap_or_else(|| connect::DEFAULT_RELAY.to_owned());
            let (account, relay) = match (
                connect::Account::parse(&raw),
                connect::Account::parse(&raw_relay),
            ) {
                (Ok(a), Ok(r)) => (a, r),
                (Err(e), _) | (_, Err(e)) => {
                    drop(term);
                    eprintln!("stonewave: {e}");
                    return ExitCode::from(2);
                }
            };
            // Registering a server changes it. That needs root, and it is kinder to say so before the person signs in.
            if !args.sign_in_only && setup::Sys::euid(&realsys::RealSys) != 0 {
                drop(term);
                eprintln!(
                    "stonewave: connect sets this server up and needs root. Run `sudo stonewave connect` (or add --sign-in-only to only sign in)."
                );
                return ExitCode::from(2);
            }
            let http = connect::Curl {
                allow_plain_http: !account.is_https(),
            };
            let relay_http = connect::Curl {
                allow_plain_http: !relay.is_https(),
            };
            let enrol = flow::Enrol {
                relay: &relay,
                http: &relay_http,
                yes: args.yes,
                trust: Vec::new(),
            };
            let trust = match flow::load_trust_files(&mut term, &args.trust_files) {
                Ok(t) => t,
                Err(()) => return ExitCode::from(2),
            };
            let enrol = flow::Enrol { trust, ..enrol };
            flow::connect_real(
                &mut term,
                &host,
                &account,
                &http,
                (!args.sign_in_only).then_some(&enrol),
            )
        }
        Command::Status if args.demo => flow::status(&mut term, &host),
        Command::Status => status::show(&mut term, &host),
        Command::Revoke => setup_flow::revoke(
            &mut term,
            &host,
            args.dry_run,
            args.yes,
            args.device.as_deref(),
        ),
        Command::Setup => setup_flow::setup(
            &mut term,
            &host,
            &setup_flow::SetupArgs {
                dry_run: args.dry_run,
                yes: args.yes,
                trust_files: args.trust_files,
            },
        ),
        Command::Banner => flow::banner_only(&mut term),
        Command::Doctor => flow::doctor(&mut term, &host),
        Command::Help | Command::Version | Command::Daemon | Command::AgentShell => {
            unreachable!("handled above")
        }
    };
    // Restore the terminal (cursor, colours, raw mode) before any final message.
    drop(term);

    match result {
        Ok(Outcome::Done) => ExitCode::SUCCESS,
        Ok(Outcome::Declined | Outcome::Failed) => ExitCode::from(1),
        Err(term::Error::Interrupted) => {
            eprintln!("\n  stopped · nothing was changed");
            ExitCode::from(130)
        }
        // The reader went away (`stonewave | head`): nothing left to tell anyone.
        Err(term::Error::Io(e)) if e.kind() == std::io::ErrorKind::BrokenPipe => ExitCode::SUCCESS,
        Err(term::Error::Io(e)) => {
            eprintln!("stonewave: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn p(args: &[&str]) -> Result<Args, String> {
        parse(args.iter().map(|s| (*s).to_owned()))
    }

    #[test]
    fn default_is_connect() {
        assert_eq!(p(&[]).unwrap().command, Command::Connect);
    }

    #[test]
    fn commands_and_options() {
        let a = p(&[
            "status",
            "--host",
            "srv-1",
            "--os=Ubuntu 24.04",
            "--no-anim",
        ])
        .unwrap();
        assert_eq!(a.command, Command::Status);
        assert_eq!(a.host.as_deref(), Some("srv-1"));
        assert_eq!(a.os.as_deref(), Some("Ubuntu 24.04"));
        assert!(a.term.no_anim);
    }

    #[test]
    fn errors() {
        assert!(p(&["--host"]).is_err());
        assert!(p(&["--bogus"]).is_err());
        assert!(p(&["status", "revoke"]).is_err());
        assert!(p(&["--width", "wide"]).is_err());
        assert!(p(&["deploy"]).is_err());
    }

    #[test]
    fn connect_options() {
        let a = p(&["connect", "--account", "https://a.test"]).unwrap();
        assert_eq!(a.account.as_deref(), Some("https://a.test"));
        assert!(p(&["--demo"]).unwrap().demo);
        assert!(p(&["status", "--demo"]).unwrap().demo);
        assert!(p(&["doctor", "--demo"]).is_err());
        assert!(p(&["status", "--account", "https://a.test"]).is_err());
        assert!(p(&["--demo", "--account", "https://a.test"]).is_err());
        assert!(p(&["connect", "--account"]).is_err());
    }

    #[test]
    fn json_only_for_doctor() {
        let a = p(&["doctor", "--json"]).unwrap();
        assert_eq!(a.command, Command::Doctor);
        assert!(a.json);
        assert!(p(&["status", "--json"]).is_err());
        assert!(p(&["--json"]).is_err());
    }

    #[test]
    fn help_wins() {
        assert_eq!(p(&["revoke", "--help"]).unwrap().command, Command::Help);
    }
}
