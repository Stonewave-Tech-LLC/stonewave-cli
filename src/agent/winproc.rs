//! The Windows wake service and how it is stopped.
//!
//! The scheduled task runs a small PowerShell wrapper (written by this tool). The wrapper records, in a file next to it that holds no
//! secret, its own process id and start time and, each time it starts node, node's process id and start time. Stopping the service
//! ends exactly those processes by id, child first, and only after checking that an id still belongs to the same process:
//! same start time and the expected program name. A process that fails that check is left alone and said so.
//! Nothing here ends a process by its name or its command line.

use std::fs;
use std::path::Path;

use serde_json::Value;

use super::exec::{Exec, ProcInfo};
use super::key::scrub;
use super::state::ServiceRec;
use super::station::Unit;

/// The program names `Get-Process` shows for the two processes of the service.
pub const WRAPPER_IMAGE: &str = "powershell";
pub const CHILD_IMAGE: &str = "node";

// ------------------------------------------------------------------ the wrapper

/// A value in a PowerShell single-quoted string.
fn ps(s: &str) -> String {
    format!("'{}'", s.replace('\'', "''"))
}

/// The wrapper: sets the two variables, keeps the record, starts node with its output appended to the wake log, and starts it
/// again 20 seconds after it ended. The key is not in it; node reads it from the key file.
pub fn wrapper_script(u: &Unit<'_>, pid_file: &str) -> String {
    let lines = [
        format!(
            "# Wake service of the Wave agent {}, made by stonewave. Runs at logon; output goes to the wake log.",
            u.agent.replace(['\r', '\n'], " ")
        ),
        "$ErrorActionPreference = 'Stop'".to_owned(),
        "$ProgressPreference = 'SilentlyContinue'".to_owned(),
        format!("$env:WAVE_MCP_URL = {}", ps(u.mcp_url)),
        format!("$env:WAVE_AGENT_KEY_FILE = {}", ps(u.key_file)),
        format!("$node = {}", ps(u.node)),
        format!("$script = {}", ps(u.script)),
        format!("$agent = {}", ps(u.agent)),
        format!("$log = {}", ps(u.log)),
        format!("$pidFile = {}", ps(pid_file)),
        String::new(),
        "# who is running: this wrapper, and the node it started (ids and start times, no secrets)".to_owned(),
        "function Save-State($child) {".to_owned(),
        "  $me = Get-Process -Id $PID".to_owned(),
        "  $o = [ordered]@{ version = 1; wrapper = [ordered]@{ pid = $PID; start = $me.StartTime.ToFileTimeUtc(); image = $me.ProcessName }; child = $child }".to_owned(),
        "  $tmp = $pidFile + '.tmp'".to_owned(),
        "  [IO.File]::WriteAllText($tmp, ($o | ConvertTo-Json -Compress -Depth 4))".to_owned(),
        "  Move-Item -Force -LiteralPath $tmp -Destination $pidFile".to_owned(),
        "}".to_owned(),
        String::new(),
        "Save-State $null".to_owned(),
        "while ($true) {".to_owned(),
        "  $out = $null; $err = $null".to_owned(),
        "  try {".to_owned(),
        "    $psi = New-Object System.Diagnostics.ProcessStartInfo".to_owned(),
        "    $psi.FileName = $node".to_owned(),
        "    $psi.Arguments = '\"' + $script + '\" --session \"' + $agent + '\"'".to_owned(),
        "    $psi.UseShellExecute = $false".to_owned(),
        "    $psi.CreateNoWindow = $true".to_owned(),
        "    $psi.RedirectStandardOutput = $true".to_owned(),
        "    $psi.RedirectStandardError = $true".to_owned(),
        "    $p = [System.Diagnostics.Process]::Start($psi)".to_owned(),
        "    Save-State ([ordered]@{ pid = $p.Id; start = $p.StartTime.ToFileTimeUtc(); image = $p.ProcessName })".to_owned(),
        "    # unbuffered, appending, readable by others (the sessions that follow the log)".to_owned(),
        "    $out = New-Object IO.FileStream($log, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite, 1)".to_owned(),
        "    $err = New-Object IO.FileStream($log, [IO.FileMode]::Append, [IO.FileAccess]::Write, [IO.FileShare]::ReadWrite, 1)".to_owned(),
        "    $t1 = $p.StandardOutput.BaseStream.CopyToAsync($out)".to_owned(),
        "    $t2 = $p.StandardError.BaseStream.CopyToAsync($err)".to_owned(),
        "    $p.WaitForExit()".to_owned(),
        "    [void][System.Threading.Tasks.Task]::WaitAll(@($t1, $t2), 5000)".to_owned(),
        "  } catch {".to_owned(),
        "    try { [IO.File]::AppendAllText($log, 'fleet: wake service problem: ' + $_.Exception.Message + [Environment]::NewLine) } catch { }".to_owned(),
        "  } finally {".to_owned(),
        "    if ($out) { $out.Dispose() }".to_owned(),
        "    if ($err) { $err.Dispose() }".to_owned(),
        "    Save-State $null".to_owned(),
        "  }".to_owned(),
        "  Start-Sleep -Seconds 20".to_owned(),
        "}".to_owned(),
    ];
    let mut s = lines.join("\r\n");
    s.push_str("\r\n");
    s
}

/// The task starts the wrapper with a hidden window and without any execution policy getting in the way.
pub fn task_arguments(wrapper: &str) -> String {
    format!(
        "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File \"{wrapper}\""
    )
}

/// A script file for Windows PowerShell 5: UTF-8 with a byte order mark, so that paths with accents are read right.
pub fn script_bytes(text: &str) -> Vec<u8> {
    let mut b = vec![0xEF, 0xBB, 0xBF];
    b.extend_from_slice(text.as_bytes());
    b
}

// ------------------------------------------------------------------ the record

/// Where the wrapper keeps its record, next to the wrapper itself.
pub fn pid_file_of(wrapper: &str) -> String {
    match wrapper.strip_suffix(".ps1") {
        Some(base) => format!("{base}.pids.json"),
        None => format!("{wrapper}.pids.json"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Recorded {
    pub pid: u32,
    /// FILETIME ticks (100 ns since 1601), as `Process.StartTime.ToFileTimeUtc()` gives them.
    pub start: i64,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PidFile {
    pub wrapper: Recorded,
    pub child: Option<Recorded>,
}

fn recorded(v: &Value) -> Option<Recorded> {
    Some(Recorded {
        pid: u32::try_from(v.get("pid")?.as_u64()?)
            .ok()
            .filter(|p| *p > 4)?,
        start: v.get("start")?.as_i64().filter(|s| *s > 0)?,
    })
}

pub fn parse_pid_file(text: &str) -> Option<PidFile> {
    let v: Value = serde_json::from_str(text.trim_start_matches('\u{feff}')).ok()?;
    if v.get("version")?.as_u64()? != 1 {
        return None;
    }
    Some(PidFile {
        wrapper: recorded(v.get("wrapper")?)?,
        child: match v.get("child") {
            None | Some(Value::Null) => None,
            Some(c) => Some(recorded(c)?),
        },
    })
}

#[derive(Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No such process any more: nothing to do.
    Gone,
    /// The very process that was recorded.
    Ours,
    /// The number belongs to something else now (or cannot be told): hands off.
    Stale(String),
}

/// Is the process that runs under the recorded id the one that was recorded?
pub fn judge(rec: &Recorded, expected_image: &str, live: Option<&ProcInfo>) -> Verdict {
    let Some(live) = live else {
        return Verdict::Gone;
    };
    if live.start != rec.start {
        return Verdict::Stale("it started at another time".to_owned());
    }
    if !live.image.eq_ignore_ascii_case(expected_image) {
        return Verdict::Stale(format!(
            "it is {}, not {expected_image}",
            scrub(&live.image)
        ));
    }
    Verdict::Ours
}

// ------------------------------------------------------------------ stopping

#[derive(Debug, Default, PartialEq, Eq)]
pub struct StopReport {
    /// The process ids that were ended.
    pub ended: Vec<u32>,
    /// Things worth telling (a stale id that was left alone).
    pub notes: Vec<String>,
    pub problems: Vec<String>,
}

/// Ends what the wrapper recorded: node first, then the wrapper (which waits 20 seconds after a node ends before it starts the next
/// one, so it cannot start another in between). Whatever fails the check is left alone.
pub fn stop(rec: &ServiceRec, exec: &dyn Exec) -> StopReport {
    let mut report = StopReport::default();
    let Some(wrapper) = rec.files.first() else {
        return report;
    };
    let pid_file = pid_file_of(wrapper);
    let text = match fs::read_to_string(&pid_file) {
        Ok(t) => t,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return report,
        Err(e) => {
            report.problems.push(format!(
                "cannot read {pid_file}: {}; nothing was ended",
                scrub(&e.to_string())
            ));
            return report;
        }
    };
    let Some(file) = parse_pid_file(&text) else {
        report.notes.push(format!(
            "{pid_file} is not a record of this tool; no process was ended"
        ));
        return report;
    };
    let targets = [
        file.child.map(|c| (c, CHILD_IMAGE, "node")),
        Some((file.wrapper, WRAPPER_IMAGE, "wrapper")),
    ];
    let mut clean = true;
    for (r, image, what) in targets.into_iter().flatten() {
        match exec.process_info(r.pid) {
            Err(e) => {
                clean = false;
                report.problems.push(format!(
                    "cannot look at process {} ({what}): {}",
                    r.pid,
                    scrub(&e.to_string())
                ));
            }
            Ok(live) => match judge(&r, image, live.as_ref()) {
                Verdict::Gone => {}
                Verdict::Stale(why) => report
                    .notes
                    .push(format!("stale pid {} ({what}), left alone: {why}", r.pid)),
                Verdict::Ours => match exec.end_process(r.pid) {
                    Ok(()) => report.ended.push(r.pid),
                    Err(e) => {
                        clean = false;
                        report.problems.push(format!(
                            "could not end process {} ({what}): {}",
                            r.pid,
                            scrub(&e.to_string())
                        ));
                    }
                },
            },
        }
    }
    if clean {
        let _ = fs::remove_file(Path::new(&pid_file));
    }
    report
}

#[cfg(test)]
mod tests {
    use super::super::exec::fake::Fake;
    use super::super::key::testdir::TempDir;
    use super::*;

    fn unit<'a>() -> Unit<'a> {
        Unit {
            agent: "agent-demo",
            node: "C:\\Program Files\\nodejs\\node.exe",
            script: "C:\\Users\\it's me\\wave\\fleet-watch.mjs",
            mcp_url: "https://wave.test/mcp",
            key_file: "C:\\Users\\it's me\\wave\\agent-demo.key",
            log: "C:\\Users\\it's me\\wave\\agent-demo.wake.log",
        }
    }

    #[test]
    fn the_wrapper_text() {
        let s = wrapper_script(&unit(), "C:\\Users\\it's me\\wave\\x.pids.json");
        assert!(s.contains("$env:WAVE_MCP_URL = 'https://wave.test/mcp'\r\n"));
        assert!(s.contains(
            "$env:WAVE_AGENT_KEY_FILE = 'C:\\Users\\it''s me\\wave\\agent-demo.key'\r\n"
        ));
        assert!(s.contains("$node = 'C:\\Program Files\\nodejs\\node.exe'\r\n"));
        assert!(s.contains("$pidFile = 'C:\\Users\\it''s me\\wave\\x.pids.json'\r\n"));
        assert!(s.contains("Start-Sleep -Seconds 20"), "the restart loop");
        assert!(s.contains("RedirectStandardOutput") && s.contains("[IO.FileMode]::Append"));
        assert!(s.contains("--session"));
        assert!(!s.replace("\r\n", "").contains('\n'), "only CRLF line ends");
        assert!(!s.contains("wak_"));
        // it kills nothing and names no program to end
        for bad in ["Stop-Process", "taskkill", "Kill(", "-Force"] {
            assert!(!s.replace("Move-Item -Force", "").contains(bad), "{bad}");
        }
        assert_eq!(&script_bytes("x")[..3], &[0xEF, 0xBB, 0xBF]);
    }

    #[test]
    fn task_arguments_and_pid_file_name() {
        assert_eq!(
            task_arguments("C:\\a b\\x.ps1"),
            "-NoProfile -NonInteractive -WindowStyle Hidden -ExecutionPolicy Bypass -File \"C:\\a b\\x.ps1\""
        );
        assert_eq!(pid_file_of("C:\\a\\x.ps1"), "C:\\a\\x.pids.json");
        assert_eq!(pid_file_of("C:\\a\\x"), "C:\\a\\x.pids.json");
    }

    const GOOD: &str = r#"{"version":1,"wrapper":{"pid":4242,"start":133800000000000000,"image":"powershell"},"child":{"pid":5252,"start":133800000100000000,"image":"node"}}"#;

    #[test]
    fn the_record_format() {
        let f = parse_pid_file(GOOD).unwrap();
        assert_eq!(
            f.wrapper,
            Recorded {
                pid: 4242,
                start: 133_800_000_000_000_000
            }
        );
        assert_eq!(
            f.child,
            Some(Recorded {
                pid: 5252,
                start: 133_800_000_100_000_000
            })
        );
        // no child while node is not running
        let none = r#"{"version":1,"wrapper":{"pid":4242,"start":133800000000000000,"image":"powershell"},"child":null}"#;
        assert_eq!(parse_pid_file(none).unwrap().child, None);
        // a byte order mark in front is fine
        assert!(parse_pid_file(&format!("\u{feff}{GOOD}")).is_some());
        // rubbish is not a record; neither are system process ids or a missing start time
        for bad in [
            "",
            "{}",
            r#"{"version":2,"wrapper":{"pid":4242,"start":1}}"#,
            r#"{"version":1,"wrapper":{"pid":0,"start":133800000000000000}}"#,
            r#"{"version":1,"wrapper":{"pid":4,"start":133800000000000000}}"#,
            r#"{"version":1,"wrapper":{"pid":4242}}"#,
            r#"{"version":1,"wrapper":{"pid":4242,"start":-5}}"#,
            r#"{"version":1,"wrapper":{"pid":"4242","start":1}}"#,
            r#"{"version":1,"wrapper":{"pid":4242,"start":1},"child":{"pid":9}}"#,
        ] {
            assert!(parse_pid_file(bad).is_none(), "{bad}");
        }
    }

    #[test]
    fn the_same_process_decision() {
        let rec = Recorded {
            pid: 5252,
            start: 100,
        };
        let live = |start, image: &str| ProcInfo {
            start,
            image: image.to_owned(),
        };
        // same start and the expected program: ours
        assert_eq!(judge(&rec, "node", Some(&live(100, "node"))), Verdict::Ours);
        assert_eq!(judge(&rec, "node", Some(&live(100, "NODE"))), Verdict::Ours);
        // another start time: the id was given to something else
        assert!(matches!(
            judge(&rec, "node", Some(&live(101, "node"))),
            Verdict::Stale(_)
        ));
        // the right start time but another program
        assert!(matches!(
            judge(&rec, "node", Some(&live(100, "chrome"))),
            Verdict::Stale(_)
        ));
        // not there: nothing to do
        assert_eq!(judge(&rec, "node", None), Verdict::Gone);
    }

    fn rec_with_files(t: &TempDir, pids: Option<&str>) -> ServiceRec {
        let wrapper = t.path().join("stonewave-wave-agent-demo.ps1");
        fs::write(&wrapper, "x").unwrap();
        let w = wrapper.to_string_lossy().into_owned();
        if let Some(p) = pids {
            fs::write(pid_file_of(&w), p).unwrap();
        }
        ServiceRec {
            kind: "schtasks".into(),
            name: "StonewaveWave-agent-demo".into(),
            files: vec![w],
        }
    }

    fn live(fake: &Fake, pid: u32, start: i64, image: &str) {
        fake.procs.borrow_mut().push((
            pid,
            ProcInfo {
                start,
                image: image.into(),
            },
        ));
    }

    #[test]
    fn stop_ends_exactly_the_recorded_ids_child_first_and_removes_the_record() {
        let t = TempDir::new("stop1");
        let rec = rec_with_files(&t, Some(GOOD));
        let fake = Fake::new(&[]);
        live(&fake, 5252, 133_800_000_100_000_000, "node");
        live(&fake, 4242, 133_800_000_000_000_000, "powershell");
        live(&fake, 9999, 5, "node"); // someone else's node: not recorded, never touched
        let r = stop(&rec, &fake);
        assert_eq!(r.ended, vec![5252, 4242]);
        assert_eq!(*fake.ended.borrow(), vec![5252, 4242]);
        assert!(r.notes.is_empty() && r.problems.is_empty());
        assert!(!Path::new(&pid_file_of(&rec.files[0])).exists());
        assert!(fake.lines().is_empty(), "no program was run by name");
    }

    #[test]
    fn stop_leaves_a_stale_id_alone_and_says_so() {
        let t = TempDir::new("stop2");
        let rec = rec_with_files(&t, Some(GOOD));
        let fake = Fake::new(&[]);
        // the child id is now another program that started later; the wrapper is the real one
        live(&fake, 5252, 133_900_000_000_000_000, "node");
        live(&fake, 4242, 133_800_000_000_000_000, "powershell");
        let r = stop(&rec, &fake);
        assert_eq!(
            *fake.ended.borrow(),
            vec![4242],
            "only the verified wrapper"
        );
        assert_eq!(r.notes.len(), 1);
        assert!(
            r.notes[0].starts_with("stale pid 5252 (node), left alone"),
            "{:?}",
            r.notes
        );
        // wrong program under the right start time
        let t = TempDir::new("stop3");
        let rec = rec_with_files(&t, Some(GOOD));
        let fake = Fake::new(&[]);
        live(&fake, 5252, 133_800_000_100_000_000, "notepad");
        live(&fake, 4242, 133_800_000_000_000_000, "cmd");
        let r = stop(&rec, &fake);
        assert!(fake.ended.borrow().is_empty());
        assert_eq!(r.notes.len(), 2);
        assert!(r.problems.is_empty());
    }

    #[test]
    fn stop_with_processes_gone_or_no_record_does_nothing() {
        let t = TempDir::new("stop4");
        let rec = rec_with_files(&t, Some(GOOD));
        let fake = Fake::new(&[]);
        let r = stop(&rec, &fake);
        assert_eq!(r, StopReport::default());
        assert!(fake.ended.borrow().is_empty());
        assert!(
            !Path::new(&pid_file_of(&rec.files[0])).exists(),
            "the old record is cleared"
        );
        // no record at all
        let t = TempDir::new("stop5");
        let rec = rec_with_files(&t, None);
        assert_eq!(stop(&rec, &Fake::new(&[])), StopReport::default());
        // a record that is not ours: nothing is ended, the file stays
        let t = TempDir::new("stop6");
        let rec = rec_with_files(&t, Some("{\"hello\":1}"));
        let fake = Fake::new(&[]);
        live(&fake, 4242, 1, "powershell");
        let r = stop(&rec, &fake);
        assert!(fake.ended.borrow().is_empty());
        assert_eq!(r.notes.len(), 1);
        assert!(Path::new(&pid_file_of(&rec.files[0])).exists());
    }

    #[test]
    fn a_process_that_cannot_be_ended_is_a_problem_and_the_record_stays() {
        let t = TempDir::new("stop7");
        let rec = rec_with_files(&t, Some(GOOD));
        let fake = Fake::new(&[]);
        fake.fail_end.borrow_mut().push(5252);
        live(&fake, 5252, 133_800_000_100_000_000, "node");
        live(&fake, 4242, 133_800_000_000_000_000, "powershell");
        let r = stop(&rec, &fake);
        assert_eq!(r.problems.len(), 1);
        assert_eq!(r.ended, vec![4242]);
        assert!(
            Path::new(&pid_file_of(&rec.files[0])).exists(),
            "can be tried again"
        );
    }
}
