//! The commands against scripted answers: what they ask, what they write, what they say.

use std::path::Path;
use std::rc::Rc;

use serde_json::{Value, json};

use super::connect_flow::{ConnectOpts, connect};
use super::dirs::{Dirs, Os};
use super::engine::Engine;
use super::exec::fake::{Fake, failed, ok_out};
use super::key::testdir::TempDir;
use super::manage::{self, DisconnectOpts};
use super::state::{self, State};
use super::testkit::{FakeHttp, Shared, TestUi, reply};
use super::ui::Kind;
use super::{Ctx, Timing};
use crate::connect::{Account, ConnectError, Http, Reply};
use crate::flow::Outcome;

pub const KEY: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_s3cretS3cretS3cret";
pub const KEY2: &str = "wak_0a1b2c3d-0000-4000-8000-000000000001_n3wN3wN3wN3wN3w";
const SECRET_PART: &str = "s3cretS3cret";
const TOKEN: &str = "aaaaaaaaaaaaaaaa.bbbbbbbbbbbbbbbbb.cccccccccccccc";

const STARTED: &str = r#"{"device_code":"abcdefghijklmnopqrstuvwxyz0123456789ABCDEFG","user_code":"WAVE-7K4Q-X2MP","verification_uri":"https://account.test/link","expires_in":600,"interval":5,"words":["amber","river","cedar","fox"]}"#;

fn approved(extra: Value) -> String {
    let mut v = json!({
        "access_token": TOKEN, "token_type": "Bearer", "expires_in": 900, "scope": "agent:connect",
        "company": "acme", "company_name": "Acme GmbH",
        "team": "ops", "team_name": "Ops",
        "agent": "agent-demo", "agent_name": "Grok (Mac)", "engine": "grok",
        "wave_url": "https://wave.test"
    });
    for (k, x) in extra.as_object().unwrap() {
        if x.is_null() {
            v.as_object_mut().unwrap().remove(k);
        } else {
            v[k] = x.clone();
        }
    }
    v.to_string()
}

pub fn minted(with_mcp: bool) -> String {
    let mut v = json!({
        "key": KEY,
        "agent": {"id": "11111111-2222-4333-8444-555555555555", "key": "agent-demo", "name": "Grok (Mac)"},
        "team": {"key": "ops", "name": "Ops"},
        "company": "acme",
        "expires_at": "2099-02-20T10:00:00Z"
    });
    if with_mcp {
        v["mcp_url"] = json!("https://wave.test/mcp");
    }
    v.to_string()
}

fn node_ok() -> impl Fn(&str, &[String]) -> Option<crate::agent::exec::Output> {
    |_, a| (a == ["--version"]).then(|| ok_out("v20.1.0\n"))
}

struct Rig {
    _t: TempDir,
    dirs: Dirs,
    account: Rc<FakeHttp>,
    wave: Rc<FakeHttp>,
}

impl Rig {
    fn new(
        os: Os,
        account: Vec<Result<Reply, ConnectError>>,
        wave: Vec<Result<Reply, ConnectError>>,
    ) -> Rig {
        let t = TempDir::new("flow");
        let mut dirs = Dirs::under(t.path());
        dirs.os = os;
        Rig {
            _t: t,
            dirs,
            account: Rc::new(FakeHttp::new(account)),
            wave: Rc::new(FakeHttp::new(wave)),
        }
    }

    fn run<R>(&self, exec: &Fake, f: impl FnOnce(&Ctx<'_>) -> R) -> R {
        let (a, w) = (self.account.clone(), self.wave.clone());
        let make = move |acct: &Account| -> Box<dyn Http> {
            if acct.base() == "https://account.test" {
                Box::new(Shared(a.clone()))
            } else {
                Box::new(Shared(w.clone()))
            }
        };
        let ctx = Ctx {
            dirs: &self.dirs,
            exec,
            http_for: &make,
            host: "mac-1",
            os: "macOS",
            timing: Timing::default(),
        };
        f(&ctx)
    }

    fn connect(&self, exec: &Fake, ui: &mut TestUi, o: &ConnectOpts) -> Outcome {
        match self.run(exec, |ctx| connect(ctx, ui, o)) {
            Ok(x) => x,
            Err(e) => panic!("{e:?}"),
        }
    }
}

fn opts(engine: Option<Engine>) -> ConnectOpts {
    ConnectOpts {
        engine,
        company: None,
        name: None,
        no_station: false,
        yes: false,
        account: Account::parse("https://account.test").unwrap(),
        wave: None,
        no_browser: true,
        german: false,
        env: |_| None,
    }
}

fn pending() -> Result<Reply, ConnectError> {
    reply(400, r#"{"error":"authorization_pending"}"#)
}

fn account_ok(extra: Value) -> Vec<Result<Reply, ConnectError>> {
    vec![reply(200, STARTED), pending(), reply(200, &approved(extra))]
}

fn no_icacls(f: &Fake) -> Vec<String> {
    f.lines()
        .into_iter()
        .filter(|l| !l.starts_with("icacls"))
        .collect()
}

fn assert_no_key(ui: &TestUi) {
    let all = ui.text();
    assert!(
        !all.contains(SECRET_PART),
        "a key part reached the screen:\n{all}"
    );
    assert!(
        !all.contains("wak_0a1b"),
        "a key reached the screen:\n{all}"
    );
}

fn read(p: &Path) -> String {
    std::fs::read_to_string(p).unwrap_or_default()
}

fn not_ready() -> Result<Reply, ConnectError> {
    reply(409, r#"{"error":{"code":"agent_not_ready"}}"#)
}

// ------------------------------------------------------------------ connect

#[test]
fn connect_grok_with_the_wake_service_end_to_end_with_fakes() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![reply(201, &minted(false))],
    );
    let exec = Fake::new(&["grok", "node"]).with(node_ok());
    let mut ui = TestUi::new(&[true]);
    let out = rig.connect(&exec, &mut ui, &opts(Some(Engine::Grok)));
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    assert_no_key(&ui);

    // the account saw an agent sign-in with what we want
    let acc = rig.account.seen();
    let start: Value = serde_json::from_str(acc[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(start["purpose"], "agent");
    assert_eq!(start["want"]["engine"], "grok");
    assert_eq!(start["client_id"], "stonewave-cli");
    assert_eq!(acc.len(), 3);

    // Wave saw the token, once, with host/os/version/engine
    let w = rig.wave.seen();
    assert_eq!(w.len(), 1);
    assert_eq!(w[0].url, "https://wave.test/v1/agent-connect/mint");
    assert_eq!(w[0].bearer.as_deref(), Some(TOKEN));
    let body: Value = serde_json::from_str(w[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["engine"], "grok");
    assert_eq!(body["host"], "mac-1");

    // key file: exact content, private
    let key_file = rig.dirs.key_file("agent-demo");
    assert_eq!(read(&key_file), KEY);
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode(&key_file), 0o600);
        assert_eq!(mode(&rig.dirs.config), 0o700);
        assert_eq!(mode(&rig.dirs.wake_log("agent-demo")), 0o600);
    }

    // the engine got the address without the key, and the service came up
    let lines = no_icacls(&exec);
    assert_eq!(lines[0], "/fake/bin/node --version");
    assert_eq!(
        lines[1],
        "/fake/bin/grok mcp add --transport http -H Authorization: Bearer ${WAVE_AGENT_KEY} wave https://wave.test/mcp"
    );
    assert_eq!(lines[2], "systemctl --user daemon-reload");
    assert_eq!(
        lines[3],
        "systemctl --user enable --now stonewave-wave-agent-demo.service"
    );
    assert!(lines.iter().all(|l| !l.contains("wak_")), "{lines:?}");

    // the unit file knows the key file, not the key
    let unit = read(
        &rig.dirs
            .systemd_user
            .join("stonewave-wave-agent-demo.service"),
    );
    assert!(unit.contains("WAVE_AGENT_KEY_FILE="), "{unit}");
    assert!(unit.contains("WAVE_MCP_URL=https://wave.test/mcp"));
    assert!(!unit.contains("wak_"));
    assert!(rig.dirs.wake_script().exists());

    // state records exactly what was made
    let st = State::load(&rig.dirs.state_file("agent-demo")).unwrap();
    assert_eq!(st.mcp_entry.as_ref().unwrap().kind, "grok");
    assert_eq!(st.service.as_ref().unwrap().kind, "systemd");
    assert_eq!(st.service.as_ref().unwrap().files.len(), 1);
    assert_eq!(
        st.wake_log.as_deref(),
        Some(rig.dirs.wake_log("agent-demo").to_str().unwrap())
    );
    assert_eq!(st.mcp_url, "https://wave.test/mcp");
    assert!(!read(&rig.dirs.state_file("agent-demo")).contains("wak_"));
    assert!(ui.asked.iter().any(|q| q.contains("Set it up")));
    assert!(ui.has(Kind::Note, "tail -n 0 -F"), "{}", ui.text());
    assert!(ui.has(Kind::Note, "enable-linger"));
}

#[test]
fn a_mint_answer_with_its_own_mcp_url_on_the_same_host_is_used() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"engine": "claude-code"})),
        vec![reply(201, &minted(true))],
    );
    let exec = Fake::new(&["claude", "node"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::ClaudeCode));
    o.no_station = true;
    assert!(
        matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done),
        "{}",
        ui.text()
    );
    let lines = no_icacls(&exec);
    assert_eq!(
        lines[0],
        "/fake/bin/claude mcp add --transport http --scope user wave https://wave.test/mcp --header Authorization: Bearer ${WAVE_AGENT_KEY}"
    );
    assert_eq!(lines.len(), 1, "no station, no node check: {lines:?}");
    assert!(ui.asked.is_empty());
}

#[test]
fn an_mcp_url_on_another_host_is_refused_but_the_key_is_kept_safe() {
    let body = minted(true).replace("https://wave.test/mcp", "https://evil.test/mcp");
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"engine": "claude-code"})),
        vec![reply(201, &body)],
    );
    let exec = Fake::new(&["claude"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::ClaudeCode));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed));
    assert!(ui.has(Kind::Note, "MCP address"));
    assert!(
        no_icacls(&exec).is_empty(),
        "no engine was told about the other host"
    );
    assert_eq!(
        read(&rig.dirs.key_file("agent-demo")),
        KEY,
        "the key is saved"
    );
    assert_no_key(&ui);
}

#[test]
fn mint_waits_while_wave_sets_up_the_agent() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![
            not_ready(),
            not_ready(),
            not_ready(),
            reply(201, &minted(false)),
        ],
    );
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done));
    assert_eq!(ui.pauses.len(), 3);
    assert!(
        ui.pauses
            .iter()
            .all(|(d, m)| d.as_secs() == 5 && m.contains("setting up the agent"))
    );
    assert_eq!(rig.wave.seen().len(), 4);
    assert!(
        rig.wave
            .seen()
            .iter()
            .all(|s| s.bearer.as_deref() == Some(TOKEN))
    );
}

#[test]
fn mint_gives_up_after_ten_minutes_and_changes_nothing_here() {
    let many: Vec<_> = (0..200).map(|_| not_ready()).collect();
    let rig = Rig::new(Os::Linux, account_ok(json!({})), many);
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed));
    let total: u64 = ui.pauses.iter().map(|(d, _)| d.as_secs()).sum();
    assert_eq!(total, 600, "ten minutes in total");
    assert!(ui.has(Kind::Fail, "did not finish"));
    assert!(ui.has(Kind::Note, "`stonewave agent connect` again"));
    assert!(!rig.dirs.key_file("agent-demo").exists());
    assert!(!rig.dirs.state_file("agent-demo").exists());
    assert!(no_icacls(&exec).is_empty());
}

#[test]
fn too_many_requests_honours_retry_after() {
    let limited = Ok(Reply {
        status: 429,
        body: r#"{"error":{"code":"too_many_requests"}}"#.into(),
        retry_after: Some(7),
    });
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![limited, reply(201, &minted(false))],
    );
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done));
    assert_eq!(ui.pauses.len(), 1);
    assert_eq!(ui.pauses[0].0.as_secs(), 7);
}

#[test]
fn hard_mint_errors_stop_with_a_clear_message_and_no_files() {
    for (status, code, needle) in [
        (409, "already_minted", "again for a new code"),
        (403, "forbidden", "admin"),
        (403, "agent_connect_not_enabled", "not switched on"),
        (401, "token_expired", "ran out"),
        (401, "invalid_token", "not valid"),
    ] {
        let body = format!(r#"{{"error":{{"code":"{code}"}}}}"#);
        let rig = Rig::new(Os::Linux, account_ok(json!({})), vec![reply(status, &body)]);
        let exec = Fake::new(&["grok"]);
        let mut ui = TestUi::new(&[]);
        let mut o = opts(Some(Engine::Grok));
        o.no_station = true;
        assert!(
            matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed),
            "{code}"
        );
        assert!(ui.text().contains(needle), "{code}: {}", ui.text());
        assert!(!rig.dirs.key_file("agent-demo").exists(), "{code}");
        assert!(ui.pauses.is_empty(), "{code}: no retry");
        assert!(no_icacls(&exec).is_empty(), "{code}");
    }
}

#[test]
fn a_missing_engine_program_stops_before_anyone_signs_in() {
    let rig = Rig::new(Os::Linux, vec![], vec![]);
    let exec = Fake::new(&[]);
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        rig.connect(&exec, &mut ui, &opts(Some(Engine::Grok))),
        Outcome::Failed
    ));
    assert!(ui.text().contains("`grok` was not found"));
    assert!(
        rig.account.seen().is_empty(),
        "the account was not even asked"
    );
    assert!(!rig.dirs.config.exists());
}

#[test]
fn no_wave_address_anywhere_stops_before_the_key_is_asked_for() {
    let rig = Rig::new(Os::Linux, account_ok(json!({"wave_url": null})), vec![]);
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        rig.connect(&exec, &mut ui, &opts(Some(Engine::Grok))),
        Outcome::Failed
    ));
    assert!(ui.text().contains("--wave"), "{}", ui.text());
    assert!(rig.wave.seen().is_empty(), "mint was not called");
    assert!(!rig.dirs.key_file("agent-demo").exists());
    // with --wave it works
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"wave_url": null})),
        vec![reply(201, &minted(false))],
    );
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    o.wave = Some("https://wave.test".into());
    assert!(
        matches!(
            rig.connect(&Fake::new(&["grok"]), &mut ui, &o),
            Outcome::Done
        ),
        "{}",
        ui.text()
    );
    // and an unusable --wave address is refused up front
    let rig = Rig::new(Os::Linux, vec![], vec![]);
    let mut o = opts(Some(Engine::Grok));
    o.wave = Some("http://wave.example.com".into());
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        rig.connect(&Fake::new(&["grok"]), &mut ui, &o),
        Outcome::Failed
    ));
    assert!(rig.account.seen().is_empty());
}

#[test]
fn flag_wins_over_the_address_the_account_sent() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"wave_url": "https://other.test"})),
        vec![reply(201, &minted(false))],
    );
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    o.wave = Some("https://wave.test".into());
    assert!(matches!(
        rig.connect(&Fake::new(&["grok"]), &mut ui, &o),
        Outcome::Done
    ));
    assert_eq!(
        rig.wave.seen()[0].url,
        "https://wave.test/v1/agent-connect/mint"
    );
}

#[test]
fn missing_node_finishes_the_connect_and_says_the_wake_service_is_missing() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![reply(201, &minted(false))],
    );
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let out = rig.connect(&exec, &mut ui, &opts(Some(Engine::Grok)));
    assert!(matches!(out, Outcome::Done), "exit 0: {}", ui.text());
    assert!(ui.has(Kind::Note, "wake service was not set up"));
    assert!(ui.has(Kind::Note, "Node.js"));
    assert!(ui.asked.is_empty());
    assert!(rig.dirs.key_file("agent-demo").exists());
    let st = State::load(&rig.dirs.state_file("agent-demo")).unwrap();
    assert!(st.service.is_none() && st.mcp_entry.is_some());
}

#[test]
fn declining_or_having_nobody_to_ask_skips_the_service_and_yes_accepts() {
    let go = |answers: &[bool], can_ask: bool, yes: bool| {
        let rig = Rig::new(
            Os::Linux,
            account_ok(json!({})),
            vec![reply(201, &minted(false))],
        );
        let exec = Fake::new(&["grok", "node"]).with(node_ok());
        let mut ui = TestUi::new(answers);
        ui.can_ask = can_ask;
        let mut o = opts(Some(Engine::Grok));
        o.yes = yes;
        assert!(
            matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done),
            "{}",
            ui.text()
        );
        State::load(&rig.dirs.state_file("agent-demo"))
            .unwrap()
            .service
            .is_some()
    };
    assert!(!go(&[false], true, false), "declined");
    assert!(!go(&[], false, false), "nobody to ask");
    assert!(go(&[], false, true), "--yes");
    assert!(go(&[true], true, false), "yes");
}

#[test]
fn the_engine_the_person_confirmed_wins_and_must_exist() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"engine": "claude-code"})),
        vec![reply(201, &minted(false))],
    );
    let exec = Fake::new(&["grok", "claude"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done));
    assert!(no_icacls(&exec)[0].starts_with("/fake/bin/claude mcp add"));
    let body: Value = serde_json::from_str(rig.wave.seen()[0].body.as_ref().unwrap()).unwrap();
    assert_eq!(body["engine"], "claude-code");
    // the confirmed engine is not installed: stop before the key
    let rig = Rig::new(Os::Linux, account_ok(json!({"engine": "codex"})), vec![]);
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        rig.connect(&Fake::new(&["grok"]), &mut ui, &o),
        Outcome::Failed
    ));
    assert!(rig.wave.seen().is_empty());
}

#[test]
fn engine_choice_without_the_option() {
    let go = |present: &[&str], answers: &[bool]| {
        let rig = Rig::new(
            Os::Linux,
            account_ok(json!({"engine": "other"})),
            vec![reply(201, &minted(false))],
        );
        let exec = Fake::new(present);
        let mut ui = TestUi::new(answers);
        let mut o = opts(None);
        o.no_station = true;
        let out = rig.connect(&exec, &mut ui, &o);
        (out, ui, exec, rig)
    };
    // one found: taken. With "other" confirmed on the page the entry is not written.
    let (out, ui, _, _) = go(&["grok"], &[]);
    assert!(matches!(out, Outcome::Done));
    assert!(ui.has(Kind::Note, "Found Grok Build"));
    // none found: generic
    let (out, ui, exec, _) = go(&[], &[]);
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    assert!(ui.has(Kind::Ok, "Wave address"));
    assert!(ui.has(Kind::Note, "Authorization: Bearer ${WAVE_AGENT_KEY}"));
    assert!(no_icacls(&exec).is_empty());
    // several found and nobody to say: stop before anything happens
    let rig = Rig::new(Os::Linux, vec![], vec![]);
    let mut ui = TestUi::new(&[]);
    ui.can_ask = false;
    assert!(matches!(
        rig.connect(&Fake::new(&["grok", "claude"]), &mut ui, &opts(None)),
        Outcome::Failed
    ));
    assert!(ui.text().contains("--engine"));
    assert!(rig.account.seen().is_empty());
    // several found and the person picks the second
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"engine": "grok"})),
        vec![reply(201, &minted(false))],
    );
    let mut ui = TestUi::new(&[false, true]);
    let mut o = opts(None);
    o.no_station = true;
    assert!(matches!(
        rig.connect(&Fake::new(&["claude", "grok"]), &mut ui, &o),
        Outcome::Done
    ));
    assert_eq!(
        ui.asked,
        vec!["Connect Claude Code?", "Connect Grok Build?"]
    );
}

#[test]
fn codex_is_written_to_the_config_and_a_foreign_table_stops_that_step_only() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"engine": "codex"})),
        vec![reply(201, &minted(false))],
    );
    let exec = Fake::new(&["codex"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Codex));
    o.no_station = true;
    assert!(
        matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done),
        "{}",
        ui.text()
    );
    let cfg = read(&rig.dirs.codex_config());
    assert!(
        cfg.contains("bearer_token_env_var = \"WAVE_AGENT_KEY\""),
        "{cfg}"
    );
    assert!(!cfg.contains("wak_"));
    assert!(no_icacls(&exec).is_empty(), "codex is not run");
    let st = State::load(&rig.dirs.state_file("agent-demo")).unwrap();
    assert_eq!(
        st.mcp_entry.unwrap().file.as_deref(),
        Some(rig.dirs.codex_config().to_str().unwrap())
    );

    // a foreign [mcp_servers.wave]: left alone, exit failure, key kept
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({"engine": "codex"})),
        vec![reply(201, &minted(false))],
    );
    std::fs::create_dir_all(&rig.dirs.codex_home).unwrap();
    let mine = "[mcp_servers.wave]\nurl = \"https://mine/mcp\"\n";
    std::fs::write(rig.dirs.codex_config(), mine).unwrap();
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        rig.connect(&Fake::new(&["codex"]), &mut ui, &o),
        Outcome::Failed
    ));
    assert_eq!(read(&rig.dirs.codex_config()), mine);
    assert!(ui.has(Kind::Note, "did not write"));
    assert!(rig.dirs.key_file("agent-demo").exists());
    assert!(
        State::load(&rig.dirs.state_file("agent-demo"))
            .unwrap()
            .mcp_entry
            .is_none()
    );
}

#[test]
fn a_failing_engine_command_is_reported_and_the_key_stays() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![reply(201, &minted(false))],
    );
    let exec =
        Fake::new(&["grok"]).with(|p, _| (p != "icacls").then(|| failed("config is read-only")));
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed));
    assert!(ui.has(Kind::Note, "config is read-only"));
    assert!(rig.dirs.key_file("agent-demo").exists());
    assert_no_key(&ui);
}

#[test]
fn ctrl_c_while_waiting_leaves_no_file_behind() {
    let many: Vec<_> = (0..5).map(|_| not_ready()).collect();
    let rig = Rig::new(Os::Linux, account_ok(json!({})), many);
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    ui.interrupt_on_pause = true;
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    let r = rig.run(&exec, |ctx| connect(ctx, &mut ui, &o));
    assert!(matches!(r, Err(crate::term::Error::Interrupted)));
    assert!(!rig.dirs.key_file("agent-demo").exists());
    assert!(!rig.dirs.state_file("agent-demo").exists());
}

#[test]
fn denied_expired_and_malformed_sign_ins() {
    let go = |account: Vec<Result<Reply, ConnectError>>| {
        let rig = Rig::new(Os::Linux, account, vec![]);
        let mut ui = TestUi::new(&[]);
        let mut o = opts(Some(Engine::Grok));
        o.no_station = true;
        let out = rig.connect(&Fake::new(&["grok"]), &mut ui, &o);
        (out, ui, rig)
    };
    let (out, ui, rig) = go(vec![
        reply(200, STARTED),
        reply(400, r#"{"error":"access_denied"}"#),
    ]);
    assert!(matches!(out, Outcome::Declined), "{}", ui.text());
    assert!(!rig.dirs.config.exists());
    let (out, ui, _) = go(vec![
        reply(200, STARTED),
        reply(400, r#"{"error":"expired_token"}"#),
    ]);
    assert!(matches!(out, Outcome::Failed));
    assert!(ui.text().contains("ran out"));
    // an agent answer with the wrong scope or a bad slug is a protocol error, nothing is written
    for bad in [
        json!({"scope": "server:register"}),
        json!({"agent": "../x"}),
        json!({"team": "A B"}),
        json!({"token_type": "Basic"}),
    ] {
        let (out, ui, rig) = go(vec![
            reply(200, STARTED),
            reply(200, &approved(bad.clone())),
        ]);
        assert!(matches!(out, Outcome::Failed), "{bad}");
        assert!(ui.text().contains("unexpected answer"), "{}", ui.text());
        assert!(!rig.dirs.config.exists());
    }
}

#[test]
fn a_key_that_cannot_be_saved_is_said_plainly() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![reply(201, &minted(false))],
    );
    // a directory where the key file should go
    std::fs::create_dir_all(rig.dirs.key_file("agent-demo")).unwrap();
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed));
    assert!(ui.has(Kind::Fail, "could not be saved"));
    assert!(ui.has(Kind::Note, "a new key replaces it"));
    assert!(no_icacls(&exec).is_empty(), "nothing else was done");
    assert_no_key(&ui);
}

// ------------------------------------------------------------------ after connect

/// A connected agent on disk (as connect leaves it), without running connect.
fn connected(rig: &Rig, service: bool) -> State {
    std::fs::create_dir_all(&rig.dirs.config).unwrap();
    std::fs::write(rig.dirs.key_file("agent-demo"), KEY).unwrap();
    let mut st = state::sample("agent-demo");
    st.key_file = rig
        .dirs
        .key_file("agent-demo")
        .to_string_lossy()
        .into_owned();
    st.key_expires_at = "2099-02-20T10:00:00Z".into();
    st.mcp_entry = None;
    if service {
        let unit = rig
            .dirs
            .systemd_user
            .join("stonewave-wave-agent-demo.service");
        std::fs::create_dir_all(&rig.dirs.systemd_user).unwrap();
        std::fs::write(&unit, "[Service]").unwrap();
        st.service.as_mut().unwrap().files = vec![unit.to_string_lossy().into_owned()];
        std::fs::create_dir_all(&rig.dirs.state).unwrap();
        std::fs::write(rig.dirs.wake_log("agent-demo"), "fleet: x\n").unwrap();
        st.wake_log = Some(
            rig.dirs
                .wake_log("agent-demo")
                .to_string_lossy()
                .into_owned(),
        );
        std::fs::write(rig.dirs.wake_script(), "// x").unwrap();
        st.script = Some(rig.dirs.wake_script().to_string_lossy().into_owned());
    } else {
        st.service = None;
        st.wake_log = None;
        st.script = None;
    }
    st.save(&rig.dirs.state_file("agent-demo")).unwrap();
    st
}

fn status_body() -> String {
    json!({
        "agent": {"id": "i-1", "key": "agent-demo", "name": "Grok (Mac)"},
        "team": {"key": "ops", "name": "Ops"},
        "company": "acme", "engine": "grok",
        "key_expires_at": "2099-02-20T10:00:00Z",
        "last_seen_at": null, "last_session": null
    })
    .to_string()
}

#[test]
fn status_json_has_a_stable_shape_and_no_secrets() {
    let rig = Rig::new(Os::Linux, vec![], vec![reply(200, &status_body())]);
    connected(&rig, true);
    let exec = Fake::new(&[]);
    let text = rig.run(&exec, |ctx| manage::status_json(ctx, None, None));
    assert!(
        !text.contains("wak_") && !text.contains(SECRET_PART),
        "{text}"
    );
    assert_eq!(text.lines().count(), 1);
    let v: Value = serde_json::from_str(&text).unwrap();
    for k in [
        "stonewave",
        "connected",
        "agent",
        "team",
        "company",
        "engine",
        "key_expires_at",
        "key_days_left",
        "local",
        "remote",
        "warnings",
    ] {
        assert!(v.get(k).is_some(), "missing {k}: {text}");
    }
    assert_eq!(v["connected"], true);
    assert_eq!(v["agent"]["key"], "agent-demo");
    assert_eq!(v["remote"]["reachable"], true);
    assert_eq!(v["remote"]["status"]["last_seen_at"], Value::Null);
    assert_eq!(v["local"]["key_file"]["readable"], true);
    assert_eq!(v["local"]["service"]["installed"], true);
    assert_eq!(rig.wave.seen()[0].bearer.as_deref(), Some(KEY));
    // not connected: still a JSON object
    let empty = Rig::new(Os::Linux, vec![], vec![]);
    let text = empty.run(&exec, |ctx| manage::status_json(ctx, None, None));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["connected"], false);
}

#[test]
fn status_without_network_still_reports_the_local_part_and_warns_before_expiry() {
    let rig = Rig::new(
        Os::Linux,
        vec![],
        vec![
            Err(ConnectError::Network("timeout".into())),
            Err(ConnectError::Network("timeout".into())),
        ],
    );
    let mut st = connected(&rig, false);
    // 10 days from now
    st.key_expires_at = super::timefmt::format_iso(super::timefmt::now_secs() + 10 * 86_400 + 60);
    st.save(&rig.dirs.state_file("agent-demo")).unwrap();
    let exec = Fake::new(&[]);
    let mut ui = TestUi::new(&[]);
    let out = rig
        .run(&exec, |ctx| manage::status(ctx, &mut ui, None, None))
        .unwrap();
    assert!(matches!(out, Outcome::Done));
    assert!(ui.has(Kind::Kv, "not reachable"), "{}", ui.text());
    assert!(ui.has(Kind::Warn, "runs out in 10 days"), "{}", ui.text());
    assert!(ui.has(Kind::Kv, "not installed"));
    assert_no_key(&ui);
    let text = rig.run(&exec, |ctx| manage::status_json(ctx, None, None));
    let v: Value = serde_json::from_str(&text).unwrap();
    assert_eq!(v["remote"]["reachable"], false);
    assert_eq!(v["key_days_left"], 10);
}

#[test]
fn rotate_replaces_the_key_file_restarts_the_service_and_keeps_state_current() {
    let rotated = json!({"key": KEY2, "expires_at": "2099-02-20T10:00:00Z"}).to_string();
    let rig = Rig::new(Os::Linux, vec![], vec![reply(200, &rotated)]);
    connected(&rig, true);
    let exec = Fake::new(&[]);
    let mut ui = TestUi::new(&[]);
    let out = rig
        .run(&exec, |ctx| manage::rotate(ctx, &mut ui, None, None))
        .unwrap();
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    assert_eq!(
        rig.wave.seen()[0].bearer.as_deref(),
        Some(KEY),
        "the old key asks"
    );
    assert_eq!(read(&rig.dirs.key_file("agent-demo")), KEY2);
    assert_eq!(
        State::load(&rig.dirs.state_file("agent-demo"))
            .unwrap()
            .key_expires_at,
        "2099-02-20T10:00:00Z"
    );
    assert!(ui.has(Kind::Ok, "same expiry 2099-02-20"), "{}", ui.text());
    assert!(
        no_icacls(&exec)
            .contains(&"systemctl --user restart stonewave-wave-agent-demo.service".to_owned()),
        "{:?}",
        exec.lines()
    );
    assert_no_key(&ui);
    assert!(!ui.text().contains("n3wN3w"));
}

#[cfg(unix)]
#[test]
fn rotate_writes_0600_and_0700_even_when_the_old_ones_were_loosened() {
    use std::os::unix::fs::PermissionsExt;
    let rotated = json!({"key": KEY2, "expires_at": "2099-05-01T00:00:00Z"}).to_string();
    let rig = Rig::new(Os::Linux, vec![], vec![reply(200, &rotated)]);
    connected(&rig, false);
    let mode = |p: &Path| std::fs::metadata(p).unwrap().permissions().mode() & 0o777;
    std::fs::set_permissions(
        rig.dirs.key_file("agent-demo"),
        std::fs::Permissions::from_mode(0o644),
    )
    .unwrap();
    std::fs::set_permissions(&rig.dirs.config, std::fs::Permissions::from_mode(0o755)).unwrap();
    let mut ui = TestUi::new(&[]);
    let out = rig
        .run(&Fake::new(&[]), |ctx| {
            manage::rotate(ctx, &mut ui, None, None)
        })
        .unwrap();
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    assert_eq!(read(&rig.dirs.key_file("agent-demo")), KEY2);
    assert_eq!(mode(&rig.dirs.key_file("agent-demo")), 0o600);
    assert_eq!(mode(&rig.dirs.config), 0o700);
}

#[test]
fn rotate_with_a_dead_key_changes_nothing() {
    let rig = Rig::new(
        Os::Linux,
        vec![],
        vec![reply(401, r#"{"error":{"code":"invalid_token"}}"#)],
    );
    connected(&rig, false);
    let mut ui = TestUi::new(&[]);
    let out = rig
        .run(&Fake::new(&[]), |ctx| {
            manage::rotate(ctx, &mut ui, None, None)
        })
        .unwrap();
    assert!(matches!(out, Outcome::Failed));
    assert_eq!(read(&rig.dirs.key_file("agent-demo")), KEY);
    assert!(ui.text().contains("agent connect"));
}

fn disconnect(rig: &Rig, exec: &Fake, ui: &mut TestUi, yes: bool, keep_remote: bool) -> Outcome {
    rig.run(exec, |ctx| {
        manage::disconnect(
            ctx,
            ui,
            &DisconnectOpts {
                agent: None,
                wave: None,
                yes,
                keep_remote,
            },
        )
    })
    .unwrap()
}

#[test]
fn disconnect_revokes_first_then_removes_exactly_what_was_recorded() {
    let rig = Rig::new(Os::Linux, vec![], vec![reply(200, r#"{"revoked":1}"#)]);
    let mut st = connected(&rig, true);
    st.mcp_entry = Some(state::EntryRec {
        kind: "grok".into(),
        name: "wave".into(),
        file: None,
    });
    st.save(&rig.dirs.state_file("agent-demo")).unwrap();
    // things that are not ours
    let bystander = rig.dirs.config.join("notes.txt");
    std::fs::write(&bystander, "mine").unwrap();
    let other_unit = rig.dirs.systemd_user.join("other.service");
    std::fs::write(&other_unit, "x").unwrap();
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[true]);
    let out = disconnect(&rig, &exec, &mut ui, false, false);
    assert!(matches!(out, Outcome::Done), "{}", ui.text());
    assert_eq!(
        rig.wave.seen()[0].url,
        "https://wave.test/v1/agent-connect/revoke"
    );
    assert_eq!(rig.wave.seen()[0].bearer.as_deref(), Some(KEY));
    let lines = no_icacls(&exec);
    assert!(
        lines.contains(&"/fake/bin/grok mcp remove wave".to_owned()),
        "{lines:?}"
    );
    assert!(
        lines.contains(
            &"systemctl --user disable --now stonewave-wave-agent-demo.service".to_owned()
        )
    );
    assert!(!rig.dirs.key_file("agent-demo").exists());
    assert!(!rig.dirs.state_file("agent-demo").exists());
    assert!(!rig.dirs.wake_log("agent-demo").exists());
    assert!(
        !rig.dirs.wake_script().exists(),
        "last agent: the shared script goes too"
    );
    assert!(
        !rig.dirs
            .systemd_user
            .join("stonewave-wave-agent-demo.service")
            .exists()
    );
    assert!(bystander.exists() && other_unit.exists());
    assert_no_key(&ui);
}

#[test]
fn disconnect_keeps_the_shared_script_while_another_agent_uses_it() {
    let rig = Rig::new(Os::Linux, vec![], vec![reply(200, r#"{"revoked":1}"#)]);
    connected(&rig, true);
    let mut other = state::sample("agent-other");
    other.script = Some(rig.dirs.wake_script().to_string_lossy().into_owned());
    other.save(&rig.dirs.state_file("agent-other")).unwrap();
    let mut ui = TestUi::new(&[]);
    let out = rig
        .run(&Fake::new(&["grok"]), |ctx| {
            manage::disconnect(
                ctx,
                &mut ui,
                &DisconnectOpts {
                    agent: Some("agent-demo"),
                    wave: None,
                    yes: true,
                    keep_remote: false,
                },
            )
        })
        .unwrap();
    assert!(matches!(out, Outcome::Done));
    assert!(rig.dirs.wake_script().exists());
    assert!(rig.dirs.state_file("agent-other").exists());
}

#[test]
fn disconnect_without_a_connection_refuses_unless_keep_remote() {
    let rig = Rig::new(
        Os::Linux,
        vec![],
        vec![Err(ConnectError::Network("timeout".into()))],
    );
    connected(&rig, false);
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        disconnect(&rig, &exec, &mut ui, true, false),
        Outcome::Failed
    ));
    assert!(ui.text().contains("--keep-remote"), "{}", ui.text());
    assert!(rig.dirs.key_file("agent-demo").exists(), "nothing removed");
    assert!(rig.dirs.state_file("agent-demo").exists());
    assert!(no_icacls(&exec).is_empty());
    // --keep-remote: local cleanup, and it says the key still works
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        disconnect(&rig, &exec, &mut ui, true, true),
        Outcome::Done
    ));
    assert!(ui.has(Kind::Warn, "still works"));
    assert!(!rig.dirs.key_file("agent-demo").exists());
    assert_eq!(rig.wave.seen().len(), 1, "no second try to reach Wave");
}

#[test]
fn disconnect_treats_401_as_already_revoked_and_other_errors_as_a_stop() {
    let rig = Rig::new(
        Os::Linux,
        vec![],
        vec![reply(401, r#"{"error":{"code":"invalid_token"}}"#)],
    );
    connected(&rig, false);
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        disconnect(&rig, &Fake::new(&[]), &mut ui, true, false),
        Outcome::Done
    ));
    assert!(!rig.dirs.key_file("agent-demo").exists());
    let rig = Rig::new(
        Os::Linux,
        vec![],
        vec![reply(403, r#"{"error":{"code":"forbidden"}}"#)],
    );
    connected(&rig, false);
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        disconnect(&rig, &Fake::new(&[]), &mut ui, true, false),
        Outcome::Failed
    ));
    assert!(rig.dirs.key_file("agent-demo").exists());
}

#[test]
fn disconnect_asks_and_a_no_changes_nothing() {
    let rig = Rig::new(Os::Linux, vec![], vec![]);
    connected(&rig, false);
    let mut ui = TestUi::new(&[false]);
    assert!(matches!(
        disconnect(&rig, &Fake::new(&[]), &mut ui, false, false),
        Outcome::Declined
    ));
    assert!(rig.dirs.key_file("agent-demo").exists());
    let mut ui = TestUi::new(&[]);
    ui.can_ask = false;
    assert!(matches!(
        disconnect(&rig, &Fake::new(&[]), &mut ui, false, false),
        Outcome::Declined
    ));
    assert!(rig.wave.seen().is_empty());
}

#[test]
fn a_step_that_cannot_be_undone_keeps_the_state_file_for_another_try() {
    let rig = Rig::new(Os::Linux, vec![], vec![reply(200, r#"{"revoked":1}"#)]);
    let mut st = connected(&rig, false);
    st.mcp_entry = Some(state::EntryRec {
        kind: "grok".into(),
        name: "wave".into(),
        file: None,
    });
    st.save(&rig.dirs.state_file("agent-demo")).unwrap();
    let exec =
        Fake::new(&["grok"]).with(|p, _| (p != "icacls").then(|| failed("permission denied")));
    let mut ui = TestUi::new(&[]);
    assert!(matches!(
        disconnect(&rig, &exec, &mut ui, true, false),
        Outcome::Failed
    ));
    assert!(rig.dirs.state_file("agent-demo").exists());
    assert!(ui.has(Kind::Fail, "Could not remove `wave`"));
    assert!(ui.has(Kind::Warn, "can be run again"));
}

// ---------------------------------------------------------------- the browser, the waiting lines, the long wait for an admin

fn open_calls(f: &Fake) -> Vec<String> {
    f.lines()
        .into_iter()
        .filter(|l| l.starts_with("start:"))
        .collect()
}

fn run_with_browser(os: Os, exec: &Fake, env: fn(&str) -> Option<String>, started: &str) -> TestUi {
    let mut account = account_ok(json!({}));
    account[0] = reply(200, started);
    let rig = Rig::new(os, account, vec![reply(201, &minted(false))]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    o.no_browser = false;
    o.env = env;
    assert!(matches!(rig.connect(exec, &mut ui, &o), Outcome::Done));
    ui
}

#[test]
fn the_browser_is_opened_on_the_account_page_and_the_code_is_still_shown() {
    let exec = Fake::new(&["grok", "open"]);
    let ui = run_with_browser(Os::Mac, &exec, |_| None, STARTED);
    assert_eq!(
        open_calls(&exec),
        vec!["start:open https://account.test/link"]
    );
    assert!(
        ui.has(Kind::Kv, "WAVE-7K4Q-X2MP"),
        "the code stays on the screen"
    );
    assert!(
        ui.has(Kind::Kv, "amber"),
        "and the four words, the person's check"
    );
    assert!(ui.has(
        Kind::Note,
        "Your browser is open. Type the code on the page."
    ));
    assert!(ui.has(Kind::Note, "same four words"));

    let exec = Fake::new(&["grok"]);
    run_with_browser(Os::Windows, &exec, |_| None, STARTED);
    assert_eq!(
        open_calls(&exec),
        vec!["start:rundll32 url.dll,FileProtocolHandler https://account.test/link"]
    );

    // Linux: only where there is a screen
    let exec = Fake::new(&["grok"]);
    run_with_browser(
        Os::Linux,
        &exec,
        |n| (n == "DISPLAY").then(|| ":0".into()),
        STARTED,
    );
    assert_eq!(
        open_calls(&exec),
        vec!["start:xdg-open https://account.test/link"]
    );
    let exec = Fake::new(&["grok"]);
    let ui = run_with_browser(Os::Linux, &exec, |_| None, STARTED);
    assert!(
        open_calls(&exec).is_empty(),
        "a server over ssh has no browser to open"
    );
    assert!(ui.has(Kind::Note, "Open the link on any device with a browser"));
}

#[test]
fn a_browser_that_cannot_be_started_or_is_not_wanted_says_where_to_go() {
    let exec = Fake::new(&["grok"]);
    exec.fail_start.set(true);
    let ui = run_with_browser(Os::Mac, &exec, |_| None, STARTED);
    assert!(ui.has(Kind::Note, "Open the link on any device with a browser"));
    // --no-browser
    let exec = Fake::new(&["grok"]);
    let rig = Rig::new(
        Os::Mac,
        account_ok(json!({})),
        vec![reply(201, &minted(false))],
    );
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    o.no_browser = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done));
    assert!(open_calls(&exec).is_empty());
}

#[test]
fn a_link_with_the_code_in_it_is_used_only_when_it_is_the_accounts_own_page() {
    let with = |uri: &str| {
        STARTED.replace(
            r#""expires_in":600"#,
            &format!(r#""verification_uri_complete":"{uri}","expires_in":600"#),
        )
    };
    let exec = Fake::new(&["grok"]);
    let ui = run_with_browser(
        Os::Mac,
        &exec,
        |_| None,
        &with("https://account.test/link?code=WAVE-7K4Q-X2MP"),
    );
    assert_eq!(
        open_calls(&exec),
        vec!["start:open https://account.test/link?code=WAVE-7K4Q-X2MP"]
    );
    assert!(ui.has(Kind::Note, "the code is already filled in"));
    // another host, another path, a look-alike host, plain http, spaces: the plain page is opened instead
    for bad in [
        "https://evil.test/link?code=X",
        "https://account.test/other?code=X",
        "https://account.test.evil.test/link?code=X",
        "http://account.test/link?code=X",
        "https://account.test/link?code=X Y",
        "https://account.test/linkx",
    ] {
        let exec = Fake::new(&["grok"]);
        run_with_browser(Os::Mac, &exec, |_| None, &with(bad));
        assert_eq!(
            open_calls(&exec),
            vec!["start:open https://account.test/link"],
            "{bad}"
        );
    }
}

#[test]
fn a_long_wait_says_plainly_that_wave_waits_for_an_admin_and_how_long_it_may_take() {
    let many: Vec<_> = (0..200).map(|_| not_ready()).collect();
    let rig = Rig::new(Os::Linux, account_ok(json!({})), many);
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed));
    // at first: setting up, with the time so far and the most it can take
    assert!(
        ui.pauses[0].1.contains("Wave is setting up the agent"),
        "{:?}",
        ui.pauses[0]
    );
    assert!(
        ui.pauses[0].1.contains("0:00 of at most 10:00"),
        "{:?}",
        ui.pauses[0]
    );
    assert!(
        ui.pauses[3].1.contains("0:15 of at most 10:00"),
        "{:?}",
        ui.pauses[3]
    );
    // after 90 seconds: the plain words, said once as a note too
    assert!(
        ui.pauses[30]
            .1
            .contains("Wave is waiting for an admin to apply the plan"),
        "{:?}",
        ui.pauses[30]
    );
    assert_eq!(
        ui.events
            .iter()
            .filter(|(k, a, _)| *k == Kind::Note && a.contains("takes longer than usual"))
            .count(),
        1
    );
    assert!(ui.has(Kind::Fail, "did not finish setting up the agent"));
    assert!(ui.has(Kind::Note, "Tell an admin of the company"));
    assert!(ui.has(Kind::Note, "When the plan is applied"));
    assert!(!rig.dirs.key_file("agent-demo").exists());
}

#[test]
fn the_same_in_german_when_asked() {
    let many: Vec<_> = (0..200).map(|_| not_ready()).collect();
    let rig = Rig::new(Os::Linux, account_ok(json!({})), many);
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    o.german = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Failed));
    assert!(ui.has(Kind::Heading, "Im Browser anmelden"));
    assert!(ui.has(Kind::Kv, "amber"));
    assert!(
        ui.pauses[0]
            .1
            .contains("Wave richtet den Agenten ein · 0:00 von höchstens 10:00"),
        "{:?}",
        ui.pauses[0]
    );
    assert!(
        ui.pauses[30]
            .1
            .contains("Wave wartet auf einen Admin, der den Plan anwendet")
    );
    assert!(ui.has(Kind::Note, "Deine Bestätigung bleibt gültig"));
}

#[test]
fn a_short_wait_never_mentions_an_admin() {
    let rig = Rig::new(
        Os::Linux,
        account_ok(json!({})),
        vec![not_ready(), not_ready(), reply(201, &minted(false))],
    );
    let exec = Fake::new(&["grok"]);
    let mut ui = TestUi::new(&[]);
    let mut o = opts(Some(Engine::Grok));
    o.no_station = true;
    assert!(matches!(rig.connect(&exec, &mut ui, &o), Outcome::Done));
    assert!(!ui.text().contains("admin"), "{}", ui.text());
}
