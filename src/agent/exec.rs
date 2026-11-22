//! Looking for programs and running them. Everything the agent commands run on this machine goes through [`Exec`],
//! so tests can record the exact arguments instead of starting `claude`, `systemctl` or `schtasks`.
//!
//! Programs are started directly (never through a shell) with their arguments as separate elements.

use std::io::{self, Read};
use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

#[derive(Debug, Clone, Default)]
pub struct Output {
    pub ok: bool,
    pub code: Option<i32>,
    pub stdout: String,
    pub stderr: String,
}

impl Output {
    /// The first useful line of what went wrong, for a message.
    pub fn problem(&self) -> String {
        let text = if self.stderr.trim().is_empty() {
            &self.stdout
        } else {
            &self.stderr
        };
        let line = text.lines().find(|l| !l.trim().is_empty()).unwrap_or("");
        if line.is_empty() {
            format!(
                "exit code {}",
                self.code.map_or("?".into(), |c| c.to_string())
            )
        } else {
            crate::connect::tame(line.trim(), 160)
        }
    }
}

/// What is known about a running process: when it started (FILETIME ticks) and its program name without `.exe`.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ProcInfo {
    pub start: i64,
    pub image: String,
}

pub trait Exec {
    /// Where `name` lives on the PATH (with the Windows extensions), if anywhere.
    fn which(&self, name: &str) -> Option<PathBuf>;
    /// Runs a program to its end and collects what it wrote. `Err` only when it could not be started.
    fn run(&self, program: &str, args: &[String]) -> io::Result<Output>;
    /// The process with this id, or None when there is none. Windows only; elsewhere the service managers do the stopping.
    fn process_info(&self, pid: u32) -> io::Result<Option<ProcInfo>>;
    /// Ends the process with this id (and nothing else). The only way this tool ends a process.
    fn end_process(&self, pid: u32) -> io::Result<()>;
}

/// The real machine.
pub struct System;

/// How long a helper program may take before it is stopped.
const RUN_TIMEOUT: Duration = Duration::from_secs(60);

impl Exec for System {
    fn which(&self, name: &str) -> Option<PathBuf> {
        let path = std::env::var_os("PATH")?;
        let exts: Vec<String> = if cfg!(windows) {
            std::env::var("PATHEXT")
                .unwrap_or_else(|_| ".COM;.EXE;.BAT;.CMD".into())
                .split(';')
                .filter(|e| !e.is_empty())
                .map(str::to_owned)
                .collect()
        } else {
            Vec::new()
        };
        for dir in std::env::split_paths(&path) {
            if dir.as_os_str().is_empty() {
                continue;
            }
            // On Windows a file without an extension (the shell shim npm puts next to the .cmd file) cannot be started by CreateProcess: only names that
            // already carry a PATHEXT extension are taken as they are.
            let lower = name.to_ascii_lowercase();
            let has_ext = exts
                .iter()
                .any(|e| lower.ends_with(&e.to_ascii_lowercase()));
            let plain = dir.join(name);
            if (exts.is_empty() || has_ext) && is_program(&plain) {
                return Some(plain);
            }
            for e in &exts {
                let with = dir.join(format!("{name}{e}"));
                if is_program(&with) {
                    return Some(with);
                }
            }
        }
        None
    }

    fn process_info(&self, pid: u32) -> io::Result<Option<ProcInfo>> {
        let script = format!(
            "$p = Get-Process -Id {pid} -ErrorAction SilentlyContinue; if ($p) {{ $p.StartTime.ToFileTimeUtc().ToString() + ' ' + $p.ProcessName }}"
        );
        let out = self.run(
            "powershell",
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                script,
            ],
        )?;
        if !out.ok {
            return Err(io::Error::other(out.problem()));
        }
        let line = out.stdout.trim();
        if line.is_empty() {
            return Ok(None);
        }
        let (start, image) = line
            .split_once(' ')
            .ok_or_else(|| io::Error::other("unexpected answer about a process"))?;
        let start = start
            .parse()
            .map_err(|_| io::Error::other("unexpected answer about a process"))?;
        Ok(Some(ProcInfo {
            start,
            image: image.trim().to_owned(),
        }))
    }

    fn end_process(&self, pid: u32) -> io::Result<()> {
        let out = self.run(
            "powershell",
            &[
                "-NoProfile".into(),
                "-NonInteractive".into(),
                "-Command".into(),
                format!("Stop-Process -Id {pid} -ErrorAction Stop"),
            ],
        )?;
        if out.ok {
            Ok(())
        } else {
            Err(io::Error::other(out.problem()))
        }
    }

    fn run(&self, program: &str, args: &[String]) -> io::Result<Output> {
        let mut child = Command::new(program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()?;
        let drain = |mut r: Box<dyn Read + Send>| {
            std::thread::spawn(move || {
                let mut b = Vec::new();
                let _ = r.read_to_end(&mut b);
                String::from_utf8_lossy(&b).into_owned()
            })
        };
        let out = drain(Box::new(child.stdout.take().expect("piped")));
        let err = drain(Box::new(child.stderr.take().expect("piped")));
        let started = Instant::now();
        let status = loop {
            if let Some(s) = child.try_wait()? {
                break s;
            }
            if started.elapsed() > RUN_TIMEOUT {
                // our own child, by handle
                let _ = child.kill();
                let _ = child.wait();
                return Err(io::Error::new(io::ErrorKind::TimedOut, "took too long"));
            }
            std::thread::sleep(Duration::from_millis(20));
        };
        Ok(Output {
            ok: status.success(),
            code: status.code(),
            stdout: out.join().unwrap_or_default(),
            stderr: err.join().unwrap_or_default(),
        })
    }
}

fn is_program(p: &Path) -> bool {
    let Ok(m) = std::fs::metadata(p) else {
        return false;
    };
    if !m.is_file() {
        return false;
    }
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        m.permissions().mode() & 0o111 != 0
    }
    #[cfg(not(unix))]
    {
        true
    }
}

#[cfg(test)]
pub mod fake {
    //! A recording stand-in for the machine.
    use super::*;
    use std::cell::RefCell;

    type Script = Box<dyn Fn(&str, &[String]) -> Option<Output>>;

    pub struct Fake {
        pub present: Vec<(String, PathBuf)>,
        pub calls: RefCell<Vec<(String, Vec<String>)>>,
        /// The running processes `process_info` knows.
        pub procs: RefCell<Vec<(u32, ProcInfo)>>,
        /// The ids `end_process` was asked to end, in order.
        pub ended: RefCell<Vec<u32>>,
        /// Ids that cannot be ended.
        pub fail_end: RefCell<Vec<u32>>,
        script: Script,
    }

    pub fn ok_out(stdout: &str) -> Output {
        Output {
            ok: true,
            code: Some(0),
            stdout: stdout.into(),
            stderr: String::new(),
        }
    }

    pub fn failed(stderr: &str) -> Output {
        Output {
            ok: false,
            code: Some(1),
            stdout: String::new(),
            stderr: stderr.into(),
        }
    }

    impl Fake {
        /// Everything succeeds with empty output unless the script says otherwise.
        pub fn new(present: &[&str]) -> Fake {
            Fake {
                present: present
                    .iter()
                    .map(|n| ((*n).to_owned(), PathBuf::from(format!("/fake/bin/{n}"))))
                    .collect(),
                calls: RefCell::new(Vec::new()),
                procs: RefCell::new(Vec::new()),
                ended: RefCell::new(Vec::new()),
                fail_end: RefCell::new(Vec::new()),
                script: Box::new(|_, _| None),
            }
        }

        pub fn with(mut self, f: impl Fn(&str, &[String]) -> Option<Output> + 'static) -> Fake {
            self.script = Box::new(f);
            self
        }

        /// Every call as one line: `program arg arg`.
        pub fn lines(&self) -> Vec<String> {
            self.calls
                .borrow()
                .iter()
                .map(|(p, a)| format!("{p} {}", a.join(" ")).trim().to_owned())
                .collect()
        }
    }

    impl Exec for Fake {
        fn which(&self, name: &str) -> Option<PathBuf> {
            self.present
                .iter()
                .find(|(n, _)| n == name)
                .map(|(_, p)| p.clone())
        }

        fn process_info(&self, pid: u32) -> io::Result<Option<ProcInfo>> {
            Ok(self
                .procs
                .borrow()
                .iter()
                .find(|(p, _)| *p == pid)
                .map(|(_, i)| i.clone()))
        }

        fn end_process(&self, pid: u32) -> io::Result<()> {
            if self.fail_end.borrow().contains(&pid) {
                return Err(io::Error::other("access denied"));
            }
            self.ended.borrow_mut().push(pid);
            self.procs.borrow_mut().retain(|(p, _)| *p != pid);
            Ok(())
        }

        fn run(&self, program: &str, args: &[String]) -> io::Result<Output> {
            self.calls
                .borrow_mut()
                .push((program.to_owned(), args.to_vec()));
            Ok((self.script)(program, args).unwrap_or_else(|| ok_out("")))
        }
    }
}
