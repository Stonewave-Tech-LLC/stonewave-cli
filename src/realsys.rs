//! The real machine behind [`crate::setup::Sys`]. Linux only in effect: files are written with
//! exact modes, commands run with a time limit, and nothing is ever run through a shell.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use crate::setup::{Run, Stat, Sys};

/// `systemctl enable --now` and `useradd` can take a moment on a busy server.
const COMMAND_LIMIT: Duration = Duration::from_secs(60);

pub struct RealSys;

impl Sys for RealSys {
    fn read(&self, path: &str) -> Option<String> {
        std::fs::read_to_string(path).ok()
    }

    fn exists(&self, path: &str) -> bool {
        std::path::Path::new(path).exists()
    }

    fn stat(&self, path: &str) -> Option<Stat> {
        let r = self.run("stat", &["-c", "%a:%U:%G", path]);
        if !r.ok {
            return None;
        }
        let mut it = r.out.trim().split(':');
        Some(Stat {
            mode: u32::from_str_radix(it.next()?, 8).ok()?,
            user: it.next()?.to_owned(),
            group: it.next()?.to_owned(),
        })
    }

    fn write(&self, path: &str, content: &str, mode: u32) -> Result<(), String> {
        let target = std::path::Path::new(path);
        let dir = target.parent().ok_or("no folder")?;
        let name = target.file_name().ok_or("no file name")?.to_string_lossy();
        // Same folder, so the rename is atomic; the rights are set when the file is created, so the
        // content is never readable with looser rights than asked for.
        let tmp = dir.join(format!(".{name}.tmp{}", std::process::id()));
        let _ = std::fs::remove_file(&tmp);
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            opts.mode(mode);
        }
        #[cfg(not(unix))]
        let _ = mode;
        let mut f = opts
            .open(&tmp)
            .map_err(|e| format!("{}: {e}", tmp.display()))?;
        let result = f
            .write_all(content.as_bytes())
            .and_then(|()| f.sync_all())
            .map_err(|e| format!("{}: {e}", tmp.display()));
        drop(f);
        if let Err(e) = result {
            let _ = std::fs::remove_file(&tmp);
            return Err(e);
        }
        std::fs::rename(&tmp, target).map_err(|e| {
            let _ = std::fs::remove_file(&tmp);
            format!("{path}: {e}")
        })
    }

    fn mkdir(&self, path: &str, mode: u32) -> Result<(), String> {
        std::fs::create_dir_all(path).map_err(|e| format!("{path}: {e}"))?;
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            std::fs::set_permissions(path, std::fs::Permissions::from_mode(mode))
                .map_err(|e| format!("{path}: {e}"))?;
        }
        #[cfg(not(unix))]
        let _ = mode;
        Ok(())
    }

    fn remove(&self, path: &str) {
        let _ = std::fs::remove_file(path);
    }

    fn run(&self, cmd: &str, args: &[&str]) -> Run {
        let spawned = Command::new(cmd)
            .args(args)
            .env("LC_ALL", "C")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn();
        let mut child = match spawned {
            Ok(c) => c,
            Err(e) => {
                return Run {
                    ok: false,
                    out: format!("{cmd}: {e}"),
                };
            }
        };
        let mut stdout = child.stdout.take();
        let mut stderr = child.stderr.take();
        let (tx, rx) = mpsc::channel();
        // Collect output on a side thread: a full pipe must not stall the command, and a hanging
        // command must not stall setup.
        let reader = std::thread::spawn(move || {
            let mut out = String::new();
            if let Some(s) = stdout.as_mut() {
                let _ = s.read_to_string(&mut out);
            }
            let mut err = String::new();
            if let Some(s) = stderr.as_mut() {
                let _ = s.read_to_string(&mut err);
            }
            let _ = tx.send((out, err));
        });
        let started = std::time::Instant::now();
        let status = loop {
            match child.try_wait() {
                Ok(Some(s)) => break Some(s),
                Ok(None) if started.elapsed() < COMMAND_LIMIT => {
                    std::thread::sleep(Duration::from_millis(10));
                }
                _ => {
                    let _ = child.kill();
                    let _ = child.wait();
                    break None;
                }
            }
        };
        let (out, err) = rx.recv_timeout(Duration::from_secs(2)).unwrap_or_default();
        let _ = reader.join();
        match status {
            Some(s) => Run {
                ok: s.success(),
                out: if out.trim().is_empty() { err } else { out },
            },
            None => Run {
                ok: false,
                out: format!("{cmd}: timed out"),
            },
        }
    }

    fn listening(&self, port: u16) -> bool {
        let addr: SocketAddr = ([127, 0, 0, 1], port).into();
        TcpStream::connect_timeout(&addr, Duration::from_millis(800)).is_ok()
    }

    fn euid(&self) -> u32 {
        std::fs::read_to_string("/proc/self/status")
            .ok()
            .as_deref()
            .and_then(crate::doctor::parse_euid)
            .unwrap_or(u32::MAX)
    }
}
