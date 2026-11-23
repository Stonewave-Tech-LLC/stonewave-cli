//! Starting one run of a coding agent: the program, its arguments, a cleaned environment, a folder and a time limit.
//!
//! The station never goes through a shell and never hands a run the whole environment of the user: [`clean_env`] lets through only what
//! the program needs to find its own login and its network, plus the one variable the station adds (the agent's key).
//! A run that takes longer than its time limit is ended: the whole group first politely, then for good.

use std::io::Read;
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::thread;
use std::time::{Duration, Instant};

/// At most this much of a run's output is kept (the rest is read and thrown away so the program never blocks on a full pipe).
pub const OUT_MAX: usize = 4 * 1024 * 1024;
/// How long a run gets to end after it was asked to, before it is ended for good.
pub const GRACE: Duration = Duration::from_secs(30);

#[derive(Debug, Clone)]
pub struct RunReq {
    pub program: PathBuf,
    pub args: Vec<String>,
    pub env: Vec<(String, String)>,
    pub cwd: PathBuf,
    pub timeout: Duration,
    pub grace: Duration,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Ended {
    Exited(Option<i32>),
    TimedOut,
    CouldNotStart(String),
}

#[derive(Debug, Clone)]
pub struct RunOut {
    pub ended: Ended,
    pub stdout: String,
    pub stderr: String,
    pub elapsed: Duration,
}

/// Starting a program. Tests replace it with a recorder.
pub trait Spawn {
    fn run(&self, req: &RunReq) -> RunOut;
}

pub struct RealSpawn;

/// Names (case-insensitive) of the variables a run may inherit: where its home is, how it reaches the network and the logins of the engines.
/// Everything else of the user's environment (other services' keys, cloud credentials, tokens) stays out.
const PASS: &[&str] = &[
    "PATH",
    "HOME",
    "USER",
    "LOGNAME",
    "LANG",
    "LC_ALL",
    "LC_CTYPE",
    "TERM",
    "TZ",
    "TMPDIR",
    "TEMP",
    "TMP",
    "USERPROFILE",
    "USERNAME",
    "LOCALAPPDATA",
    "APPDATA",
    "SYSTEMROOT",
    "SYSTEMDRIVE",
    "COMSPEC",
    "PATHEXT",
    "WINDIR",
    "XDG_CONFIG_HOME",
    "XDG_STATE_HOME",
    "XDG_DATA_HOME",
    "XDG_CACHE_HOME",
    "XDG_RUNTIME_DIR",
    "HTTPS_PROXY",
    "HTTP_PROXY",
    "NO_PROXY",
    "ALL_PROXY",
    "SSL_CERT_FILE",
    "SSL_CERT_DIR",
    "NODE_EXTRA_CA_CERTS",
    // the login of Claude Code (its folder, a token for unattended jobs, an API key)
    "CLAUDE_CONFIG_DIR",
    "CLAUDE_CODE_OAUTH_TOKEN",
    "ANTHROPIC_API_KEY",
    "ANTHROPIC_AUTH_TOKEN",
    "ANTHROPIC_BASE_URL",
];

/// The environment of a run: the allowed variables of `get_all`, then `extra` (the key).
pub fn clean_env(
    all: impl IntoIterator<Item = (String, String)>,
    extra: &[(String, String)],
) -> Vec<(String, String)> {
    let mut out: Vec<(String, String)> = all
        .into_iter()
        .filter(|(k, _)| PASS.iter().any(|p| p.eq_ignore_ascii_case(k)))
        .collect();
    for (k, v) in extra {
        out.retain(|(x, _)| !x.eq_ignore_ascii_case(k));
        out.push((k.clone(), v.clone()));
    }
    out
}

fn drain(mut r: impl Read + Send + 'static) -> thread::JoinHandle<Vec<u8>> {
    thread::spawn(move || {
        let mut kept = Vec::new();
        let mut buf = [0u8; 16 * 1024];
        loop {
            match r.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let room = OUT_MAX.saturating_sub(kept.len());
                    kept.extend_from_slice(&buf[..n.min(room)]);
                }
            }
        }
        kept
    })
}

#[cfg(unix)]
fn signal_group(pid: u32, sig: i32) {
    // SAFETY: kill has no memory effects; a negative pid addresses the process group the child leads (it was started as its leader).
    unsafe {
        libc::kill(-(pid as i32), sig);
    }
}

#[cfg(unix)]
fn ask_to_end(child: &mut std::process::Child) {
    signal_group(child.id(), libc::SIGTERM);
}

#[cfg(unix)]
fn end_for_good(child: &mut std::process::Child) {
    signal_group(child.id(), libc::SIGKILL);
    let _ = child.kill();
}

#[cfg(not(unix))]
fn ask_to_end(child: &mut std::process::Child) {
    // the child's own tree, by its process id; nothing is ended by name
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
}

#[cfg(not(unix))]
fn end_for_good(child: &mut std::process::Child) {
    let _ = Command::new("taskkill")
        .args(["/PID", &child.id().to_string(), "/T", "/F"])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status();
    let _ = child.kill();
}

impl Spawn for RealSpawn {
    fn run(&self, req: &RunReq) -> RunOut {
        let started = Instant::now();
        let mut cmd = Command::new(&req.program);
        cmd.args(&req.args)
            .env_clear()
            .envs(req.env.iter().map(|(k, v)| (k, v)))
            .current_dir(&req.cwd)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        #[cfg(unix)]
        {
            use std::os::unix::process::CommandExt;
            cmd.process_group(0);
        }
        let mut child = match cmd.spawn() {
            Ok(c) => c,
            Err(e) => {
                return RunOut {
                    ended: Ended::CouldNotStart(e.kind().to_string()),
                    stdout: String::new(),
                    stderr: String::new(),
                    elapsed: started.elapsed(),
                };
            }
        };
        let out = child.stdout.take().map(drain);
        let err = child.stderr.take().map(drain);
        let mut asked: Option<Instant> = None;
        let mut timed_out = false;
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) => {}
                Err(_) => break None,
            }
            let now = Instant::now();
            if !timed_out && now.duration_since(started) >= req.timeout {
                timed_out = true;
                asked = Some(now);
                ask_to_end(&mut child);
            } else if let Some(t) = asked {
                if now.duration_since(t) >= req.grace {
                    end_for_good(&mut child);
                    break child.wait().ok();
                }
            }
            thread::sleep(Duration::from_millis(50));
        };
        let take = |h: Option<thread::JoinHandle<Vec<u8>>>| -> String {
            h.and_then(|h| h.join().ok())
                .map(|b| String::from_utf8_lossy(&b).into_owned())
                .unwrap_or_default()
        };
        let stdout = take(out);
        let stderr = take(err);
        RunOut {
            ended: if timed_out {
                Ended::TimedOut
            } else {
                Ended::Exited(status.and_then(|s| s.code()))
            },
            stdout,
            stderr,
            elapsed: started.elapsed(),
        }
    }
}

/// The program as the station will start it: an explicit path, or `claude` found on the PATH.
pub fn find_program(
    explicit: Option<&Path>,
    name: &str,
    which: &dyn Fn(&str) -> Option<PathBuf>,
) -> Option<PathBuf> {
    explicit.map(Path::to_path_buf).or_else(|| which(name))
}

#[cfg(test)]
pub mod fake {
    use super::*;
    use std::cell::RefCell;
    use std::collections::VecDeque;

    /// Answers from a list, in order, and remembers every request.
    pub struct Recorder {
        pub reqs: RefCell<Vec<RunReq>>,
        answers: RefCell<VecDeque<RunOut>>,
    }

    pub fn out(code: i32, stdout: &str) -> RunOut {
        RunOut {
            ended: Ended::Exited(Some(code)),
            stdout: stdout.into(),
            stderr: String::new(),
            elapsed: Duration::from_millis(5),
        }
    }

    pub fn timed_out() -> RunOut {
        RunOut {
            ended: Ended::TimedOut,
            stdout: String::new(),
            stderr: String::new(),
            elapsed: Duration::from_secs(1200),
        }
    }

    impl Recorder {
        pub fn new(answers: Vec<RunOut>) -> Recorder {
            Recorder {
                reqs: RefCell::new(Vec::new()),
                answers: RefCell::new(answers.into()),
            }
        }
    }

    impl Spawn for Recorder {
        fn run(&self, req: &RunReq) -> RunOut {
            self.reqs.borrow_mut().push(req.clone());
            self.answers
                .borrow_mut()
                .pop_front()
                .expect("unexpected run")
        }
    }
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use std::fs;

    fn script(dir: &Path, body: &str) -> PathBuf {
        use std::os::unix::fs::PermissionsExt;
        let p = dir.join("fake.sh");
        fs::write(&p, format!("#!/bin/sh\n{body}\n")).unwrap();
        fs::set_permissions(&p, fs::Permissions::from_mode(0o755)).unwrap();
        p
    }

    fn tmp(label: &str) -> PathBuf {
        let d =
            std::env::temp_dir().join(format!("stonewave-spawn-{label}-{}", std::process::id()));
        let _ = fs::remove_dir_all(&d);
        fs::create_dir_all(&d).unwrap();
        d
    }

    fn req(p: PathBuf, cwd: &Path, timeout: Duration) -> RunReq {
        RunReq {
            program: p,
            args: vec![],
            env: vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("WAVE_AGENT_KEY".into(), "wak_x".into()),
            ],
            cwd: cwd.into(),
            timeout,
            grace: Duration::from_millis(300),
        }
    }

    #[test]
    fn the_environment_is_cleaned_and_the_key_is_added() {
        let dir = tmp("env");
        let p = script(
            &dir,
            "echo \"key=$WAVE_AGENT_KEY other=$SECRET_SERVICE_TOKEN aws=$AWS_SECRET_ACCESS_KEY\"; pwd",
        );
        let mut r = req(p, &dir, Duration::from_secs(10));
        r.env = clean_env(
            vec![
                ("PATH".into(), "/usr/bin:/bin".into()),
                ("SECRET_SERVICE_TOKEN".into(), "leak1".into()),
                ("AWS_SECRET_ACCESS_KEY".into(), "leak2".into()),
            ],
            &[("WAVE_AGENT_KEY".into(), "wak_x".into())],
        );
        let o = RealSpawn.run(&r);
        assert_eq!(o.ended, Ended::Exited(Some(0)));
        assert!(o.stdout.contains("key=wak_x other= aws="), "{}", o.stdout);
        assert!(!o.stdout.contains("leak"));
        assert!(
            o.stdout
                .contains(dir.file_name().unwrap().to_str().unwrap()),
            "runs in the folder it was given"
        );
    }

    #[test]
    fn a_run_that_is_too_slow_is_ended_with_its_children() {
        let dir = tmp("slow");
        let p = script(&dir, "sleep 30 & sleep 30; echo done");
        let t = Instant::now();
        let o = RealSpawn.run(&req(p, &dir, Duration::from_millis(300)));
        assert_eq!(o.ended, Ended::TimedOut);
        assert!(
            t.elapsed() < Duration::from_secs(5),
            "ended in time: {:?}",
            t.elapsed()
        );
        assert!(!o.stdout.contains("done"));
    }

    #[test]
    fn a_run_that_ignores_the_polite_request_is_ended_for_good() {
        let dir = tmp("stubborn");
        let p = script(&dir, "trap '' TERM; while true; do sleep 1; done");
        let t = Instant::now();
        let o = RealSpawn.run(&req(p, &dir, Duration::from_millis(200)));
        assert_eq!(o.ended, Ended::TimedOut);
        assert!(t.elapsed() < Duration::from_secs(6));
    }

    #[test]
    fn output_is_capped_and_a_missing_program_is_said_plainly() {
        let dir = tmp("big");
        let p = script(&dir, "head -c 6000000 /dev/zero | tr '\\0' 'x'");
        let o = RealSpawn.run(&req(p, &dir, Duration::from_secs(20)));
        assert_eq!(o.ended, Ended::Exited(Some(0)));
        assert_eq!(o.stdout.len(), OUT_MAX);
        let o = RealSpawn.run(&req(dir.join("nope"), &dir, Duration::from_secs(5)));
        assert!(matches!(o.ended, Ended::CouldNotStart(_)));
    }
}

#[cfg(test)]
mod env_tests {
    use super::*;

    #[test]
    fn only_allowed_names_pass_whatever_their_case() {
        let env = clean_env(
            vec![
                ("Path".into(), "p".into()),
                ("AWS_ACCESS_KEY_ID".into(), "a".into()),
                ("GITHUB_TOKEN".into(), "g".into()),
                ("INFISICAL_TOKEN".into(), "i".into()),
                ("MY_AGENT_KEY".into(), "k".into()),
                ("ANTHROPIC_API_KEY".into(), "c".into()),
            ],
            &[("WAVE_AGENT_KEY".into(), "wak_x".into())],
        );
        let names: Vec<&str> = env.iter().map(|(k, _)| k.as_str()).collect();
        assert_eq!(names, vec!["Path", "ANTHROPIC_API_KEY", "WAVE_AGENT_KEY"]);
    }

    #[test]
    fn the_added_key_replaces_one_of_the_same_name() {
        let env = clean_env(
            vec![("wave_agent_key".into(), "old".into())],
            &[("WAVE_AGENT_KEY".into(), "new".into())],
        );
        assert_eq!(env, vec![("WAVE_AGENT_KEY".to_owned(), "new".to_owned())]);
    }
}
