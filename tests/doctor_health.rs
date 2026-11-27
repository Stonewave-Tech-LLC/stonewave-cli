//! `stonewave doctor --health --json` as the nightly check and the server alarm see it: the real binary,
//! one JSON object on stdout in the schema `stonewave.health/1`, whatever the machine is.
//!
//! This also runs on a machine that has none of the tools (a developer laptop): what cannot be read is
//! listed under `not_collected`, and the program still answers.

use std::process::Command;

fn run(args: &[&str]) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_stonewave"))
        .args(args)
        .env_remove("NO_COLOR")
        .output()
        .expect("the binary runs")
}

#[test]
fn one_json_object_in_the_fixed_schema() {
    let out = run(&["doctor", "--health", "--json"]);
    // 0: nothing high or critical; 1: something is. Never anything else (2 would be a refused call).
    let code = out.status.code();
    assert!(matches!(code, Some(0) | Some(1)), "exit {code:?}");
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(
        !text.contains('\u{1b}'),
        "no colour or cursor codes: {text:?}"
    );
    assert_eq!(text.lines().count(), 1, "one line: {text:?}");
    let v: serde_json::Value = serde_json::from_str(text.trim()).expect("one JSON object");
    assert_eq!(v["schema"], "stonewave.health/1");
    for key in ["stonewave", "host", "ts", "root", "summary", "checks"] {
        assert!(v.get(key).is_some(), "missing {key}: {text}");
    }
    let ids: Vec<&str> = v["checks"]
        .as_array()
        .expect("checks is a list")
        .iter()
        .map(|c| c["id"].as_str().expect("id"))
        .collect();
    assert_eq!(
        ids,
        [
            "system",
            "units",
            "reboot",
            "updates",
            "autoupdates",
            "storage",
            "raid",
            "smart",
            "ports"
        ]
    );
    for c in v["checks"].as_array().unwrap() {
        assert!(
            ["collected", "partial", "not_collected"].contains(&c["collection"].as_str().unwrap()),
            "{c}"
        );
        for f in c["findings"].as_array().unwrap() {
            // source duty: every finding says where it comes from
            assert!(
                f["source"].get("command").is_some() || f["source"].get("file").is_some(),
                "a finding without a source: {f}"
            );
            assert!(
                ["critical", "high", "medium", "low", "info"]
                    .contains(&f["severity"].as_str().unwrap())
            );
        }
    }
    // exit code and summary agree
    let highest = v["summary"]["highest"].as_str().unwrap();
    assert_eq!(code == Some(1), highest == "high" || highest == "critical");
}

#[test]
fn the_plain_screen_has_no_escape_codes_and_names_what_it_could_not_check() {
    let out = run(&["doctor", "--health"]);
    let text = String::from_utf8(out.stdout).expect("utf-8");
    assert!(text.starts_with("stonewave doctor --health"), "{text}");
    assert!(!text.contains('\u{1b}'), "{text:?}");
    assert!(text.contains("smart"), "{text}");
}

#[test]
fn health_is_refused_where_it_means_nothing() {
    for args in [
        &["status", "--health"][..],
        &["connect", "--health"][..],
        &["--health"][..],
    ] {
        let out = run(args);
        assert_eq!(out.status.code(), Some(2), "{args:?}");
    }
}
