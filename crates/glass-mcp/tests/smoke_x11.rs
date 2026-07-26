//! End-to-end: the shipped `smoke` subcommand against a real app under Xvfb.
//! #[ignore]d (needs Xvfb + a target app); run via:
//!   cargo test -p glass-mcp --test smoke_x11 -- --ignored

mod common;

use common::Xvfb;
use std::process::Command;

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
#[ignore = "requires Xvfb and a target app; run via: cargo test -p glass-mcp --test smoke_x11 -- --ignored"]
fn smoke_x11_passes_against_a_real_app() {
    let xvfb = Xvfb::start();
    let dir = tempfile::tempdir().expect("tempdir");
    let report = dir.path().join("smoke.json");

    let out = Command::new(SERVER)
        .args(["smoke", "--backend", "x11", "--report"])
        .arg(&report)
        .env("DISPLAY", &xvfb.display)
        .output()
        .expect("run smoke");

    let stdout = String::from_utf8_lossy(&out.stdout);
    assert!(out.status.success(), "smoke failed:\n{stdout}");

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("report written"))
            .expect("report is JSON");
    assert_eq!(json["backend"], "x11");
    assert!(
        json["app"].as_str().is_some_and(|a| !a.is_empty()),
        "the selected app must be recorded: {json}"
    );
    let names: Vec<&str> = json["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .map(|c| c["name"].as_str().unwrap_or_default())
        .collect();
    for expected in [
        "start",
        "screenshot",
        "a11y snapshot",
        "interaction",
        "error honesty",
        "stop",
    ] {
        assert!(
            names.contains(&expected),
            "missing check {expected}: {names:?}"
        );
    }
}
