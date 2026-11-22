//! `stonewave agent status | rotate | disconnect | run`: everything after the connect.

use std::path::Path;

use serde_json::{Value, json};

use super::Ctx;
use super::dirs::Dirs;
use super::engine::{self, Engine, Unregistered};
use super::key::{self, Acl, Key, ReadError, read_key_file, scrub};
use super::state::{self, State};
use super::station;
use super::timefmt;
use super::ui::Ui;
use super::wave::{self, WaveError};
use crate::connect::Account;
use crate::flow::Outcome;
use crate::term::Result;

/// A key that runs out within this many days is called out.
pub const WARN_DAYS: i64 = 14;

/// Which agent is meant: the one named, or the only one there is.
pub fn pick_agent(
    dirs: &Dirs,
    wanted: Option<&str>,
    by_key: bool,
) -> std::result::Result<String, String> {
    if let Some(w) = wanted {
        if !crate::connect::is_agent_slug(w) {
            return Err(format!(
                "`{}` is not an agent name (lower case letters, digits and dashes)",
                scrub(w)
            ));
        }
        return Ok(w.to_owned());
    }
    let names: Vec<String> = if by_key {
        state::list_keys(&dirs.config)
    } else {
        state::list(&dirs.config).into_iter().map(|x| x.0).collect()
    };
    match names.as_slice() {
        [] => Err(
            "no agent is connected on this machine; `stonewave agent connect` connects one"
                .to_owned(),
        ),
        [one] => Ok(one.clone()),
        many => Err(format!(
            "several agents are connected here ({}); say which one with --agent <name>",
            many.join(", ")
        )),
    }
}

fn wave_of(st: &State, flag: Option<&str>) -> std::result::Result<Account, String> {
    Account::parse_as("Wave", flag.unwrap_or(&st.wave_url)).map_err(|e| e.to_string())
}

fn load_state(dirs: &Dirs, agent: &str) -> std::result::Result<State, String> {
    let path = dirs.state_file(agent);
    if !path.exists() {
        let key_hint = if dirs.key_file(agent).exists() {
            format!(
                " (the key file {} is there; delete it by hand and ask a Wave admin to stop the agent)",
                dirs.key_file(agent).display()
            )
        } else {
            String::new()
        };
        return Err(format!(
            "`{agent}` is not connected here: {} does not exist{key_hint}",
            path.display()
        ));
    }
    State::load(&path)
}

// ------------------------------------------------------------------ status

fn service_text(st: &State, ctx_exec: &dyn super::exec::Exec) -> (&'static str, Value) {
    match &st.service {
        None => (
            "not installed",
            json!({"installed": false, "active": null, "kind": null}),
        ),
        Some(rec) => {
            let s = station::status(rec, ctx_exec);
            let text = match (s.installed, s.active) {
                (false, _) => "recorded, but its file is gone",
                (true, Some(true)) if rec.kind == "schtasks" => "scheduled at logon",
                (true, Some(true)) => "running",
                (true, Some(false)) => "installed, not running",
                (true, None) => "installed (state unknown)",
            };
            (
                text,
                json!({"installed": s.installed, "active": s.active, "kind": rec.kind, "name": rec.name}),
            )
        }
    }
}

pub struct Report {
    pub json: Value,
    pub lines: Vec<(String, String)>,
    pub warnings: Vec<String>,
}

/// Collects everything `status` shows. Never fails: what cannot be found is reported as missing or unknown.
pub fn report(ctx: &Ctx<'_>, st: &State, wave_override: Option<&str>) -> Report {
    let dirs = ctx.dirs;
    let key_path = dirs.key_file(&st.agent);
    let key = read_key_file(&key_path);
    let mut warnings = Vec::new();
    let private = key::is_private(&key_path);
    let key_json = json!({
        "path": super::dirs::text(&key_path),
        "exists": !matches!(key, Err(ReadError::Missing)),
        "readable": key.is_ok(),
        "mode": key::mode_text(&key_path),
        "private": private,
    });
    let key_line = match (&key, private) {
        (Err(e), _) => e.to_string(),
        (Ok(_), Some(false)) => {
            warnings.push(format!(
                "the key file {} can be read by other users; run `chmod 600` on it",
                key_path.display()
            ));
            format!("{} · readable by others!", key_path.display())
        }
        (Ok(_), _) => format!("{} · only you can read it", key_path.display()),
    };
    let engine = Engine::parse(&st.engine);
    let entry = engine.and_then(|e| engine::entry_present(e, dirs));
    let entry_text = match entry {
        Some(true) => "present",
        Some(false) => "missing",
        None => "unknown",
    };
    let (service_line, service_json) = service_text(st, ctx.exec);

    // Wave itself
    let mut remote_json = json!({"reachable": false, "error": null, "status": null});
    let remote_line: String;
    let mut expires = st.key_expires_at.clone();
    match (&key, wave_of(st, wave_override)) {
        (Ok(k), Ok(w)) => {
            let http = (ctx.http_for)(&w);
            match wave::status(&*http, &w, k) {
                Ok(r) => {
                    expires = r.key_expires_at.clone();
                    remote_line = format!(
                        "reachable · last seen {}{}",
                        r.last_seen_at.as_deref().unwrap_or("never"),
                        r.last_session
                            .as_deref()
                            .map_or(String::new(), |s| format!(" · {s}"))
                    );
                    remote_json = json!({
                        "reachable": true, "error": null,
                        "status": {
                            "agent": {"id": r.agent.id, "key": r.agent.key, "name": r.agent.name},
                            "team": {"key": r.team.key, "name": r.team.name},
                            "company": r.company, "engine": r.engine,
                            "key_expires_at": r.key_expires_at,
                            "last_seen_at": r.last_seen_at, "last_session": r.last_session,
                        }
                    });
                }
                Err(e) => {
                    remote_line = if e.is_unauthorized() {
                        warnings.push("Wave no longer accepts this key (revoked or expired); `stonewave agent connect` makes a new one".to_owned());
                        "Wave refused the key".to_owned()
                    } else {
                        format!("not reachable: {e}")
                    };
                    remote_json =
                        json!({"reachable": false, "error": e.to_string(), "status": null});
                }
            }
        }
        (Err(_), _) => remote_line = "not asked (no readable key)".to_owned(),
        (_, Err(e)) => remote_line = format!("not asked ({e})"),
    }
    let days = timefmt::days_until(&expires, timefmt::now_secs());
    let expires_line = match days {
        Some(d) if d < 0 => {
            warnings.push(
                "the key has expired; run `stonewave agent connect` for a new one".to_owned(),
            );
            format!("{} · expired", timefmt::date_of(&expires))
        }
        Some(d) => {
            if d <= WARN_DAYS {
                warnings.push(format!(
                    "the key runs out in {d} days; `stonewave agent rotate` renews it"
                ));
            }
            format!("{} · in {d} days", timefmt::date_of(&expires))
        }
        None => timefmt::date_of(&expires),
    };
    if entry == Some(false) {
        warnings.push(format!(
            "the `wave` entry is missing in {}; `stonewave agent connect` adds it again",
            engine.map_or("the engine", Engine::label)
        ));
    }
    if st.service.is_some() && service_json["active"].as_bool() == Some(false) {
        warnings.push("the wake service is not running".to_owned());
    }
    let json = json!({
        "stonewave": crate::VERSION,
        "connected": true,
        "agent": {"key": st.agent, "name": st.agent_name, "id": st.agent_id},
        "team": {"key": st.team, "name": st.team_name},
        "company": st.company,
        "engine": st.engine,
        "wave_url": st.wave_url,
        "mcp_url": st.mcp_url,
        "created": st.created,
        "key_expires_at": expires,
        "key_days_left": days,
        "local": {
            "state_file": super::dirs::text(&dirs.state_file(&st.agent)),
            "key_file": key_json,
            "engine_entry": entry_text,
            "service": service_json,
            "wake_log": st.wake_log,
        },
        "remote": remote_json,
        "warnings": warnings,
    });
    let lines = vec![
        (
            "Agent".to_owned(),
            format!("{} ({})", st.agent_name, st.agent),
        ),
        (
            "Team".to_owned(),
            format!("{} · {}", st.team_name, st.company),
        ),
        ("Engine".to_owned(), st.engine.clone()),
        ("Key file".to_owned(), key_line),
        ("Key ends".to_owned(), expires_line),
        ("Wave entry".to_owned(), entry_text.to_owned()),
        ("Wake".to_owned(), service_line.to_owned()),
        ("Wave".to_owned(), remote_line),
    ];
    Report {
        json,
        lines,
        warnings,
    }
}

pub fn status(
    ctx: &Ctx<'_>,
    ui: &mut dyn Ui,
    agent: Option<&str>,
    wave: Option<&str>,
) -> Result<Outcome> {
    let name = match pick_agent(ctx.dirs, agent, false) {
        Ok(n) => n,
        Err(m) => {
            ui.note(&format!("Not connected: {m}."));
            ui.flush()?;
            // a report could be made: it says "nothing here"
            return Ok(Outcome::Done);
        }
    };
    let st = match load_state(ctx.dirs, &name) {
        Ok(s) => s,
        Err(m) => {
            ui.fail(&m);
            ui.flush()?;
            return Ok(Outcome::Done);
        }
    };
    let r = report(ctx, &st, wave);
    ui.heading(&format!("Wave agent {}", st.agent_name));
    for (k, v) in &r.lines {
        ui.kv(k, v);
    }
    for w in &r.warnings {
        ui.warn(w);
    }
    ui.flush()?;
    Ok(Outcome::Done)
}

/// `status --json`: one object on one line. No secrets.
pub fn status_json(ctx: &Ctx<'_>, agent: Option<&str>, wave: Option<&str>) -> String {
    let none = |why: String| {
        json!({"stonewave": crate::VERSION, "connected": false, "reason": scrub(&why)}).to_string()
    };
    let name = match pick_agent(ctx.dirs, agent, false) {
        Ok(n) => n,
        Err(m) => return none(m),
    };
    match load_state(ctx.dirs, &name) {
        Ok(st) => report(ctx, &st, wave).json.to_string(),
        Err(m) => none(m),
    }
}

// ------------------------------------------------------------------ rotate

pub fn rotate(
    ctx: &Ctx<'_>,
    ui: &mut dyn Ui,
    agent: Option<&str>,
    wave: Option<&str>,
) -> Result<Outcome> {
    let acl = Acl {
        exec: ctx.exec,
        user: &ctx.dirs.win_user,
    };
    let name = match pick_agent(ctx.dirs, agent, false) {
        Ok(n) => n,
        Err(m) => return fail(ui, "Cannot rotate", &m),
    };
    let mut st = match load_state(ctx.dirs, &name) {
        Ok(s) => s,
        Err(m) => return fail(ui, "Cannot rotate", &m),
    };
    let key_path = ctx.dirs.key_file(&name);
    let old = match read_key_file(&key_path) {
        Ok(k) => k,
        Err(e) => {
            return fail(
                ui,
                "Cannot rotate",
                &format!("{e} ({})", key_path.display()),
            );
        }
    };
    let w = match wave_of(&st, wave) {
        Ok(w) => w,
        Err(m) => return fail(ui, "Cannot rotate", &m),
    };
    // Wave revokes the old key with this call, so the new one must be writable: look first
    if let Err(e) = key::check_writable(&ctx.dirs.config) {
        return fail(
            ui,
            "Cannot rotate",
            &format!("{} is not writable: {e}", ctx.dirs.config.display()),
        );
    }
    let http = (ctx.http_for)(&w);
    let (new, expires) = match wave::rotate(&*http, &w, &old) {
        Ok(x) => x,
        Err(e) => {
            let extra = if e.is_unauthorized() {
                "The key is not valid any more; `stonewave agent connect` makes a new one."
            } else {
                "Nothing was changed."
            };
            return fail(ui, "Wave did not rotate the key", &format!("{e}. {extra}"));
        }
    };
    match key::write_private_file(&key_path, new.expose().as_bytes(), &acl) {
        Ok(None) => {}
        Ok(Some(w)) => ui.warn(&w),
        Err(e) => {
            ui.fail("Wave made a new key, but it could not be saved");
            ui.note(&format!("{}: {e}", key_path.display()));
            ui.note("The old key is already revoked. Run `stonewave agent connect` to get a working key.");
            ui.flush()?;
            return Ok(Outcome::Failed);
        }
    }
    st.key_expires_at = expires.clone();
    if let Err(e) = st.save(&ctx.dirs.state_file(&name)) {
        ui.warn(&format!("Could not update the state file: {e}"));
    }
    ui.ok(
        "Key rotated",
        &format!(
            "new key in {} · ends {}",
            key_path.display(),
            timefmt::date_of(&expires)
        ),
    );
    let mut all_well = true;
    if let Some(rec) = &st.service {
        match station::restart(rec, ctx.exec) {
            Ok(()) => ui.ok("Wake service", "restarted with the new key"),
            Err(e) => {
                all_well = false;
                ui.warn(&format!("The wake service could not be restarted: {e}. Restart it by hand; it still has the old key."));
            }
        }
    }
    ui.note("Sessions that are already running keep the old key in their environment: restart them with `stonewave agent run`.");
    ui.flush()?;
    Ok(if all_well {
        Outcome::Done
    } else {
        Outcome::Failed
    })
}

fn fail(ui: &mut dyn Ui, what: &str, detail: &str) -> Result<Outcome> {
    ui.fail(what);
    ui.note(detail);
    ui.flush()?;
    Ok(Outcome::Failed)
}

// ------------------------------------------------------------------ disconnect

pub struct DisconnectOpts<'a> {
    pub agent: Option<&'a str>,
    pub wave: Option<&'a str>,
    pub yes: bool,
    pub keep_remote: bool,
}

pub fn disconnect(ctx: &Ctx<'_>, ui: &mut dyn Ui, o: &DisconnectOpts<'_>) -> Result<Outcome> {
    let name = match pick_agent(ctx.dirs, o.agent, false) {
        Ok(n) => n,
        Err(m) => return fail(ui, "Cannot disconnect", &m),
    };
    let st = match load_state(ctx.dirs, &name) {
        Ok(s) => s,
        Err(m) => return fail(ui, "Cannot disconnect", &m),
    };
    ui.heading(&format!(
        "Disconnect {} from {}?",
        st.agent_name, st.company
    ));
    ui.note(if o.keep_remote {
        "Wave is NOT told: the key stays valid there until someone revokes it."
    } else {
        "Wave revokes the key first, then the files, the `wave` entry and the wake service made by `agent connect` are removed."
    });
    if !o.yes {
        if !ui.can_ask() {
            ui.note("Nobody to ask: run with --yes to do it.");
            ui.flush()?;
            return Ok(Outcome::Declined);
        }
        if !ui.ask("Disconnect?")? {
            ui.note("Nothing was changed.");
            ui.flush()?;
            return Ok(Outcome::Declined);
        }
    }

    let key_path = ctx.dirs.key_file(&name);
    // ---- Wave first: with the key gone from this machine nobody could revoke it any more
    if o.keep_remote {
        ui.warn("The key was not revoked in Wave and still works there. Revoke it in the Wave app, or run `disconnect` again with a connection.");
    } else {
        match read_key_file(&key_path) {
            Err(e) => {
                return fail(
                    ui,
                    "Cannot tell Wave to revoke the key",
                    &format!(
                        "{e}. Use --keep-remote to clean up here anyway, and have a Wave admin stop the agent."
                    ),
                );
            }
            Ok(k) => {
                let w = match wave_of(&st, o.wave) {
                    Ok(w) => w,
                    Err(m) => return fail(ui, "Cannot tell Wave to revoke the key", &m),
                };
                let http = (ctx.http_for)(&w);
                match wave::revoke(&*http, &w, &k) {
                    Ok(n) => ui.ok("Key revoked in Wave", &format!("{n} key(s)")),
                    Err(e) if e.is_unauthorized() => {
                        ui.ok("Key revoked in Wave", "it was not valid any more");
                    }
                    Err(e) => {
                        let hint = match &e {
                            WaveError::Network(_) => {
                                "No connection: nothing was removed. Try again with a connection, or use --keep-remote to clean up here only."
                            }
                            _ => "Nothing was removed. Use --keep-remote to clean up here only.",
                        };
                        return fail(ui, "Wave did not revoke the key", &format!("{e}. {hint}"));
                    }
                }
            }
        }
    }

    // ---- exactly what was recorded
    let mut problems = 0;
    if let Some(entry) = &st.mcp_entry {
        match Engine::parse(&entry.kind) {
            Some(e) => {
                let program = e.program().and_then(|p| ctx.exec.which(p));
                match engine::unregister(e, ctx.exec, program.as_deref(), ctx.dirs) {
                    Ok(Unregistered::Removed) => ui.ok(
                        "Engine entry",
                        &format!("`wave` removed from {}", e.label()),
                    ),
                    Ok(Unregistered::NotThere) => ui.ok(
                        "Engine entry",
                        &format!("`wave` was not in {} any more", e.label()),
                    ),
                    Err(m) => {
                        problems += 1;
                        ui.fail(&format!("Could not remove `wave` from {}", e.label()));
                        ui.note(&m);
                    }
                }
            }
            None => ui.warn(&format!(
                "Unknown engine `{}` in the state file; its entry was left alone.",
                scrub(&entry.kind)
            )),
        }
    }
    if let Some(rec) = &st.service {
        let r = station::uninstall(rec, ctx.exec);
        for p in &r.problems {
            problems += 1;
            ui.fail(p);
        }
        if r.problems.is_empty() {
            ui.ok("Wake service", "stopped and removed");
        }
    }
    if let Some(log) = &st.wake_log {
        match key::remove_if_there(Path::new(log)) {
            Ok(_) => ui.ok("Wake log", "removed"),
            Err(e) => {
                problems += 1;
                ui.fail(&format!("Could not remove {log}: {e}"));
            }
        }
    }
    match key::remove_if_there(&key_path) {
        Ok(true) => ui.ok("Key file", "removed"),
        Ok(false) => ui.ok("Key file", "was not there"),
        Err(e) => {
            problems += 1;
            ui.fail(&format!("Could not remove {}: {e}", key_path.display()));
        }
    }
    // the wake script is shared: it goes with the last agent
    if let Some(script) = &st.script {
        let others = state::list(&ctx.dirs.config)
            .into_iter()
            .filter(|(n, _)| *n != name)
            .filter_map(|(_, p)| State::load(&p).ok())
            .any(|s| s.script.is_some());
        if !others {
            let _ = key::remove_if_there(Path::new(script));
        }
    }
    if problems > 0 {
        ui.warn(&format!(
            "{problems} thing(s) could not be removed. The state file stays, so `stonewave agent disconnect` can be run again."
        ));
        ui.flush()?;
        return Ok(Outcome::Failed);
    }
    if let Err(e) = key::remove_if_there(&ctx.dirs.state_file(&name)) {
        ui.fail(&format!("Could not remove the state file: {e}"));
        ui.flush()?;
        return Ok(Outcome::Failed);
    }
    ui.blank();
    ui.heading("Disconnected");
    ui.flush()?;
    Ok(Outcome::Done)
}

// ------------------------------------------------------------------ run

/// What `agent run` needs: the key and the program to start. The key goes into the child's environment and nowhere else.
#[derive(Debug)]
pub struct RunPlan {
    pub program: std::path::PathBuf,
    pub args: Vec<String>,
    pub key: Key,
    pub warning: Option<String>,
}

pub fn plan_run(
    dirs: &Dirs,
    exec: &dyn super::exec::Exec,
    agent: Option<&str>,
    cmd: &[String],
) -> std::result::Result<RunPlan, String> {
    let name = pick_agent(dirs, agent, true)?;
    let Some((first, rest)) = cmd.split_first() else {
        return Err(
            "no command given: `stonewave agent run [--agent <name>] -- <command> [arguments]`"
                .to_owned(),
        );
    };
    let key_path = dirs.key_file(&name);
    let key = read_key_file(&key_path).map_err(|e| format!("{e} ({})", key_path.display()))?;
    let program = if first.contains(['/', '\\']) {
        std::path::PathBuf::from(first)
    } else {
        exec.which(first)
            .ok_or_else(|| format!("`{}` was not found on your PATH", scrub(first)))?
    };
    let warning = (key::is_private(&key_path) == Some(false)).then(|| {
        format!(
            "the key file {} can be read by other users; run `chmod 600` on it",
            key_path.display()
        )
    });
    Ok(RunPlan {
        program,
        args: rest.to_vec(),
        key,
        warning,
    })
}

#[cfg(test)]
mod tests {
    use super::super::exec::fake::Fake;
    use super::super::key::testdir::TempDir;
    use super::super::state::sample;
    use super::*;

    #[test]
    fn pick_agent_rules() {
        let t = TempDir::new("pick");
        let dirs = Dirs::under(t.path());
        std::fs::create_dir_all(&dirs.config).unwrap();
        assert!(
            pick_agent(&dirs, None, false)
                .unwrap_err()
                .contains("no agent")
        );
        sample("agent-a").save(&dirs.state_file("agent-a")).unwrap();
        assert_eq!(pick_agent(&dirs, None, false).unwrap(), "agent-a");
        sample("agent-b").save(&dirs.state_file("agent-b")).unwrap();
        let e = pick_agent(&dirs, None, false).unwrap_err();
        assert!(
            e.contains("agent-a, agent-b") && e.contains("--agent"),
            "{e}"
        );
        assert_eq!(
            pick_agent(&dirs, Some("agent-b"), false).unwrap(),
            "agent-b"
        );
        assert!(pick_agent(&dirs, Some("../x"), false).is_err());
    }

    #[test]
    fn run_plan_reads_the_key_and_resolves_the_program() {
        let t = TempDir::new("plan");
        let dirs = Dirs::under(t.path());
        std::fs::create_dir_all(&dirs.config).unwrap();
        let fake = Fake::new(&["claude"]);
        let cmd = vec!["claude".to_owned(), "--resume".to_owned()];
        assert!(
            plan_run(&dirs, &fake, None, &cmd)
                .unwrap_err()
                .contains("no agent")
        );
        let key = "wak_0a1b2c3d-0000-4000-8000-000000000001_s3cretS3cretS3cret";
        std::fs::write(dirs.key_file("agent-a"), format!("{key}\n")).unwrap();
        let plan = plan_run(&dirs, &fake, None, &cmd).unwrap();
        assert_eq!(plan.key.expose(), key);
        assert_eq!(plan.args, vec!["--resume"]);
        assert_eq!(plan.program, std::path::PathBuf::from("/fake/bin/claude"));
        assert!(
            plan_run(&dirs, &fake, None, &[])
                .unwrap_err()
                .contains("no command")
        );
        assert!(
            plan_run(&dirs, &fake, None, &["nope".to_owned()])
                .unwrap_err()
                .contains("not found")
        );
        let e = plan_run(&dirs, &fake, Some("agent-zzz"), &cmd).unwrap_err();
        assert!(e.contains("key file is missing"), "{e}");
        assert!(!format!("{:?}", plan.key).contains("s3cret"));
    }
}
