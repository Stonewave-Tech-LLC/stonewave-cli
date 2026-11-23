//! `stonewave --version` says the real version and the commit it was built from, in the one form the installers check:
//! `stonewave <x.y.z> (<commit>)`, where the commit is 8 hex characters, `-dirty` for a tree with changes, or `unknown` without git.

use std::process::Command;

#[test]
fn version_says_the_cargo_version_and_the_commit() {
    let out = Command::new(env!("CARGO_BIN_EXE_stonewave"))
        .arg("--version")
        .output()
        .unwrap();
    assert!(out.status.success());
    let text = String::from_utf8_lossy(&out.stdout);
    let line = text.trim();
    let want_prefix = format!("stonewave {} (", env!("CARGO_PKG_VERSION"));
    assert!(
        line.starts_with(&want_prefix) && line.ends_with(')'),
        "{line}"
    );
    let commit = &line[want_prefix.len()..line.len() - 1];
    let hex8 = commit.len() >= 8
        && commit
            .chars()
            .take(8)
            .all(|c| c.is_ascii_hexdigit() && !c.is_ascii_uppercase());
    assert!(
        hex8 && (commit.len() == 8 || commit.ends_with("-dirty")) || commit == "unknown",
        "{commit}"
    );
    assert!(!line.contains("early build"), "{line}");
    assert_eq!(text.lines().count(), 1, "one line only: {text}");
}
