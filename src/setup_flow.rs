//! The screens of `stonewave setup` and `stonewave revoke`: the plan first, then (after a yes) the work.
//!
//! The logic is in [`crate::setup`]; this file only shows it. Without a terminal to ask, every question is
//! answered no, so a script has to say `--yes` on purpose.

use crate::banner;
use crate::doctor;
use crate::flow::{Host, Outcome as FlowOutcome, ask};
use crate::realsys::RealSys;
use crate::setup::{self, Config, Outcome, StepReport, Sys, TrustKey};
use crate::style::{ColorMode, pal};
use crate::term::{Input, Result, Term};
use crate::ui::{DETAIL, INDENT};

pub struct SetupArgs {
    pub dry_run: bool,
    pub yes: bool,
    pub trust_files: Vec<String>,
}

fn row(m: ColorMode, r: &StepReport, dry: bool) -> Vec<String> {
    let name = format!("{:<15}", r.name);
    match &r.outcome {
        Outcome::Kept(text) => vec![format!(
            "{DETAIL}{} {}{}",
            m.fg_bold(pal::GREEN, "✓"),
            name,
            m.fg(pal::GREY, text)
        )],
        Outcome::Attention(text) => vec![format!(
            "{DETAIL}{} {}{}",
            m.fg_bold(pal::RED, "✗"),
            name,
            m.fg(pal::RED, text)
        )],
        Outcome::Changed(changes) => {
            let (icon, word) = if dry {
                (m.fg_bold(pal::AMBER, "◆"), "would change")
            } else {
                (m.fg_bold(pal::GREEN, "✓"), "changed")
            };
            let mut lines = vec![format!("{DETAIL}{icon} {name}{}", m.fg(pal::GREY, word))];
            for c in changes {
                lines.push(format!(
                    "{DETAIL}    {}",
                    m.fg(pal::GREY, &format!("· {}", c.0))
                ));
            }
            lines
        }
    }
}

fn show(term: &mut Term, reports: &[StepReport], dry: bool) {
    let m = term.mode();
    for r in reports {
        for line in row(m, r, dry) {
            term.line(&line);
        }
    }
}

fn load_trust(term: &mut Term, files: &[String]) -> std::result::Result<Vec<TrustKey>, ()> {
    let m = term.mode();
    let mut keys = Vec::new();
    for f in files {
        let text = match std::fs::read_to_string(f) {
            Ok(t) => t,
            Err(e) => {
                term.line(&format!(
                    "{INDENT}{}",
                    m.fg(pal::RED, &format!("cannot read {f}: {e}"))
                ));
                return Err(());
            }
        };
        match setup::parse_trust_file(&text) {
            Ok(k) => keys.extend(k),
            Err(e) => {
                term.line(&format!("{INDENT}{}", m.fg(pal::RED, &format!("{f}: {e}"))));
                return Err(());
            }
        }
    }
    Ok(keys)
}

fn needs_root(term: &mut Term, what: &str) {
    let m = term.mode();
    term.line(&format!(
        "{INDENT}{}",
        m.fg(
            pal::RED,
            &format!(
                "{what} changes this server and needs root. Run it with sudo, or add --dry-run to only look."
            )
        )
    ));
}

fn no_terminal(term: &mut Term) {
    if term.caps.input == Input::None {
        let m = term.mode();
        term.line(&format!(
            "{DETAIL}{}",
            m.fg(
                pal::GREY,
                "No terminal to ask, so the answer is no. Add --yes to go ahead without asking."
            )
        ));
    }
}

pub fn setup(term: &mut Term, host: &Host, args: &SetupArgs) -> Result<FlowOutcome> {
    let m = term.mode();
    term.blank();
    term.line(&banner::compact(m, &host.name));
    term.blank();
    let Ok(trust) = load_trust(term, &args.trust_files) else {
        return Ok(FlowOutcome::Failed);
    };
    let sys = RealSys;
    if !args.dry_run && sys.euid() != 0 {
        needs_root(term, "setup");
        return Ok(FlowOutcome::Declined);
    }
    let cfg = Config {
        version: crate::VERSION,
        trust,
    };
    let checks = doctor::run_all(&doctor::SYSTEM);

    // The plan: reads only.
    let plan = match setup::run_steps(&sys, true, &cfg, &checks, |_| {}) {
        Ok(p) => p,
        Err((_, e)) => {
            term.line(&format!("{INDENT}{}", m.fg(pal::RED, &e)));
            return Ok(FlowOutcome::Failed);
        }
    };
    term.line(&format!(
        "{INDENT}{}",
        m.bold(if args.dry_run {
            "Plan (dry run)"
        } else {
            "Plan"
        })
    ));
    show(term, &plan, true);
    term.blank();
    if plan
        .iter()
        .any(|r| matches!(r.outcome, Outcome::Attention(_)))
    {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(
                pal::RED,
                "Something needs a look first. Nothing was changed."
            )
        ));
        term.flush()?;
        return Ok(FlowOutcome::Failed);
    }
    if args.dry_run {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(
                pal::GREY,
                "Dry run: nothing was changed. Run without --dry-run to set it up."
            )
        ));
        term.flush()?;
        return Ok(FlowOutcome::Done);
    }
    if setup::has_changes(&plan) && !args.yes {
        term.line(&format!(
            "{DETAIL}{}",
            m.fg(
                pal::GREY,
                "Setup only changes what is listed above and keeps what is already in place."
            )
        ));
        if !ask(term, "Set this up?")? {
            term.line(&format!(
                "{INDENT}{}",
                m.fg(pal::GREY, "Nothing was changed.")
            ));
            no_terminal(term);
            term.flush()?;
            return Ok(FlowOutcome::Declined);
        }
        term.blank();
    }

    match setup::run_steps(&sys, false, &cfg, &checks, |_| {}) {
        Ok(done) => {
            term.line(&format!("{INDENT}{}", m.bold("Setup")));
            show(term, &done, false);
            term.blank();
            term.line(&format!(
                "{INDENT}{}",
                m.fg_bold(
                    pal::GREEN,
                    "Done. Run it again any time: it changes nothing that is already right."
                )
            ));
            term.flush()?;
            Ok(FlowOutcome::Done)
        }
        Err((done, e)) => {
            show(term, &done, false);
            term.blank();
            term.line(&format!(
                "{INDENT}{}",
                m.fg(
                    pal::RED,
                    &format!("Stopped: {e}. What ran is in place; running setup again is safe.")
                )
            ));
            term.flush()?;
            Ok(FlowOutcome::Failed)
        }
    }
}

pub fn revoke(
    term: &mut Term,
    host: &Host,
    dry_run: bool,
    yes: bool,
    device: Option<&str>,
) -> Result<FlowOutcome> {
    let m = term.mode();
    term.blank();
    term.line(&banner::compact(m, &host.name));
    term.blank();
    let sys = RealSys;
    if !dry_run && sys.euid() != 0 {
        needs_root(term, "revoke");
        return Ok(FlowOutcome::Declined);
    }
    let plan = match setup::revoke(&sys, true, device) {
        Ok(p) => p,
        Err(e) => {
            term.line(&format!("{INDENT}{}", m.fg(pal::RED, &e)));
            return Ok(FlowOutcome::Failed);
        }
    };
    let title = match device {
        Some(d) => format!("Revoke the admin device \"{d}\"?"),
        None => format!("Revoke all agent access on {}?", host.name),
    };
    term.line(&format!(
        "{INDENT}{} {}",
        m.fg_bold(pal::AMBER, "◆"),
        m.bold(&title)
    ));
    show(term, &plan, true);
    term.blank();
    if dry_run {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(pal::GREY, "Dry run: nothing was changed.")
        ));
        term.flush()?;
        return Ok(FlowOutcome::Done);
    }
    if !setup::has_changes(&plan) {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(pal::GREY, "Nothing to revoke: it is already closed.")
        ));
        term.flush()?;
        return Ok(FlowOutcome::Done);
    }
    if !yes && !ask(term, "Revoke?")? {
        term.line(&format!(
            "{INDENT}{}",
            m.fg(pal::GREY, "Nothing was changed.")
        ));
        no_terminal(term);
        term.flush()?;
        return Ok(FlowOutcome::Declined);
    }
    match setup::revoke(&sys, false, device) {
        Ok(done) => {
            show(term, &done, false);
            term.blank();
            term.line(&format!(
                "{INDENT}{}",
                m.fg_bold(pal::GREEN, "Agent access revoked. The audit log stays.")
            ));
            if device.is_none() {
                term.line(&format!(
                    "{INDENT}{}  {}",
                    m.fg(pal::CYAN, "sudo stonewave setup"),
                    m.fg(pal::GREY, "opens it again")
                ));
            }
            term.flush()?;
            Ok(FlowOutcome::Done)
        }
        Err(e) => {
            term.line(&format!(
                "{INDENT}{}",
                m.fg(pal::RED, &format!("Revoke stopped: {e}. Run it again."))
            ));
            term.flush()?;
            Ok(FlowOutcome::Failed)
        }
    }
}
