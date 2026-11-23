//! Puts the commit this program was built from into `stonewave --version`.
//!
//! `STONEWAVE_COMMIT` (environment) wins, for a build from a source archive that has no git folder. Otherwise the first 8 characters of
//! HEAD, with `-dirty` added when the working tree has changes, so a build of unpublished code can never be mistaken for a release.
//! Without git the commit is `unknown`.

use std::process::Command;

fn git(args: &[&str]) -> Option<String> {
    let out = Command::new("git").args(args).output().ok()?;
    out.status
        .success()
        .then(|| String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

fn main() {
    println!("cargo:rerun-if-env-changed=STONEWAVE_COMMIT");
    // what a new commit changes: HEAD, the branch it points to, the packed refs and the index (`--git-path` also finds them in a worktree)
    for what in ["HEAD", "index", "packed-refs"] {
        if let Some(p) = git(&["rev-parse", "--git-path", what]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    if let Some(branch) = git(&["symbolic-ref", "-q", "HEAD"]) {
        if let Some(p) = git(&["rev-parse", "--git-path", &branch]) {
            println!("cargo:rerun-if-changed={p}");
        }
    }
    let commit = std::env::var("STONEWAVE_COMMIT")
        .ok()
        .filter(|c| {
            (1..=40).contains(&c.len())
                && c.chars().all(|ch| ch.is_ascii_alphanumeric() || ch == '-')
        })
        .or_else(|| {
            let head = git(&["rev-parse", "--short=8", "HEAD"])?;
            let dirty = git(&["status", "--porcelain", "--untracked-files=no"])
                .is_some_and(|s| !s.is_empty());
            Some(if dirty { format!("{head}-dirty") } else { head })
        })
        .unwrap_or_else(|| "unknown".to_owned());
    println!("cargo:rustc-env=STONEWAVE_COMMIT={commit}");
}
