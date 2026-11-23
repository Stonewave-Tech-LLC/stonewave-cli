//! `stonewave agent connect`: sign in with a device code, get the agent key from Wave, keep it in a private file, tell the engine
//! about Wave (without the key) and, if wanted, start the wake service.
//!
//! The order matters. Everything that could stop the run for a boring reason (no program for the engine, no Wave address, no
//! writable folder, no answer to the question about the service) happens before the key is made. After that the key goes into its
//! file first, and every later step reports on its own: a failed step never loses the key, and `disconnect` knows what exists.

use std::time::Duration;

use super::Ctx;
use super::dirs::Os;
use super::engine::{self, Engine, Registered};
use super::key::{Acl, check_writable, ensure_private_dir, write_private_file};
use super::state::{EntryRec, State};
use super::station;
use super::timefmt;
use super::ui::Ui;
use super::wave::{self, MintRequest, WaveError};
use crate::connect::{Account, MAX_FAILURES, Purpose, Wait, Want};
use crate::flow::Outcome;
use crate::term::Result;

pub struct ConnectOpts {
    pub engine: Option<Engine>,
    pub company: Option<String>,
    pub name: Option<String>,
    pub no_station: bool,
    pub yes: bool,
    pub account: Account,
    /// `--wave` or `STONEWAVE_WAVE_URL`, not yet parsed.
    pub wave: Option<String>,
    /// `--no-browser`: only show the link and the code.
    pub no_browser: bool,
    /// The sign-in and waiting lines in German.
    pub german: bool,
    /// Reads an environment variable (tests replace it).
    pub env: fn(&str) -> Option<String>,
}

pub fn real_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// `en`, or `de` when the screens are German.
fn t(german: bool, en: &str, de: &str) -> String {
    if german { de } else { en }.to_owned()
}

fn mmss(d: Duration) -> String {
    format!("{}:{:02}", d.as_secs() / 60, d.as_secs() % 60)
}

/// Opens the link in the person's browser, if there is one to open: not on a server over ssh with no display. The link is made from the
/// account's own address here, never run through a shell.
fn open_browser(ctx: &Ctx<'_>, o: &ConnectOpts, url: &str) -> bool {
    if o.no_browser || !url.starts_with("https://") && !url.starts_with("http://127.0.0.1") {
        return false;
    }
    let (program, args): (&str, Vec<String>) = match ctx.dirs.os {
        Os::Mac => ("open", vec![url.to_owned()]),
        Os::Windows => (
            "rundll32",
            vec!["url.dll,FileProtocolHandler".to_owned(), url.to_owned()],
        ),
        Os::Linux => {
            if (o.env)("DISPLAY").is_none() && (o.env)("WAYLAND_DISPLAY").is_none() {
                return false;
            }
            ("xdg-open", vec![url.to_owned()])
        }
    };
    ctx.exec.start_detached(program, &args).is_ok()
}

fn stop(ui: &mut dyn Ui, what: &str, detail: &str, unchanged: &str) -> Result<Outcome> {
    ui.fail(what);
    if !detail.is_empty() {
        ui.note(detail);
    }
    if !unchanged.is_empty() {
        ui.note(unchanged);
    }
    ui.flush()?;
    Ok(Outcome::Failed)
}

/// Which engine, when `--engine` was not given.
fn choose_engine(
    ctx: &Ctx<'_>,
    ui: &mut dyn Ui,
    yes: bool,
) -> Result<std::result::Result<Engine, String>> {
    let found = engine::detect(ctx.exec);
    match found.as_slice() {
        [] => {
            ui.note("No Claude Code, Codex or Grok found on your PATH: setting up for another agent (the address is shown at the end).");
            Ok(Ok(Engine::Other))
        }
        [one] => {
            ui.note(&format!("Found {}.", one.label()));
            Ok(Ok(*one))
        }
        many => {
            let names: Vec<&str> = many.iter().map(|e| e.name()).collect();
            if !yes && ui.can_ask() {
                for e in many {
                    if ui.ask(&format!("Connect {}?", e.label()))? {
                        return Ok(Ok(*e));
                    }
                }
            }
            Ok(Err(format!(
                "Several agents were found ({}). Say which one with --engine.",
                names.join(", ")
            )))
        }
    }
}

pub fn connect(ctx: &Ctx<'_>, ui: &mut dyn Ui, o: &ConnectOpts) -> Result<Outcome> {
    let acl = Acl {
        exec: ctx.exec,
        user: &ctx.dirs.win_user,
    };
    ui.banner()?;
    ui.kv("host", &format!("{} ({})", ctx.host, ctx.os));
    ui.kv("to", &o.account.host_text());
    ui.blank();

    // ---- everything that can be checked before anybody is asked to sign in
    let wave_flag = match o.wave.as_deref().map(|w| Account::parse_as("Wave", w)) {
        Some(Ok(a)) => Some(a),
        Some(Err(e)) => {
            return stop(
                ui,
                "The Wave address is not usable",
                &e.to_string(),
                "Nothing was changed.",
            );
        }
        None => None,
    };
    let mut engine = match o.engine {
        Some(e) => e,
        None => match choose_engine(ctx, ui, o.yes)? {
            Ok(e) => e,
            Err(m) => return stop(ui, "Which agent?", &m, "Nothing was changed."),
        },
    };
    let mut program = match engine::locate(engine, ctx.exec) {
        Ok(p) => p,
        Err(m) => {
            return stop(
                ui,
                &format!("Cannot connect {}", engine.label()),
                &m,
                "Nothing was changed.",
            );
        }
    };

    // ---- sign in
    let http = (ctx.http_for)(&o.account);
    let want = Want {
        engine: engine.name().to_owned(),
        agent_name: o.name.clone(),
        company: o.company.clone(),
    };
    let started =
        match crate::connect::start_with(&*http, &o.account, ctx.host, ctx.os, Some(&want)) {
            Ok(s) => s,
            Err(e) => {
                return stop(
                    ui,
                    "Could not start the sign-in",
                    &e.to_string(),
                    "Nothing was changed.",
                );
            }
        };
    let mark = ui.mark();
    let de = o.german;
    let (link, prefilled) = started.link(&o.account);
    ui.heading(&t(
        de,
        "Sign in to connect this agent",
        "Im Browser anmelden, um den Agenten zu verbinden",
    ));
    ui.kv(&t(de, "Open", "Seite"), link.trim_start_matches("https://"));
    ui.kv("Code", &started.user_code);
    ui.kv(&t(de, "Words", "Wörter"), &started.words.join(" · "));
    let opened = open_browser(ctx, o, &link);
    ui.note(&match (opened, prefilled) {
        (true, true) => t(
            de,
            "Your browser is open, the code is already filled in.",
            "Dein Browser ist offen, der Code ist schon eingetragen.",
        ),
        (true, false) => t(
            de,
            "Your browser is open. Type the code on the page.",
            "Dein Browser ist offen. Gib dort den Code ein.",
        ),
        (false, _) => t(
            de,
            "Open the link on any device with a browser and type the code.",
            "Öffne den Link auf einem beliebigen Gerät mit Browser und gib den Code ein.",
        ),
    });
    ui.note(&t(
        de,
        "The page shows the same four words. Confirm only if they match and you started this yourself.",
        "Die Seite zeigt dieselben vier Wörter. Bestätige nur, wenn sie gleich sind und du das selbst gestartet hast.",
    ));
    ui.blank();
    ui.flush()?;
    let waited = {
        let mut pace = ui.pace();
        crate::connect::wait_for_approval_as(
            &*http,
            &o.account,
            &started,
            &mut *pace,
            Purpose::Agent,
        )
    };
    ui.finish_wait(mark);
    let waited = match waited? {
        Ok(w) => w,
        Err(e) => {
            return stop(
                ui,
                "Lost the connection to the account",
                &e.to_string(),
                "Nothing was changed.",
            );
        }
    };
    match waited.end {
        Wait::Denied => {
            ui.fail("Not connected: the sign-in was declined");
            ui.note("Nothing was changed.");
            ui.flush()?;
            return Ok(Outcome::Declined);
        }
        Wait::Expired => {
            return stop(
                ui,
                "The code ran out",
                "Run `stonewave agent connect` again for a new code.",
                "Nothing was changed.",
            );
        }
        Wait::Approved => {}
    }
    let approved = waited.approved.expect("approved comes with the answer");
    let grant = approved
        .agent
        .clone()
        .expect("an agent sign-in comes with the agent fields");
    ui.ok(
        "Signed in",
        &format!(
            "{} · team {} · {}",
            approved.company_name, grant.team_name, grant.agent_name
        ),
    );

    // what the person confirmed on the page wins over what the command line suggested
    if let Some(confirmed) = Engine::parse(&grant.engine) {
        if confirmed != engine {
            ui.note(&format!(
                "Wave has {} for this agent (the command line said {}): setting up {}.",
                confirmed.label(),
                engine.label(),
                confirmed.label()
            ));
            engine = confirmed;
            program = match engine::locate(engine, ctx.exec) {
                Ok(p) => p,
                Err(m) => {
                    return stop(
                        ui,
                        &format!("Cannot connect {}", engine.label()),
                        &m,
                        "No key was made and nothing was changed. This sign-in is used up; run connect again.",
                    );
                }
            };
        }
    }

    // ---- where is Wave?
    let wave_addr = match (&wave_flag, grant.wave_url.as_deref()) {
        (Some(a), _) => a.clone(),
        (None, Some(u)) => match Account::parse_as("Wave", u) {
            Ok(a) => a,
            Err(e) => {
                return stop(
                    ui,
                    "The address of Wave that the account sent is not usable",
                    &e.to_string(),
                    "No key was made and nothing was changed. Run connect again with --wave <address>.",
                );
            }
        },
        (None, None) => {
            return stop(
                ui,
                "Neither the account nor you said where Wave is",
                "Run again with --wave <address> (or set STONEWAVE_WAVE_URL).",
                "No key was made and nothing was changed. This sign-in is used up; the next run shows a new code.",
            );
        }
    };

    // ---- the folder for the key
    if let Err(e) =
        ensure_private_dir(&ctx.dirs.config, &acl).and_then(|()| check_writable(&ctx.dirs.config))
    {
        return stop(
            ui,
            "Cannot prepare the folder for the key",
            &format!("{}: {e}", ctx.dirs.config.display()),
            "No key was made and nothing was changed. This sign-in is used up; the next run shows a new code.",
        );
    }

    // ---- the wake service: decided now, done later
    let mut station_note = None;
    let mut node = None;
    if !o.no_station {
        match station::find_node(ctx.exec) {
            Err(why) => {
                station_note = Some(format!(
                    "The wake service was not set up: {why}. The agent works without it; install Node.js 18 or newer and run `stonewave agent connect` again to add it."
                ));
            }
            Ok(path) => {
                ui.blank();
                ui.heading("Wake service");
                ui.note(
                    "A small background program, under your user (never root), asks Wave what was",
                );
                ui.note(&format!(
                    "addressed to the agent and writes one line per new thing to {}.",
                    ctx.dirs.wake_log(&grant.agent).display()
                ));
                let yes = if o.yes {
                    true
                } else if ui.can_ask() {
                    ui.ask("Set it up?")?
                } else {
                    station_note = Some("The wake service was not set up (nobody to ask). Run with --yes to set it up.".to_owned());
                    false
                };
                if yes {
                    node = Some(path);
                } else if station_note.is_none() {
                    station_note = Some("The wake service was not set up.".to_owned());
                }
            }
        }
    }

    // ---- the key
    ui.blank();
    ui.note("Asking Wave for the key ...");
    ui.flush()?;
    let wave_http = (ctx.http_for)(&wave_addr);
    let req = MintRequest {
        host: ctx.host,
        os: ctx.os,
        engine: engine.name(),
    };
    let mut slept = Duration::ZERO;
    let mut not_ready_for = Duration::ZERO;
    let mut told_admin = false;
    let mut last_not_ready;
    let mut net_failures = 0;
    let minted = loop {
        let e = match wave::mint(&*wave_http, &wave_addr, &approved.token, &req) {
            Ok(m) => break m,
            Err(e) => e,
        };
        last_not_ready = false;
        let (pause, msg) = match &e {
            WaveError::Api { code, .. } if code == "agent_not_ready" => {
                net_failures = 0;
                last_not_ready = true;
                not_ready_for += ctx.timing.mint_retry;
                if not_ready_for >= ctx.timing.not_ready_notice {
                    if !told_admin {
                        told_admin = true;
                        ui.note(&t(
                            de,
                            "This takes longer than usual: Wave is waiting for an admin to apply the plan. You can wait here, or press Ctrl-C and sign in again later (your confirmation stays valid).",
                            "Das dauert länger als üblich: Wave wartet auf einen Admin, der den Plan anwendet. Du kannst hier warten oder Strg-C drücken und dich später noch einmal anmelden (deine Bestätigung bleibt gültig).",
                        ));
                    }
                    (
                        ctx.timing.mint_retry,
                        t(
                            de,
                            "Wave is waiting for an admin to apply the plan",
                            "Wave wartet auf einen Admin, der den Plan anwendet",
                        ),
                    )
                } else {
                    (
                        ctx.timing.mint_retry,
                        t(
                            de,
                            "Wave is setting up the agent",
                            "Wave richtet den Agenten ein",
                        ),
                    )
                }
            }
            WaveError::Api {
                status: 429,
                retry_after,
                ..
            } => {
                net_failures = 0;
                (
                    Duration::from_secs(retry_after.unwrap_or(30).clamp(1, 120)),
                    t(
                        de,
                        "Wave asks to wait a moment",
                        "Wave bittet um einen Moment Geduld",
                    ),
                )
            }
            WaveError::Network(_) if net_failures + 1 < MAX_FAILURES => {
                net_failures += 1;
                (
                    ctx.timing.mint_retry,
                    t(
                        de,
                        "No answer from Wave, trying again",
                        "Keine Antwort von Wave, ich versuche es noch einmal",
                    ),
                )
            }
            _ => {
                return stop(
                    ui,
                    "Wave did not give a key",
                    &e.to_string(),
                    "Nothing was changed on this machine.",
                );
            }
        };
        if slept + pause > ctx.timing.mint_max_wait {
            if last_not_ready && told_admin {
                return stop(
                    ui,
                    &t(
                        de,
                        "Wave did not finish setting up the agent",
                        "Wave hat den Agenten nicht fertig eingerichtet",
                    ),
                    &t(
                        de,
                        "Wave is waiting for an admin to apply the plan. Tell an admin of the company.",
                        "Wave wartet auf einen Admin, der den Plan anwendet. Sag einem Admin der Firma Bescheid.",
                    ),
                    &t(
                        de,
                        "Your confirmation stays valid on the account side. When the plan is applied, run `stonewave agent connect` again (a new code, the same choice). Nothing was changed on this machine.",
                        "Deine Bestätigung bleibt gültig. Wenn der Plan angewendet ist, starte `stonewave agent connect` noch einmal (neuer Code, dieselbe Wahl). Auf diesem Computer wurde nichts geändert.",
                    ),
                );
            }
            return stop(
                ui,
                "Wave did not finish within the time we wait",
                &format!("Last answer: {e}"),
                "Your confirmation stays valid on the account side. Run `stonewave agent connect` again (a new code, the same choice) to continue. Nothing was changed on this machine.",
            );
        }
        ui.pause(
            pause,
            &format!(
                "{msg} · {} {} {}",
                mmss(slept),
                t(de, "of at most", "von höchstens"),
                mmss(ctx.timing.mint_max_wait)
            ),
        )?;
        slept += pause;
    };

    // ---- from here on the key exists: keep it before anything else
    let agent = minted.agent.key.clone();
    let key_path = ctx.dirs.key_file(&agent);
    match write_private_file(&key_path, minted.key.expose().as_bytes(), &acl) {
        Ok(None) => {}
        Ok(Some(w)) => ui.warn(&w),
        Err(e) => {
            ui.fail("Wave made a key, but it could not be saved");
            ui.note(&format!("{}: {e}", key_path.display()));
            ui.note("The key is lost (it was never shown). Fix the problem above and run `stonewave agent connect` again: a new key replaces it.");
            ui.flush()?;
            return Ok(Outcome::Failed);
        }
    }
    ui.ok(
        "Key saved",
        &format!("{} · only you can read it", key_path.display()),
    );

    let old = State::load(&ctx.dirs.state_file(&agent)).ok();
    if old.is_some() {
        ui.note("This agent was connected here before; the new key replaces the old one.");
    }
    let mcp_url = match wave::mcp_address(&wave_addr, minted.mcp_url.as_deref()) {
        Ok(u) => u,
        Err(e) => {
            ui.fail("The key is saved, but Wave's MCP address cannot be used");
            ui.note(&e.to_string());
            ui.note("The key file is in place; `stonewave agent disconnect` removes it.");
            ui.flush()?;
            return Ok(Outcome::Failed);
        }
    };
    let mut st = State {
        agent: agent.clone(),
        agent_name: minted.agent.name.clone(),
        agent_id: minted.agent.id.clone(),
        team: minted.team.key.clone(),
        team_name: minted.team.name.clone(),
        company: minted.company.clone(),
        engine: engine.name().to_owned(),
        wave_url: wave_addr.base().to_owned(),
        mcp_url: mcp_url.clone(),
        account_url: o.account.base().to_owned(),
        created: timefmt::now_iso(),
        key_expires_at: minted.expires_at.clone(),
        key_file: super::dirs::text(&key_path),
        mcp_entry: None,
        service: None,
        script: None,
        wake_log: None,
    };
    if let Some(o) = &old {
        // what an earlier connect made stays known, so disconnect still finds it
        st.mcp_entry = o.mcp_entry.clone();
        st.service = o.service.clone();
        st.script = o.script.clone();
        st.wake_log = o.wake_log.clone();
    }
    let state_path = ctx.dirs.state_file(&agent);
    let save = |st: &State, ui: &mut dyn Ui| {
        if let Err(e) = st.save(&state_path) {
            ui.warn(&format!(
                "Could not write {}: {e}. `disconnect` will not know what was set up.",
                state_path.display()
            ));
        }
    };
    save(&st, ui);
    let mut all_well = true;

    // ---- the engine
    match engine::register(engine, ctx.exec, program.as_deref(), &mcp_url, ctx.dirs) {
        Ok(Registered::NothingToWrite) => {
            ui.ok("Wave address", &mcp_url);
            ui.note(&format!("Header for your agent: {}", engine::HEADER));
        }
        Ok(r) => {
            if r == Registered::Replaced {
                ui.warn(&format!(
                    "{} already had an entry named `wave`; it was replaced.",
                    engine.label()
                ));
            }
            ui.ok(
                engine.label(),
                &format!("`wave` added, no key in its configuration · {mcp_url}"),
            );
            let file =
                (engine == Engine::Codex).then(|| super::dirs::text(&ctx.dirs.codex_config()));
            st.mcp_entry = Some(EntryRec {
                kind: engine.name().to_owned(),
                name: engine::SERVER_NAME.to_owned(),
                file,
            });
            save(&st, ui);
        }
        Err(engine::RegisterError::Foreign(path)) => {
            all_well = false;
            ui.fail(&format!("{} was not changed", engine.label()));
            ui.note(&format!(
                "{} already has a [mcp_servers.wave] that this tool did not write. It was left alone.",
                path.display()
            ));
            ui.note("Remove or rename it, then run `stonewave agent connect` again. The key file is in place.");
        }
        Err(engine::RegisterError::Failed(m)) => {
            all_well = false;
            ui.fail(&format!("{} was not set up", engine.label()));
            ui.note(&m);
            ui.note("The key file is in place. Fix the problem and run `stonewave agent connect` again.");
        }
    }

    // ---- the wake service
    if let Some(node) = node {
        let done = station::install(
            &station::Request {
                agent: &agent,
                mcp_url: &mcp_url,
                node: &node,
                dirs: ctx.dirs,
                exec: ctx.exec,
            },
            &acl,
        );
        st.script = Some(super::dirs::text(&ctx.dirs.wake_script()));
        st.wake_log = Some(super::dirs::text(&ctx.dirs.wake_log(&agent)));
        if !done.rec.kind.is_empty() {
            st.service = Some(done.rec.clone());
        }
        save(&st, ui);
        for n in &done.notes {
            ui.note(n);
        }
        match done.problem {
            None => {
                let log = ctx.dirs.wake_log(&agent);
                ui.ok("Wake service", &format!("{} running", done.rec.name));
                ui.note(&format!(
                    "Follow it: {}",
                    station::follow_command(ctx.dirs.os, &super::dirs::text(&log))
                ));
                if ctx.dirs.os == super::dirs::Os::Linux {
                    ui.note(&format!(
                        "To keep it running when you are logged out: loginctl enable-linger {}",
                        std::env::var("USER").unwrap_or_else(|_| "<your user>".into())
                    ));
                }
            }
            Some(p) => {
                all_well = false;
                ui.fail("The wake service did not start");
                ui.note(&p);
                ui.note("What was written is recorded; `stonewave agent disconnect` removes it.");
            }
        }
    } else if let Some(n) = station_note {
        ui.note(&n);
    }

    // ---- what now
    ui.blank();
    let days = timefmt::days_until(&minted.expires_at, timefmt::now_secs());
    let expires = format!(
        "{}{}",
        timefmt::date_of(&minted.expires_at),
        days.map_or(String::new(), |d| format!(" (in {d} days)"))
    );
    ui.heading(&format!(
        "{} is connected to {} · team {}",
        minted.agent.name, approved.company_name, minted.team.name
    ));
    ui.kv(
        "Key",
        &format!(
            "expires {expires}; then `stonewave agent connect` again (`rotate` replaces a key, it does not extend it)"
        ),
    );
    match engine.program() {
        Some(p) => ui.kv("Start", &format!("stonewave agent run -- {p}")),
        None => ui.kv("Start", "stonewave agent run -- <your agent's command>"),
    }
    ui.kv("Check", "stonewave agent status");
    ui.kv("Remove", "stonewave agent disconnect");
    ui.flush()?;
    Ok(if all_well {
        Outcome::Done
    } else {
        Outcome::Failed
    })
}
