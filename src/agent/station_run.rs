//! The station that runs (F-136, D-254): it asks Wave's inbox for what is addressed to the agent, and for each room where a person of the company
//! mentioned it, **continues that room's session** of the engine (Claude Code) with one wake: a fixed text with ids, strict configuration, limits.
//!
//! Who wakes it: only a person of the company (author kind `member`), never an agent, never text of outside origin, and only in the rooms it was
//! told to serve (all rooms it may read if none were named). Everything else is skipped and said once with its reason, never with its text.
//!
//! What it never does: put the text of a message into a prompt, a log or a line; start a run without the limits below; start a second run at the
//! same time; hand the run more of the environment than [`spawn::clean_env`] lets through; write the key anywhere.
//!
//! Slice 1 polls the inbox (every `poll`, 15 s by default). The live line to Wave's doorbell is slice 2.

use std::collections::BTreeMap;
use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use serde_json::{Value, json};

use super::claude::{self, Settings, Verdict};
use super::dirs::Dirs;
use super::engine::KEY_VAR;
use super::key::{Key, atomic_write, read_key_file, scrub};
use super::sessions::{RoomSession, Sessions};
use super::spawn::{Ended, GRACE, RunReq, Spawn, clean_env};
use crate::connect::{ConnectError, Http};

pub const DEFAULT_POLL: Duration = Duration::from_secs(15);
pub const DEFAULT_TIMEOUT: Duration = Duration::from_secs(20 * 60);
const PAGE: u64 = 20;
const MAX_PAGES: usize = 5;
/// Ids go into a prompt, so they must be plain.
fn plain_id(s: &str) -> bool {
    (1..=80).contains(&s.len())
        && s.bytes()
            .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'_')
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeySource {
    /// A private file (`agent connect` writes one).
    File(PathBuf),
    /// An environment variable of the station's own process (for example from `infisical run`); the key never touches a disk.
    Env(String),
}

#[derive(Debug, Clone)]
pub struct Config {
    pub agent: String,
    pub mcp_url: String,
    pub key: KeySource,
    pub workdir: PathBuf,
    /// Names, short names or ids of the rooms to serve; empty = every room the agent may read.
    pub rooms: Vec<String>,
    pub poll: Duration,
    pub timeout: Duration,
    pub settings: Settings,
    pub per_hour: u32,
    pub per_day: u32,
    /// After this many wakes a room's session is ended on purpose (with a handover note).
    pub session_max_wakes: u32,
    pub program: Option<PathBuf>,
    pub grace: Duration,
}

impl Config {
    pub fn new(agent: &str, mcp_url: &str, key: KeySource, workdir: PathBuf) -> Config {
        Config {
            agent: agent.to_owned(),
            mcp_url: mcp_url.to_owned(),
            key,
            workdir,
            rooms: Vec::new(),
            poll: DEFAULT_POLL,
            timeout: DEFAULT_TIMEOUT,
            settings: Settings {
                max_turns: 40,
                budget_usd: Some(2.0),
                model: None,
            },
            per_hour: 10,
            per_day: 50,
            session_max_wakes: 200,
            program: None,
            grace: GRACE,
        }
    }
}

// ------------------------------------------------------------------ the key

pub fn load_key(src: &KeySource, env: &dyn Fn(&str) -> Option<String>) -> Result<Key, String> {
    match src {
        KeySource::Env(name) => {
            let v = env(name).ok_or_else(|| format!("the variable {name} is not set (start the station with `infisical run -- ...` or export it)"))?;
            Key::parse(&v).ok_or_else(|| {
                format!("the variable {name} does not hold an agent key (it starts with wak_)")
            })
        }
        KeySource::File(p) => read_key_file(p).map_err(|e| format!("{e}")),
    }
}

// ------------------------------------------------------------------ talking to Wave

#[derive(Debug, PartialEq, Eq)]
pub enum WaveErr {
    /// 401 or 403: the key is not accepted.
    Key(String),
    /// 423, 429, 503 or no connection: try again later (seconds from Wave's `Retry-After` when it sent one).
    Later(String, Option<u64>),
    /// A tool refused or the answer had an unexpected shape.
    Other(String),
}

fn rpc(
    http: &dyn Http,
    mcp: &str,
    key: &Key,
    id: u64,
    tool: &str,
    args: Value,
) -> Result<Value, WaveErr> {
    let body = json!({ "jsonrpc": "2.0", "id": id, "method": "tools/call", "params": { "name": tool, "arguments": args } }).to_string();
    let r = match http.send("POST", mcp, Some(key.expose()), Some(&body)) {
        Ok(r) => r,
        Err(ConnectError::Network(m)) => {
            return Err(WaveErr::Later(
                format!("no connection to Wave: {}", scrub(&m)),
                None,
            ));
        }
        Err(e) => return Err(WaveErr::Later(scrub(&e.to_string()), None)),
    };
    match r.status {
        200 => {}
        401 | 403 => {
            return Err(WaveErr::Key(format!(
                "Wave does not accept the key ({})",
                r.status
            )));
        }
        423 => {
            return Err(WaveErr::Later(
                "the agent or the company is paused in Wave (423)".into(),
                r.retry_after.or(Some(60)),
            ));
        }
        429 | 503 => {
            return Err(WaveErr::Later(
                format!("Wave asks to wait ({})", r.status),
                r.retry_after.or(Some(30)),
            ));
        }
        s => return Err(WaveErr::Other(format!("Wave answered {s}"))),
    }
    let j: Value = serde_json::from_str(&r.body)
        .map_err(|_| WaveErr::Other("the answer is not JSON".into()))?;
    if j.get("error").is_some() {
        return Err(WaveErr::Other("protocol error".into()));
    }
    let text = j["result"]["content"]
        .as_array()
        .and_then(|a| {
            a.iter()
                .rev()
                .find_map(|c| c.get("text").and_then(Value::as_str))
        })
        .ok_or_else(|| WaveErr::Other("the answer has no content".into()))?;
    let data: Value = serde_json::from_str(text)
        .map_err(|_| WaveErr::Other("the tool's answer is not JSON".into()))?;
    if j["result"]["isError"].as_bool() == Some(true) {
        let code = data["error"]["code"].as_str().unwrap_or("error");
        return Err(WaveErr::Other(format!(
            "{tool} refused: {}",
            code.chars().take(40).collect::<String>()
        )));
    }
    Ok(data)
}

// ------------------------------------------------------------------ who wakes the agent

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict2 {
    Wake { room: String, entry: String },
    Skip(&'static str),
}

/// Pure: does this inbox item wake the agent?
pub fn classify(
    item: &Value,
    rooms_wanted: &[String],
    names: &BTreeMap<String, String>,
) -> Verdict2 {
    let Some(entry) = item
        .get("id")
        .and_then(Value::as_str)
        .filter(|s| plain_id(s))
    else {
        return Verdict2::Skip("an entry without a plain id");
    };
    let Some(room) = item
        .get("room_id")
        .and_then(Value::as_str)
        .filter(|s| plain_id(s))
    else {
        return Verdict2::Skip("an entry without a plain room id");
    };
    if item.get("untrusted").and_then(Value::as_bool) == Some(true) {
        return Verdict2::Skip("text of outside origin");
    }
    match item.pointer("/author/kind").and_then(Value::as_str) {
        Some("member") => {}
        Some("agent") => return Verdict2::Skip("written by an agent"),
        _ => return Verdict2::Skip("not written by a person of the company"),
    }
    if !rooms_wanted.is_empty() {
        let label = names.get(room).map(String::as_str).unwrap_or("");
        let ok = rooms_wanted.iter().any(|w| {
            w.eq_ignore_ascii_case(room)
                || (!label.is_empty() && w.trim().eq_ignore_ascii_case(label.trim()))
        });
        if !ok {
            return Verdict2::Skip("a room this station does not serve");
        }
    }
    Verdict2::Wake {
        room: room.to_owned(),
        entry: entry.to_owned(),
    }
}

// ------------------------------------------------------------------ state

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct StationState {
    /// The next inbox number to ask for; none on the very first start (then everything old is skipped, not woken).
    pub next_seq: Option<u64>,
    /// Unix times of the runs of the last day (for the limits).
    pub runs: Vec<u64>,
}

impl StationState {
    pub fn load(path: &Path) -> StationState {
        let Some(v) = fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return StationState::default();
        };
        StationState {
            next_seq: v.get("next_seq").and_then(Value::as_u64),
            runs: v
                .get("runs")
                .and_then(Value::as_array)
                .map(|a| a.iter().filter_map(Value::as_u64).collect())
                .unwrap_or_default(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        atomic_write(
            path,
            json!({ "next_seq": self.next_seq, "runs": self.runs })
                .to_string()
                .as_bytes(),
            0o600,
        )
    }

    fn count_since(&self, now: u64, secs: u64) -> u32 {
        self.runs
            .iter()
            .filter(|t| now.saturating_sub(**t) < secs)
            .count() as u32
    }
}

pub struct Paths {
    pub state: PathBuf,
    pub sessions: PathBuf,
    pub mcp: PathBuf,
    pub paused: PathBuf,
    pub runs_log: PathBuf,
}

pub fn paths(dirs: &Dirs, agent: &str) -> Paths {
    Paths {
        state: dirs.state.join(format!("{agent}.station.json")),
        sessions: dirs.state.join(format!("{agent}.sessions.json")),
        mcp: dirs.state.join(format!("{agent}.mcp.json")),
        paused: dirs.state.join(format!("{agent}.station.paused")),
        runs_log: dirs.state.join(format!("{agent}.runs.log")),
    }
}

// ------------------------------------------------------------------ the working folder

/// The working folder is where the wake may read and write. It must not be a place the key or the engine's login live in or above.
pub fn check_workdir(
    work: &Path,
    dirs: &Dirs,
    key_file: Option<&Path>,
) -> Result<Vec<String>, String> {
    if !work.is_absolute() {
        return Err("the working folder must be a full path".into());
    }
    if work.parent().is_none() || work == dirs.home {
        return Err("the working folder cannot be the root or your whole home folder".into());
    }
    let inside = |outer: &Path, inner: &Path| inner.starts_with(outer);
    for guarded in [
        &dirs.config,
        &dirs.state,
        &dirs.codex_home,
        &dirs.home.join(".claude"),
        &dirs.home.join(".ssh"),
        &dirs.home.join(".grok"),
    ] {
        if inside(work, guarded) || inside(guarded, work) {
            return Err(format!(
                "the working folder cannot be or contain {} (it holds keys or logins)",
                super::dirs::text(guarded)
            ));
        }
    }
    if let Some(k) = key_file {
        if inside(work, k) {
            return Err("the key file lies inside the working folder".into());
        }
    }
    fs::create_dir_all(work)
        .map_err(|e| format!("cannot create the working folder: {}", e.kind()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let _ = fs::set_permissions(work, fs::Permissions::from_mode(0o700));
    }
    let canon = fs::canonicalize(work)
        .map_err(|e| format!("cannot read the working folder: {}", e.kind()))?;
    let mut warnings = Vec::new();
    let mut seen = 0usize;
    let mut stack = vec![(work.to_path_buf(), 0u8)];
    while let Some((dir, depth)) = stack.pop() {
        let Ok(rd) = fs::read_dir(&dir) else { continue };
        for e in rd.flatten() {
            seen += 1;
            if seen > 5000 {
                warnings.push("the working folder is large; only the first 5000 entries were checked for links".into());
                stack.clear();
                break;
            }
            let p = e.path();
            let Ok(meta) = fs::symlink_metadata(&p) else {
                continue;
            };
            if meta.file_type().is_symlink() {
                let target = fs::canonicalize(&p).map_err(|_| {
                    format!(
                        "{} is a link that leads nowhere; remove it",
                        super::dirs::text(&p)
                    )
                })?;
                if !target.starts_with(&canon) {
                    return Err(format!(
                        "{} is a link that leads out of the working folder; remove it",
                        super::dirs::text(&p)
                    ));
                }
            } else if meta.is_dir() && depth < 2 {
                stack.push((p, depth + 1));
            }
        }
    }
    for name in ["CLAUDE.md", "AGENTS.md", ".mcp.json", ".claude"] {
        if work.join(name).exists() {
            warnings.push(format!("{name} is in the working folder: the agent reads its instructions, so only use a folder you trust"));
        }
    }
    Ok(warnings)
}

// ------------------------------------------------------------------ the station

pub struct Deps<'a> {
    pub http: &'a dyn Http,
    pub spawn: &'a dyn Spawn,
    pub now: &'a dyn Fn() -> u64,
    /// The whole environment of this process (the run gets only the allowed part of it).
    pub env: &'a dyn Fn() -> Vec<(String, String)>,
    pub which: &'a dyn Fn(&str) -> Option<PathBuf>,
}

#[derive(Debug, Default, PartialEq)]
pub struct Report {
    /// Rooms woken, with how the wake ended.
    pub woke: Vec<(String, String)>,
    pub skipped: Vec<&'static str>,
    /// Something that holds the station back until later (limit, pause, Wave busy). Said once by the loop.
    pub waiting: Option<String>,
    /// Something that needs a person (the key is refused, the engine is not logged in).
    pub fatal: Option<String>,
}

pub struct Station<'a> {
    pub cfg: Config,
    pub deps: Deps<'a>,
    pub paths: Paths,
    key: Key,
    pub state: StationState,
    pub sessions: Sessions,
    pub names: BTreeMap<String, String>,
    rpc_id: u64,
}

impl<'a> Station<'a> {
    pub fn new(cfg: Config, deps: Deps<'a>, paths: Paths, key: Key) -> Station<'a> {
        let state = StationState::load(&paths.state);
        let sessions = Sessions::load(&paths.sessions);
        Station {
            cfg,
            deps,
            paths,
            key,
            state,
            sessions,
            names: BTreeMap::new(),
            rpc_id: 0,
        }
    }

    fn call(&mut self, tool: &str, mut args: Value) -> Result<Value, WaveErr> {
        self.rpc_id += 1;
        args["session"] = json!(format!("station-{}", self.cfg.agent));
        rpc(
            self.deps.http,
            &self.cfg.mcp_url,
            &self.key,
            self.rpc_id,
            tool,
            args,
        )
    }

    fn save_state(&self) {
        let _ = self.state.save(&self.paths.state);
    }

    fn refresh_rooms(&mut self) {
        if let Ok(d) = self.call("rooms", json!({})) {
            for r in d["items"].as_array().into_iter().flatten() {
                if let Some(id) = r["id"].as_str() {
                    let name = r["name"].as_str().or(r["key"].as_str()).unwrap_or("");
                    self.names
                        .insert(id.to_owned(), name.chars().take(80).collect());
                }
            }
        }
    }

    fn write_mcp_file(&self) -> std::io::Result<()> {
        if let Some(d) = self.paths.mcp.parent() {
            fs::create_dir_all(d)?;
        }
        atomic_write(
            &self.paths.mcp,
            claude::mcp_config(&self.cfg.mcp_url).as_bytes(),
            0o600,
        )
    }

    /// The command of one wake, as it would be started.
    pub fn plan(&self, prompt: &str, resume: Option<&str>) -> Result<RunReq, String> {
        let program = super::spawn::find_program(
            self.cfg.program.as_deref(),
            "claude",
            self.deps.which,
        )
        .ok_or(
            "Claude Code (`claude`) is not on the PATH; install it or give its path with --claude",
        )?;
        let env = clean_env(
            (self.deps.env)(),
            &[(KEY_VAR.to_owned(), self.key.expose().to_owned())],
        );
        Ok(RunReq {
            program,
            args: claude::build_args(prompt, &self.paths.mcp, &self.cfg.settings, resume),
            env,
            cwd: self.cfg.workdir.clone(),
            timeout: self.cfg.timeout,
            grace: self.cfg.grace,
        })
    }

    fn limit_hit(&self, now: u64) -> Option<String> {
        if self.state.count_since(now, 3600) >= self.cfg.per_hour {
            return Some(format!(
                "limit of {} wakes per hour reached",
                self.cfg.per_hour
            ));
        }
        if self.state.count_since(now, 86_400) >= self.cfg.per_day {
            return Some(format!(
                "limit of {} wakes per day reached",
                self.cfg.per_day
            ));
        }
        None
    }

    fn log_run(&self, room: &str, ended: &str, v: Option<&Verdict>, secs: u64) {
        let line = format!(
            "{} room={} ended={} secs={} turns={} cost={}\n",
            (self.deps.now)(),
            room,
            ended,
            secs,
            v.and_then(|v| v.turns)
                .map_or("?".into(), |t| t.to_string()),
            v.and_then(|v| v.cost_usd)
                .map_or("?".into(), |c| format!("{c:.4}")),
        );
        if let Some(d) = self.paths.runs_log.parent() {
            let _ = fs::create_dir_all(d);
        }
        let old = fs::read_to_string(&self.paths.runs_log).unwrap_or_default();
        let kept: Vec<&str> = old
            .lines()
            .rev()
            .take(999)
            .collect::<Vec<_>>()
            .into_iter()
            .rev()
            .collect();
        let _ = atomic_write(
            &self.paths.runs_log,
            (kept.join("\n") + if kept.is_empty() { "" } else { "\n" } + &line).as_bytes(),
            0o600,
        );
    }

    /// One run against the room's session. Counts against the limits whether it works or not.
    fn run_once(
        &mut self,
        room: &str,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<(Ended, Option<Verdict>), String> {
        let req = self.plan(prompt, resume)?;
        let now = (self.deps.now)();
        self.state.runs.retain(|t| now.saturating_sub(*t) < 86_400);
        self.state.runs.push(now);
        self.save_state();
        let out = self.deps.spawn.run(&req);
        let verdict = claude::parse_verdict(&out.stdout, &out.stderr);
        let how = match (&out.ended, &verdict) {
            (Ended::TimedOut, _) => "timeout".to_owned(),
            (Ended::CouldNotStart(k), _) => format!("not-started:{k}"),
            (Ended::Exited(c), Some(v)) if !v.is_error && *c == Some(0) => "ok".to_owned(),
            (Ended::Exited(c), _) => format!("failed:{}", c.map_or("?".into(), |c| c.to_string())),
        };
        self.log_run(room, &how, verdict.as_ref(), out.elapsed.as_secs());
        Ok((out.ended, verdict))
    }

    /// The handover: the old session writes a short note, then the room starts a new session that reads it.
    fn rotate(&mut self, room: &str, s: &RoomSession) -> RoomSession {
        let rel = format!("notes/{}-handover.md", room);
        let _ = fs::create_dir_all(self.cfg.workdir.join("notes"));
        let prompt = format!("{} The file: {rel}", claude::HANDOVER_PROMPT);
        let note = match self.run_once(room, &prompt, s.session.as_deref()) {
            Ok((Ended::Exited(Some(0)), Some(v)))
                if !v.is_error && self.cfg.workdir.join(&rel).exists() =>
            {
                Some(rel)
            }
            _ => None,
        };
        RoomSession {
            session: None,
            wakes: 0,
            started: (self.deps.now)(),
            note,
        }
    }

    /// One wake of one room. Returns how it ended, in a word.
    fn wake_room(&mut self, room: &str, entries: &[String]) -> Result<&'static str, String> {
        let mut s = self.sessions.get(room);
        if s.session.is_some() && s.wakes >= self.cfg.session_max_wakes {
            s = self.rotate(room, &s);
            self.sessions.put(room, s.clone());
        }
        let note = s.note.clone();
        let prompt = claude::wake_prompt(room, entries, note.as_deref());
        let mut resume = s.session.clone();
        let mut fresh_fallback_used = false;
        let mut retried = false;
        loop {
            let (ended, v) = self.run_once(room, &prompt, resume.as_deref())?;
            match (&ended, &v) {
                (Ended::TimedOut, _) => return Ok("timeout"),
                (Ended::CouldNotStart(_), _) => return Ok("not-started"),
                (_, Some(v)) if v.login_problem => return Ok("login"),
                (_, Some(v)) if v.session_missing && resume.is_some() && !fresh_fallback_used => {
                    // the session is gone (cleaned up, other machine): a fresh run is the fallback, and its session replaces the lost one
                    fresh_fallback_used = true;
                    resume = None;
                    continue;
                }
                (Ended::Exited(Some(0)), Some(v)) if !v.is_error => {
                    let keep = v.session_id.clone().or(resume.clone());
                    let continued = resume.is_some() && keep.as_deref() == resume.as_deref();
                    self.sessions.put(
                        room,
                        RoomSession {
                            session: keep,
                            wakes: if continued { s.wakes + 1 } else { 1 },
                            started: if continued {
                                s.started
                            } else {
                                (self.deps.now)()
                            },
                            note: if continued { s.note.clone() } else { None },
                        },
                    );
                    let _ = self.sessions.save(&self.paths.sessions);
                    return Ok("ok");
                }
                _ if !retried => {
                    retried = true;
                    continue;
                }
                _ => return Ok("failed"),
            }
        }
    }

    fn post_failure(&mut self, room: &str, why: &str) {
        let body = format!(
            "Station note: I could not finish the run for the message(s) addressed to me here ({why}). Nothing was done. Mention me again to try again."
        );
        let _ = self.call("post", json!({ "room_id": room, "body": body }));
    }

    /// One look at the inbox and the wakes it calls for.
    pub fn cycle(&mut self) -> Report {
        let mut rep = Report::default();
        if self.paths.paused.exists() {
            rep.waiting = Some("paused (`stonewave agent station resume` ends the pause)".into());
            return rep;
        }
        let now = (self.deps.now)();
        let mut pages = 0;
        let baseline = self.state.next_seq.is_none();
        loop {
            let since = self.state.next_seq.unwrap_or(0);
            let data = match self.call("inbox", json!({ "since_seq": since, "limit": PAGE })) {
                Ok(d) => d,
                Err(WaveErr::Key(m)) => {
                    rep.fatal = Some(m);
                    return rep;
                }
                Err(WaveErr::Later(m, _)) | Err(WaveErr::Other(m)) => {
                    rep.waiting = Some(m);
                    return rep;
                }
            };
            let items: Vec<Value> = data["items"].as_array().cloned().unwrap_or_default();
            let next = data["next_seq"].as_u64().unwrap_or(since).max(since);
            if baseline {
                // the first start: what is old stays old; nothing is woken for it
                self.state.next_seq = Some(next);
                self.save_state();
                if (items.len() as u64) < PAGE {
                    return rep;
                }
                continue;
            }
            if items.iter().any(|i| {
                i["room_id"]
                    .as_str()
                    .is_some_and(|r| !self.names.contains_key(r))
            }) && !self.cfg.rooms.is_empty()
            {
                self.refresh_rooms();
            }
            // who is woken, room by room in the order of the first entry
            let mut order: Vec<String> = Vec::new();
            let mut by_room: BTreeMap<String, (Vec<String>, u64)> = BTreeMap::new();
            for it in &items {
                match classify(it, &self.cfg.rooms, &self.names) {
                    Verdict2::Skip(why) => rep.skipped.push(why),
                    Verdict2::Wake { room, entry } => {
                        let seq = it["seq"].as_u64().unwrap_or(next);
                        let e = by_room.entry(room.clone()).or_insert_with(|| {
                            order.push(room.clone());
                            (Vec::new(), seq)
                        });
                        if e.0.len() < 10 {
                            e.0.push(entry);
                        }
                    }
                }
            }
            let mut cursor = next;
            for room in order {
                let (entries, first_seq) = by_room.remove(&room).unwrap_or_default();
                if let Some(why) = self.limit_hit((self.deps.now)()) {
                    rep.waiting = Some(why);
                    cursor = cursor.min(first_seq.saturating_sub(1)).max(since);
                    break;
                }
                match self.wake_room(&room, &entries) {
                    Ok("login") => {
                        rep.fatal = Some("Claude Code is not logged in for this user (or its login ran out); log in once with `claude auth login`".into());
                        cursor = cursor.min(first_seq.saturating_sub(1)).max(since);
                        break;
                    }
                    Ok(how) => {
                        if matches!(how, "timeout" | "failed" | "not-started") {
                            let why = match how {
                                "timeout" => "time limit",
                                "not-started" => "the engine could not be started",
                                _ => "the engine ended with an error",
                            };
                            self.post_failure(&room, why);
                        }
                        rep.woke.push((room.clone(), how.to_owned()));
                    }
                    Err(m) => {
                        rep.fatal = Some(m);
                        cursor = cursor.min(first_seq.saturating_sub(1)).max(since);
                        break;
                    }
                }
            }
            self.state.next_seq = Some(cursor);
            self.state.runs.retain(|t| {
                now.saturating_sub(*t) < 86_400 || (self.deps.now)().saturating_sub(*t) < 86_400
            });
            self.save_state();
            pages += 1;
            if rep.waiting.is_some()
                || rep.fatal.is_some()
                || (items.len() as u64) < PAGE
                || pages >= MAX_PAGES
            {
                return rep;
            }
        }
    }

    /// What `--dry-run` shows: the exact command of a wake and everything around it, without starting anything or asking Wave.
    pub fn dry_run_text(&self) -> String {
        let mut o = String::new();
        let fake_room = "ROOM-ID";
        let prompt = claude::wake_prompt(fake_room, &["ENTRY-ID".to_owned()], None);
        o.push_str(&format!(
            "agent        {}\nWave (MCP)   {}\n",
            self.cfg.agent, self.cfg.mcp_url
        ));
        o.push_str(&format!(
            "key          {}\n",
            match &self.cfg.key {
                KeySource::File(p) => format!(
                    "file {} (read when the station starts, never written anywhere else)",
                    super::dirs::text(p)
                ),
                KeySource::Env(n) =>
                    format!("environment variable {n} of this process (never written to a disk)"),
            }
        ));
        o.push_str(&format!(
            "work folder  {}\n",
            super::dirs::text(&self.cfg.workdir)
        ));
        o.push_str(&format!(
            "rooms        {}\n",
            if self.cfg.rooms.is_empty() {
                "every room the agent may read".to_owned()
            } else {
                self.cfg.rooms.join(", ")
            }
        ));
        o.push_str(
            "wakes by     people of the company only (never agents, never text from outside)\n",
        );
        o.push_str(&format!(
            "limits       {} min per wake, {} turns, {} per wake, {} wakes per hour, {} per day, one at a time; a room's session ends after {} wakes\n",
            self.cfg.timeout.as_secs() / 60,
            self.cfg.settings.max_turns,
            self.cfg.settings.budget_usd.map_or("no cost cap".to_owned(), |b| format!("{b:.2} USD (an estimate)")),
            self.cfg.per_hour,
            self.cfg.per_day,
            self.cfg.session_max_wakes
        ));
        match self.plan(&prompt, Some("SESSION-ID-OF-THIS-ROOM")) {
            Ok(r) => {
                o.push_str(&format!(
                    "\nthe command of a wake that continues the room's session:\n  {}\n",
                    shell_words(&r.program, &r.args)
                ));
                let names: Vec<&str> = r.env.iter().map(|(k, _)| k.as_str()).collect();
                o.push_str(&format!(
                    "environment it gets (names only, nothing else of yours): {}\n",
                    names.join(", ")
                ));
                o.push_str(&format!("runs in: {}\n", super::dirs::text(&r.cwd)));
            }
            Err(e) => o.push_str(&format!("\ncannot plan a wake: {e}\n")),
        }
        o.push_str("\nno command tools (--restricted, Bash and web denied), one MCP server (wave), nothing is started by --dry-run\n");
        o
    }
}

/// A command for people to read (not to paste into a shell): each argument quoted when it has spaces.
fn shell_words(program: &Path, args: &[String]) -> String {
    let q = |s: &str| {
        if s.is_empty() || s.contains([' ', '"', '\'', '$']) {
            format!("\"{}\"", s.replace('"', "\\\""))
        } else {
            s.to_owned()
        }
    };
    std::iter::once(q(&super::dirs::text(program)))
        .chain(args.iter().map(|a| q(a)))
        .collect::<Vec<_>>()
        .join(" ")
}

/// Sets the paths and the MCP file up and checks the folder. Called before the first cycle.
pub fn prepare(st: &Station<'_>, dirs: &Dirs) -> Result<Vec<String>, String> {
    fs::create_dir_all(&dirs.state).map_err(|e| {
        format!(
            "cannot create {}: {}",
            super::dirs::text(&dirs.state),
            e.kind()
        )
    })?;
    let key_file = match &st.cfg.key {
        KeySource::File(p) => Some(p.as_path()),
        KeySource::Env(_) => None,
    };
    let warnings = check_workdir(&st.cfg.workdir, dirs, key_file)?;
    st.write_mcp_file()
        .map_err(|e| format!("cannot write the MCP file: {}", e.kind()))?;
    Ok(warnings)
}

#[cfg(test)]
mod tests;
