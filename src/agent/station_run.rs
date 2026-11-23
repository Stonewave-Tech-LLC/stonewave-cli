//! The station that runs: it asks Wave's inbox for what is addressed to the agent, and for each room where a person of the company
//! mentioned it, **continues that room's session** of the engine (Claude Code) with one wake: a fixed text with ids, strict configuration, limits.
//!
//! Who wakes it: only a person of the company (author kind `member`, text not of outside origin), never an agent, and only in the rooms it was
//! told to serve (all rooms it may read if none were named). Everything else is skipped and said with its reason, never with its text.
//!
//! What it never does: put the text of a message into a prompt, a log or a line; start a run without the limits below; start a second run at the
//! same time (also not from a second station of the same agent: a lock file); hand the run more of the environment than
//! [`spawn::clean_env`] lets through; write the key anywhere.
//!
//! Each room has its own folder inside the working folder (`<workdir>/rooms/<room>`): a run sees only that folder, so what one room's session
//! writes is not read by another room's. Claude Code also reads a `CLAUDE.md` of the parent folders, which is where shared instructions go.
//!
//! Slice 1 polls the inbox (every `poll`, 15 s by default). The live line to Wave's doorbell is slice 2.

use std::collections::{BTreeMap, BTreeSet};
use std::fs;
use std::path::{Component, Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::thread;
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
const MAX_BASELINE_PAGES: usize = 50;
/// Entries a woken room is remembered for, so a wake is never repeated for the same entry.
const DONE_KEEP: usize = 300;
const NOTE: &str = "notes/handover.md";

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
    /// An environment variable of the station's own process (for example from a secret-store command); the key never touches a disk.
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
            let v = env(name).ok_or_else(|| format!("the variable {name} is not set (start the station through your secret-store command or export it)"))?;
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

/// The rooms the agent may read, as `(id, name)`: what the menu offers when a station is set up. Rooms without a plain id are left out.
pub fn list_rooms(http: &dyn Http, mcp: &str, key: &Key) -> Result<Vec<(String, String)>, WaveErr> {
    let d = rpc(http, mcp, key, 1, "rooms", json!({}))?;
    Ok(d["items"]
        .as_array()
        .into_iter()
        .flatten()
        .filter_map(|r| {
            let id = r["id"].as_str().filter(|s| plain_id(s))?;
            let name = r["name"]
                .as_str()
                .filter(|s| !s.trim().is_empty())
                .or_else(|| r["key"].as_str())
                .unwrap_or(id);
            Some((
                id.to_owned(),
                name.chars().filter(|c| !c.is_control()).take(80).collect(),
            ))
        })
        .collect())
}

// ------------------------------------------------------------------ who wakes the agent

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict2 {
    Wake { room: String, entry: String },
    Skip(&'static str),
}

/// Pure: does this inbox item wake the agent? Fails closed: an item that does not say plainly that it is not of outside origin does not wake.
pub fn classify(item: &Value, rooms: &BTreeSet<String>) -> Verdict2 {
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
    match item.get("untrusted").and_then(Value::as_bool) {
        Some(false) => {}
        Some(true) => return Verdict2::Skip("text of outside origin"),
        None => return Verdict2::Skip("an entry that does not say where its text comes from"),
    }
    match item.pointer("/author/kind").and_then(Value::as_str) {
        Some("member") => {}
        Some("agent") => return Verdict2::Skip("written by an agent"),
        _ => return Verdict2::Skip("not written by a person of the company"),
    }
    // rooms are matched by their id (the names were turned into ids once, at the start): a room that is renamed, or a new room with the same
    // name, is not served by accident
    if !rooms.contains(room) {
        return Verdict2::Skip("a room this station does not serve");
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
    /// Entries already woken for (ids only), so a page that is asked for again does not wake twice.
    pub done: Vec<String>,
}

impl StationState {
    pub fn load(path: &Path) -> StationState {
        let Some(v) = fs::read_to_string(path)
            .ok()
            .and_then(|t| serde_json::from_str::<Value>(&t).ok())
        else {
            return StationState::default();
        };
        let list = |k: &str| -> Vec<Value> {
            v.get(k)
                .and_then(Value::as_array)
                .cloned()
                .unwrap_or_default()
        };
        StationState {
            next_seq: v.get("next_seq").and_then(Value::as_u64),
            runs: list("runs").iter().filter_map(Value::as_u64).collect(),
            done: list("done")
                .iter()
                .filter_map(Value::as_str)
                .filter(|s| plain_id(s))
                .map(str::to_owned)
                .collect(),
        }
    }

    pub fn save(&self, path: &Path) -> std::io::Result<()> {
        atomic_write(
            path,
            json!({ "next_seq": self.next_seq, "runs": self.runs, "done": self.done })
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

    fn remember(&mut self, entries: &[String]) {
        self.done.extend(entries.iter().cloned());
        let n = self.done.len();
        if n > DONE_KEEP {
            self.done.drain(..n - DONE_KEEP);
        }
    }
}

pub struct Paths {
    pub state: PathBuf,
    pub sessions: PathBuf,
    pub mcp: PathBuf,
    pub paused: PathBuf,
    pub runs_log: PathBuf,
    pub lock: PathBuf,
    /// The process id of the wake that is going (written by the spawner).
    pub child: PathBuf,
}

pub fn paths(dirs: &Dirs, agent: &str) -> Paths {
    Paths {
        state: dirs.state.join(format!("{agent}.station.json")),
        sessions: dirs.state.join(format!("{agent}.sessions.json")),
        mcp: dirs.state.join(format!("{agent}.mcp.json")),
        paused: dirs.state.join(format!("{agent}.station.paused")),
        runs_log: dirs.state.join(format!("{agent}.runs.log")),
        lock: dirs.state.join(format!("{agent}.station.lock")),
        child: dirs.state.join(format!("{agent}.station.child")),
    }
}

// ------------------------------------------------------------------ one station per agent

/// A lock file that a running station keeps fresh: a second station for the same agent would share the inbox position and wake twice.
pub struct Lock {
    path: PathBuf,
    stop: Arc<AtomicBool>,
}

const LOCK_FRESH: Duration = Duration::from_secs(90);
const LOCK_BEAT: Duration = Duration::from_secs(15);

/// Whether a process with this id exists (Linux and macOS; elsewhere the age of the lock file decides).
fn pid_alive(pid: u32) -> bool {
    #[cfg(unix)]
    {
        if pid == 0 || pid > i32::MAX as u32 {
            return false;
        }
        // SAFETY: signal 0 only checks that the process exists and may be signalled.
        let r = unsafe { libc::kill(pid as i32, 0) };
        r == 0 || std::io::Error::last_os_error().raw_os_error() == Some(libc::EPERM)
    }
    #[cfg(not(unix))]
    {
        let _ = pid;
        false
    }
}

/// A wake of an earlier station that is still going (the station was killed hard and its run was left behind): a new wake must not start next
/// to it. Nothing is ended here; the person decides. A stale file of a run that is over is removed.
pub fn previous_wake_running(child_file: &Path) -> Option<u32> {
    let pid: u32 = fs::read_to_string(child_file).ok()?.trim().parse().ok()?;
    if pid_alive(pid) {
        Some(pid)
    } else {
        let _ = fs::remove_file(child_file);
        None
    }
}

/// Whether a station is running for this lock file: its process exists (Linux and macOS), or the file was touched in the last 90 seconds.
pub fn station_running(lock: &Path) -> bool {
    let Ok(who) = fs::read_to_string(lock) else {
        return false;
    };
    match who.trim().parse::<u32>() {
        Ok(p) if cfg!(unix) => pid_alive(p),
        _ => fs::metadata(lock)
            .ok()
            .and_then(|m| m.modified().ok())
            .and_then(|t| t.elapsed().ok())
            .is_some_and(|age| age < LOCK_FRESH),
    }
}

fn create_lock(path: &Path, pid: &str) -> Result<(), String> {
    use std::io::Write as _;
    let mut f = fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(path)
        .map_err(|_| "another station for this agent has just started".to_owned())?;
    f.write_all(pid.as_bytes())
        .map_err(|e| format!("cannot write the lock file: {}", e.kind()))
}

impl Lock {
    /// The lock file is made in one step that fails if it exists. One that exists blocks while its station is alive (its process id is still
    /// there on Linux and macOS; elsewhere: its file was touched in the last 90 seconds); otherwise it is replaced.
    pub fn acquire(path: &Path) -> Result<Lock, String> {
        let pid = std::process::id().to_string();
        if create_lock(path, &pid).is_err() {
            let who = fs::read_to_string(path).unwrap_or_default();
            let fresh = fs::metadata(path)
                .ok()
                .and_then(|m| m.modified().ok())
                .and_then(|t| t.elapsed().ok())
                .is_some_and(|age| age < LOCK_FRESH);
            let alive = match who.trim().parse::<u32>() {
                Ok(p) if cfg!(unix) => pid_alive(p),
                _ => fresh,
            };
            if alive {
                return Err(format!(
                    "another station for this agent is running (process {}); stop it first. If you are sure it is gone, delete {}",
                    who.trim().chars().take(12).collect::<String>(),
                    super::dirs::text(path)
                ));
            }
            // nobody keeps it: replace it, in a step that fails if a second new station does the same at the same moment
            let _ = fs::remove_file(path);
            create_lock(path, &pid)?;
        }
        let stop = Arc::new(AtomicBool::new(false));
        let (p2, s2) = (path.to_path_buf(), stop.clone());
        thread::spawn(move || {
            let mut waited = Duration::ZERO;
            while !s2.load(Ordering::SeqCst) {
                thread::sleep(Duration::from_millis(250));
                waited += Duration::from_millis(250);
                if waited >= LOCK_BEAT {
                    waited = Duration::ZERO;
                    let _ = atomic_write(&p2, pid.as_bytes(), 0o600);
                }
            }
        });
        Ok(Lock {
            path: path.to_path_buf(),
            stop,
        })
    }
}

impl Drop for Lock {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::SeqCst);
        let _ = fs::remove_file(&self.path);
    }
}

// ------------------------------------------------------------------ the working folder

fn same_place(a: &Path, b: &Path) -> bool {
    if cfg!(any(windows, target_os = "macos")) {
        a.to_string_lossy().to_lowercase() == b.to_string_lossy().to_lowercase()
    } else {
        a == b
    }
}

fn starts_with_place(path: &Path, prefix: &Path) -> bool {
    let (p, q): (Vec<_>, Vec<_>) = (path.components().collect(), prefix.components().collect());
    p.len() >= q.len()
        && p.iter()
            .zip(q.iter())
            .all(|(x, y)| same_place(Path::new(x.as_os_str()), Path::new(y.as_os_str())))
}

/// The real place of a path, or the path itself where it does not exist (yet).
fn real(p: &Path) -> PathBuf {
    let mut rest: Vec<std::ffi::OsString> = Vec::new();
    let mut cur = p.to_path_buf();
    loop {
        if let Ok(c) = fs::canonicalize(&cur) {
            return rest.iter().rev().fold(c, |a, r| a.join(r));
        }
        match (cur.file_name().map(ToOwned::to_owned), cur.parent()) {
            (Some(n), Some(par)) => {
                rest.push(n);
                cur = par.to_path_buf();
            }
            _ => return p.to_path_buf(),
        }
    }
}

/// The working folder is where the wake may read and write. It must not be a place the key or the engine's login live in or above, and it is
/// compared by its real place (links, `..` and capital letters do not get around it).
pub fn check_workdir(
    work: &Path,
    dirs: &Dirs,
    key_file: Option<&Path>,
) -> Result<Vec<String>, String> {
    if !work.is_absolute() {
        return Err("the working folder must be a full path".into());
    }
    if work.components().any(|c| matches!(c, Component::ParentDir)) {
        return Err("the working folder cannot contain `..`".into());
    }
    let canon = real(work);
    if canon.parent().is_none() || same_place(&canon, &real(&dirs.home)) {
        return Err("the working folder cannot be the root or your whole home folder".into());
    }
    let guarded_homes = [
        ".claude",
        ".ssh",
        ".grok",
        ".aws",
        ".gnupg",
        ".kube",
        ".docker",
        ".config",
        ".npm",
        ".cargo",
        "Library",
        "AppData",
        ".local/share/keyrings",
        ".local/share/gnome-keyring",
    ];
    let mut guarded: Vec<PathBuf> = vec![
        dirs.config.clone(),
        dirs.state.clone(),
        dirs.codex_home.clone(),
    ];
    guarded.extend(guarded_homes.iter().map(|n| dirs.home.join(n)));
    for g in guarded {
        let g = real(&g);
        // the folders under the user's own config and state folders that are NOT keys are fine only if they are the agent's room folders;
        // `.config` and `.local` are guarded as a whole unless the working folder is nowhere in them
        if starts_with_place(&canon, &g) || starts_with_place(&g, &canon) {
            return Err(format!(
                "the working folder cannot be or contain {} (it holds keys or logins)",
                super::dirs::text(&g)
            ));
        }
    }
    if let Some(k) = key_file {
        if starts_with_place(&real(k), &canon) {
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
                if !starts_with_place(&target, &canon) {
                    return Err(format!(
                        "{} is a link that leads out of the working folder; remove it",
                        super::dirs::text(&p)
                    ));
                }
            } else if meta.is_dir() && depth < 3 {
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

/// What a folder may not carry for a wake to start in it: settings, hooks or MCP servers of their own, or a local instruction file that can hold them.
/// Looked for in the room's folder and in every folder above it up to the working folder.
const PLANTABLE: &[&str] = &[".claude", ".mcp.json", "CLAUDE.local.md"];

pub fn settings_found(room_dir: &Path, workdir: &Path) -> Vec<String> {
    let mut out = Vec::new();
    let mut cur = Some(room_dir);
    while let Some(d) = cur {
        for n in PLANTABLE {
            if fs::symlink_metadata(d.join(n)).is_ok() {
                out.push(format!(
                    "{n} in {}",
                    d.file_name()
                        .map_or("?".into(), |f| f.to_string_lossy().into_owned())
                ));
            }
        }
        if d == workdir {
            break;
        }
        cur = d.parent();
    }
    out
}

/// A wake only gets to write inside its room folder: whatever of that kind appeared there is removed.
pub fn strip_planted(room_dir: &Path) -> Vec<String> {
    let mut removed = Vec::new();
    for n in PLANTABLE {
        let p = room_dir.join(n);
        let Ok(meta) = fs::symlink_metadata(&p) else {
            continue;
        };
        let done = if meta.is_dir() {
            fs::remove_dir_all(&p).is_ok()
        } else {
            fs::remove_file(&p).is_ok()
        };
        if done {
            removed.push((*n).to_owned());
        }
    }
    removed
}

// ------------------------------------------------------------------ the station

pub struct Deps<'a> {
    pub http: &'a dyn Http,
    pub spawn: &'a dyn Spawn,
    pub now: &'a dyn Fn() -> u64,
    /// The whole environment of this process (the run gets only the allowed part of it).
    pub env: &'a dyn Fn() -> Vec<(String, String)>,
    pub which: &'a dyn Fn(&str) -> Option<PathBuf>,
    /// Set by a signal: a run that is going is ended and the station stops.
    pub stop: &'a AtomicBool,
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
    /// The waiting is a problem on the way to Wave (not a pause or a limit): wait longer and longer.
    pub backoff: bool,
    /// Seconds Wave asked to wait.
    pub wait_secs: Option<u64>,
    /// Things to say once (the rooms that are served, settings a wake wrote that were removed).
    pub notes: Vec<String>,
}

enum RoomsErr {
    Fatal(String),
    Wait(String, Option<u64>),
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Outcome {
    Ok,
    Timeout,
    Failed,
    NotStarted,
    Login,
    TooOld,
    Stopped,
    FolderRefused,
}

impl Outcome {
    fn word(self) -> &'static str {
        match self {
            Outcome::Ok => "ok",
            Outcome::Timeout => "timeout",
            Outcome::Failed => "failed",
            Outcome::NotStarted => "not-started",
            Outcome::Login => "login",
            Outcome::TooOld => "too-old",
            Outcome::Stopped => "stopped",
            Outcome::FolderRefused => "folder-refused",
        }
    }
}

struct Ran {
    ended: Ended,
    verdict: Option<Verdict>,
    /// Claude Code did not know a flag (it is older than the station needs).
    usage_error: bool,
}

pub struct Station<'a> {
    pub cfg: Config,
    pub deps: Deps<'a>,
    pub dirs: Dirs,
    pub paths: Paths,
    key: Key,
    pub state: StationState,
    pub sessions: Sessions,
    /// The ids of the rooms that are served (the names of `Config::rooms` turned into ids once).
    pub room_ids: BTreeSet<String>,
    rooms_resolved: bool,
    notes: Vec<String>,
    rpc_id: u64,
}

impl<'a> Station<'a> {
    pub fn new(cfg: Config, deps: Deps<'a>, dirs: Dirs, paths: Paths, key: Key) -> Station<'a> {
        let state = StationState::load(&paths.state);
        let sessions = Sessions::load(&paths.sessions);
        Station {
            cfg,
            deps,
            dirs,
            paths,
            key,
            state,
            sessions,
            room_ids: BTreeSet::new(),
            rooms_resolved: false,
            notes: Vec::new(),
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

    /// Turns the rooms the person named (a name, a short name or an id) into ids, once. A name that matches no room the agent can read, or more than
    /// one, is refused: serving a room by a guess is worse than not starting.
    fn resolve_rooms(&mut self) -> Result<(), RoomsErr> {
        if self.cfg.rooms.is_empty() {
            return Err(RoomsErr::Fatal(
                "no rooms to serve: name them with --rooms (a station that answers in every room it can read is not the default)".into(),
            ));
        }
        let d = match self.call("rooms", json!({})) {
            Ok(d) => d,
            Err(WaveErr::Key(m)) => return Err(RoomsErr::Fatal(m)),
            Err(WaveErr::Later(m, secs)) => return Err(RoomsErr::Wait(m, secs)),
            Err(WaveErr::Other(m)) => return Err(RoomsErr::Wait(m, None)),
        };
        let rooms: Vec<(String, String, String)> = d["items"]
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(|r| {
                let id = r["id"].as_str().filter(|s| plain_id(s))?;
                let name = r["name"].as_str().unwrap_or("");
                let key = r["key"].as_str().unwrap_or("");
                Some((
                    id.to_owned(),
                    name.chars().take(80).collect(),
                    key.chars().take(80).collect(),
                ))
            })
            .collect();
        let mut ids = BTreeSet::new();
        for want in self.cfg.rooms.clone() {
            let w = want.trim();
            let by_id: Vec<&(String, String, String)> = rooms
                .iter()
                .filter(|r| r.0.eq_ignore_ascii_case(w))
                .collect();
            let found: Vec<&(String, String, String)> = if by_id.is_empty() {
                rooms
                    .iter()
                    .filter(|r| {
                        r.1.trim().eq_ignore_ascii_case(w)
                            || (!r.2.is_empty() && r.2.trim().eq_ignore_ascii_case(w))
                    })
                    .collect()
            } else {
                by_id
            };
            match found.as_slice() {
                [one] => {
                    self.notes.push(format!(
                        "serving room {} ({})",
                        if one.1.is_empty() { &one.2 } else { &one.1 },
                        one.0.chars().take(8).collect::<String>()
                    ));
                    ids.insert(one.0.clone());
                }
                [] => {
                    let known: Vec<&str> = rooms.iter().map(|r| r.1.as_str()).take(20).collect();
                    return Err(RoomsErr::Fatal(format!(
                        "no room called `{}` that the agent can read (it can read: {})",
                        scrub(w),
                        known.join(", ")
                    )));
                }
                _ => {
                    return Err(RoomsErr::Fatal(format!(
                        "several rooms are called `{}`: name the one you mean by its id",
                        scrub(w)
                    )));
                }
            }
        }
        self.room_ids = ids;
        self.rooms_resolved = true;
        Ok(())
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

    /// The folder of one room: a run sees only this one.
    pub fn room_dir(&self, room: &str) -> PathBuf {
        self.cfg.workdir.join("rooms").join(room)
    }

    /// The command of one wake, as it would be started.
    pub fn plan(&self, prompt: &str, resume: Option<&str>, cwd: &Path) -> Result<RunReq, String> {
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
            cwd: cwd.to_path_buf(),
            timeout: self.cfg.timeout,
            grace: self.cfg.grace,
            pid_file: Some(self.paths.child.clone()),
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
        cwd: &Path,
        prompt: &str,
        resume: Option<&str>,
    ) -> Result<Ran, String> {
        let req = self.plan(prompt, resume, cwd)?;
        let now = (self.deps.now)();
        self.state.runs.retain(|t| now.saturating_sub(*t) < 86_400);
        self.state.runs.push(now);
        self.save_state();
        let out = self.deps.spawn.run(&req, self.deps.stop);
        let verdict = claude::parse_verdict(&out.stdout, &out.stderr);
        let usage_error =
            verdict.is_none() && matches!(out.ended, Ended::Exited(Some(c)) if c != 0) && {
                let e = out.stderr.to_lowercase();
                e.contains("unknown option")
                    || e.contains("unknown argument")
                    || e.contains("unrecognized")
            };
        let how = match (&out.ended, &verdict) {
            (Ended::TimedOut, _) => "timeout".to_owned(),
            (Ended::Stopped, _) => "stopped".to_owned(),
            (Ended::CouldNotStart(k), _) => format!("not-started:{k}"),
            (Ended::Exited(c), Some(v)) if !v.is_error && *c == Some(0) => "ok".to_owned(),
            (Ended::Exited(c), _) => format!("failed:{}", c.map_or("?".into(), |c| c.to_string())),
        };
        self.log_run(room, &how, verdict.as_ref(), out.elapsed.as_secs());
        Ok(Ran {
            ended: out.ended,
            verdict,
            usage_error,
        })
    }

    /// The handover: the old session writes a short note, then the room starts a new session that reads it. `None` = not now (limit, or the old
    /// session could not write the note): the session simply goes on and the next wake tries again.
    fn rotate(&mut self, room: &str, s: &RoomSession, dir: &Path) -> Option<RoomSession> {
        if self.limit_hit((self.deps.now)()).is_some() || self.deps.stop.load(Ordering::SeqCst) {
            return None;
        }
        let note = dir.join(NOTE);
        let _ = fs::remove_file(&note);
        let _ = fs::create_dir_all(dir.join("notes"));
        let prompt = format!("{} The file: {NOTE}", claude::HANDOVER_PROMPT);
        let ran = self
            .run_once(room, dir, &prompt, s.session.as_deref())
            .ok()?;
        match (&ran.ended, &ran.verdict) {
            (Ended::Exited(Some(0)), Some(v)) if !v.is_error && note.exists() => {
                Some(RoomSession {
                    session: None,
                    wakes: 0,
                    started: (self.deps.now)(),
                    note: Some(NOTE.to_owned()),
                })
            }
            _ => None,
        }
    }

    /// One wake of one room. Before it: a folder that carries settings, hooks or MCP servers of its own is refused. After it: anything of that kind the
    /// wake wrote is removed (it would be read on the next resume).
    fn wake_room(&mut self, room: &str, entries: &[String]) -> Result<Outcome, String> {
        let dir = self.room_dir(room);
        let found = settings_found(&dir, &self.cfg.workdir);
        if !found.is_empty() {
            self.notes.push(format!(
                "room {} is not woken: its folder (or one above it) has {}; remove it, a wake must not start with settings of its own",
                room.chars().take(8).collect::<String>(),
                found.join(", ")
            ));
            return Ok(Outcome::FolderRefused);
        }
        let outcome = self.wake_room_inner(room, entries);
        let removed = strip_planted(&dir);
        if !removed.is_empty() {
            self.notes.push(format!(
                "removed {} that a wake wrote into the folder of room {}",
                removed.join(", "),
                room.chars().take(8).collect::<String>()
            ));
        }
        outcome
    }

    fn wake_room_inner(&mut self, room: &str, entries: &[String]) -> Result<Outcome, String> {
        let key_file = match &self.cfg.key {
            KeySource::File(p) => Some(p.clone()),
            KeySource::Env(_) => None,
        };
        // a folder that is not acceptable (a link that leads out, say) stops THIS room, not the whole station
        if check_workdir(&self.cfg.workdir, &self.dirs, key_file.as_deref()).is_err() {
            return Ok(Outcome::FolderRefused);
        }
        let dir = self.room_dir(room);
        fs::create_dir_all(&dir)
            .map_err(|e| format!("cannot create the room folder: {}", e.kind()))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let _ = fs::set_permissions(&dir, fs::Permissions::from_mode(0o700));
        }
        let mut s = self.sessions.get(room);
        if s.session.is_some() && s.wakes >= self.cfg.session_max_wakes {
            if let Some(ns) = self.rotate(room, &s, &dir) {
                s = ns;
                self.sessions.put(room, s.clone());
                let _ = self.sessions.save(&self.paths.sessions);
            }
        }
        let note = s.note.clone().filter(|n| claude::plain_note(n));
        let prompt = claude::wake_prompt(room, entries, note.as_deref());
        let mut resume = s.session.clone();
        let mut fresh_fallback_used = false;
        let mut retried = false;
        loop {
            let ran = self.run_once(room, &dir, &prompt, resume.as_deref())?;
            if ran.usage_error {
                return Ok(Outcome::TooOld);
            }
            match (&ran.ended, &ran.verdict) {
                (Ended::Stopped, _) => return Ok(Outcome::Stopped),
                (Ended::TimedOut, _) => return Ok(Outcome::Timeout),
                (Ended::CouldNotStart(_), _) => return Ok(Outcome::NotStarted),
                (_, Some(v)) if v.login_problem => return Ok(Outcome::Login),
                (_, Some(v)) if v.session_missing && resume.is_some() && !fresh_fallback_used => {
                    // the session is gone (cleaned up, other machine): a fresh run is the fallback, and its session replaces the lost one
                    fresh_fallback_used = true;
                    resume = None;
                    continue;
                }
                (Ended::Exited(Some(0)), Some(v)) if !v.is_error => {
                    let continued = resume.is_some();
                    self.sessions.put(
                        room,
                        RoomSession {
                            session: v.session_id.clone().or(resume.clone()),
                            wakes: if continued {
                                s.wakes.saturating_add(1)
                            } else {
                                1
                            },
                            started: if continued {
                                s.started
                            } else {
                                (self.deps.now)()
                            },
                            note: if continued { s.note.clone() } else { None },
                        },
                    );
                    let _ = self.sessions.save(&self.paths.sessions);
                    return Ok(Outcome::Ok);
                }
                // a run that already did something (it may have posted) is not repeated; one that never got going is, once
                (Ended::Exited(Some(c)), v)
                    if *c != 0
                        && !retried
                        && v.as_ref().and_then(|v| v.turns).unwrap_or(0) == 0 =>
                {
                    retried = true;
                    continue;
                }
                _ => return Ok(Outcome::Failed),
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
        let mut rep = self.cycle_inner();
        rep.notes.append(&mut self.notes);
        rep
    }

    fn cycle_inner(&mut self) -> Report {
        let mut rep = Report::default();
        if self.paths.paused.exists() {
            rep.waiting = Some("paused (`stonewave agent station resume` ends the pause)".into());
            return rep;
        }
        if !self.rooms_resolved {
            match self.resolve_rooms() {
                Ok(()) => rep.notes.append(&mut self.notes),
                Err(RoomsErr::Fatal(m)) => {
                    rep.fatal = Some(m);
                    return rep;
                }
                Err(RoomsErr::Wait(m, secs)) => {
                    rep.waiting = Some(m);
                    rep.backoff = true;
                    rep.wait_secs = secs;
                    return rep;
                }
            }
        }
        let mut pages = 0;
        loop {
            let baseline = self.state.next_seq.is_none();
            let since = self.state.next_seq.unwrap_or(0);
            let data = match self.call("inbox", json!({ "since_seq": since, "limit": PAGE })) {
                Ok(d) => d,
                Err(WaveErr::Key(m)) => {
                    rep.fatal = Some(m);
                    return rep;
                }
                Err(WaveErr::Later(m, secs)) => {
                    rep.waiting = Some(m);
                    rep.backoff = true;
                    rep.wait_secs = secs;
                    return rep;
                }
                Err(WaveErr::Other(m)) => {
                    rep.waiting = Some(m);
                    rep.backoff = true;
                    return rep;
                }
            };
            let items: Vec<Value> = data["items"].as_array().cloned().unwrap_or_default();
            let next = data["next_seq"].as_u64().unwrap_or(since).max(since);
            let full = (items.len() as u64) >= PAGE;
            if baseline {
                // the first start: what is old stays old; nothing is woken for it
                self.state.next_seq = Some(next);
                self.save_state();
                pages += 1;
                if !full || pages >= MAX_BASELINE_PAGES {
                    return rep;
                }
                continue;
            }
            // who is woken, room by room in the order of the first entry
            let mut order: Vec<String> = Vec::new();
            let mut by_room: BTreeMap<String, (Vec<String>, u64)> = BTreeMap::new();
            for it in &items {
                match classify(it, &self.room_ids) {
                    Verdict2::Skip(why) => rep.skipped.push(why),
                    Verdict2::Wake { room, entry } => {
                        if self.state.done.contains(&entry) {
                            continue;
                        }
                        let seq = it["seq"].as_u64().unwrap_or(next);
                        let e = by_room.entry(room.clone()).or_insert_with(|| {
                            order.push(room.clone());
                            (Vec::new(), seq)
                        });
                        e.0.push(entry);
                    }
                }
            }
            let mut cursor = next;
            for room in order {
                let (entries, first_seq) = by_room.remove(&room).unwrap_or_default();
                let held = first_seq.saturating_sub(1).max(since);
                if self.deps.stop.load(Ordering::SeqCst) {
                    rep.waiting = Some("stopping".into());
                    cursor = held;
                    break;
                }
                if let Some(why) = self.limit_hit((self.deps.now)()) {
                    rep.waiting = Some(why);
                    cursor = held;
                    break;
                }
                match self.wake_room(&room, &entries) {
                    Ok(Outcome::Login) => {
                        rep.fatal = Some("Claude Code is not logged in for this user (or its login ran out); log in once with `claude auth login`".into());
                        cursor = held;
                        break;
                    }
                    Ok(Outcome::TooOld) => {
                        rep.fatal = Some(format!(
                            "this Claude Code does not know a flag the station needs; the station needs Claude Code {}.{}.{} or newer",
                            claude::MIN_VERSION.0,
                            claude::MIN_VERSION.1,
                            claude::MIN_VERSION.2
                        ));
                        cursor = held;
                        break;
                    }
                    Ok(Outcome::Stopped) => {
                        rep.waiting = Some("stopping".into());
                        cursor = held;
                        break;
                    }
                    Ok(o) => {
                        if matches!(
                            o,
                            Outcome::Timeout
                                | Outcome::Failed
                                | Outcome::NotStarted
                                | Outcome::FolderRefused
                        ) {
                            let why = match o {
                                Outcome::Timeout => "time limit",
                                Outcome::NotStarted => "the engine could not be started",
                                Outcome::FolderRefused => {
                                    "the working folder is not acceptable: ask whoever set the station up"
                                }
                                _ => "the engine ended with an error",
                            };
                            self.post_failure(&room, why);
                        }
                        self.state.remember(&entries);
                        rep.woke.push((room.clone(), o.word().to_owned()));
                    }
                    Err(m) => {
                        rep.fatal = Some(m);
                        cursor = held;
                        break;
                    }
                }
            }
            self.state.next_seq = Some(cursor);
            self.save_state();
            pages += 1;
            if rep.waiting.is_some() || rep.fatal.is_some() || !full || pages >= MAX_PAGES {
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
            "work folder  {} (one folder per room inside)\n",
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
            self.cfg
                .settings
                .budget_usd
                .map_or("no cost cap".to_owned(), |b| format!(
                    "{b:.2} USD (an estimate)"
                )),
            self.cfg.per_hour,
            self.cfg.per_day,
            self.cfg.session_max_wakes
        ));
        match self.plan(
            &prompt,
            Some("SESSION-ID-OF-THIS-ROOM"),
            &self.room_dir(fake_room),
        ) {
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
        o.push_str("\nno command tools (--restricted, only file tools, Bash and web denied), one MCP server (wave), nothing is started by --dry-run\n");
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
pub fn prepare(st: &Station<'_>) -> Result<Vec<String>, String> {
    fs::create_dir_all(&st.dirs.state).map_err(|e| {
        format!(
            "cannot create {}: {}",
            super::dirs::text(&st.dirs.state),
            e.kind()
        )
    })?;
    let key_file = match &st.cfg.key {
        KeySource::File(p) => Some(p.as_path()),
        KeySource::Env(_) => None,
    };
    let warnings = check_workdir(&st.cfg.workdir, &st.dirs, key_file)?;
    st.write_mcp_file()
        .map_err(|e| format!("cannot write the MCP file: {}", e.kind()))?;
    Ok(warnings)
}

#[cfg(test)]
mod tests;
