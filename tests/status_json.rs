//! `stonewave status --json` as an agent sees it: the real binary, one JSON object on stdout.

use std::process::Command;

#[test]
fn one_json_object_no_colour_exit_zero() {
    let out = Command::new(env!("CARGO_BIN_EXE_stonewave"))
        .args(["status", "--json"])
        .env_remove("NO_COLOR")
        .output()
        .expect("the binary runs");
    // A report could be made, so the exit code is 0 whatever the state of the machine is.
    assert!(out.status.success(), "exit {:?}", out.status.code());
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(
        !text.contains('\u{1b}'),
        "no colour or cursor codes: {text:?}"
    );
    assert_eq!(text.lines().count(), 1, "one line: {text:?}");
    let v: serde_json::Value = serde_json::from_str(text.trim()).expect("one JSON object");
    assert!(v.is_object());
    for key in [
        "stonewave",
        "ts",
        "readable",
        "set_up",
        "connected",
        "daemon",
        "agent_ssh",
        "health",
    ] {
        assert!(v.get(key).is_some(), "missing {key}: {text}");
    }
}

#[test]
fn json_is_refused_where_it_means_nothing() {
    let out = Command::new(env!("CARGO_BIN_EXE_stonewave"))
        .args(["connect", "--json"])
        .output()
        .expect("the binary runs");
    assert_eq!(out.status.code(), Some(2));
}
