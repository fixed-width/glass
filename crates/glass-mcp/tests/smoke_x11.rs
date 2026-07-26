//! End-to-end: the shipped `smoke` subcommand against a real app under Xvfb.
//! #[ignore]d (needs Xvfb + a target app); run via:
//!   cargo test -p glass-mcp --test smoke_x11 -- --ignored

mod common;

use common::Xvfb;
use std::process::Command;

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

/// Every row a real run must produce, in order. Asserting the whole set — not just that a
/// few names appear somewhere — is what makes a check that quietly stopped running visible.
const EXPECTED_ROWS: [(u64, &str); 9] = [
    (1, "version"),
    (2, "start"),
    (3, "capabilities+doctor"),
    (4, "screenshot"),
    (5, "a11y snapshot"),
    (6, "interaction"),
    (8, "logs"),
    (9, "error honesty"),
    (10, "stop"),
];

/// The checks the CI fixture controls end to end: the runner installs the target app and the
/// accessibility bus, so each of these must actually be `pass`. `Skip` exits 0, so without
/// this a check that degraded to a skip — `interaction` when the app stops exposing an
/// editable element, say — would keep CI green forever while proving nothing.
///
/// `version` is skipped without `--expect-version`, and `capabilities+doctor` grades the host
/// environment rather than the fixture, so neither is pinned to `pass` here.
const MUST_PASS: [&str; 7] = [
    "start",
    "screenshot",
    "a11y snapshot",
    "interaction",
    "logs",
    "error honesty",
    "stop",
];

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
    // A setup failure (no target app, no accessibility bus) writes no report and explains
    // itself on stderr, so a stdout-only message would leave nothing to triage from.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "smoke failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let json: serde_json::Value =
        serde_json::from_str(&std::fs::read_to_string(&report).expect("report written"))
            .expect("report is JSON");
    assert_eq!(json["backend"], "x11");
    assert!(
        json["app"].as_str().is_some_and(|a| !a.is_empty()),
        "the selected app must be recorded: {json}"
    );

    let checks = json["checks"].as_array().expect("checks array");
    let rows: Vec<(u64, &str)> = checks
        .iter()
        .map(|c| {
            (
                c["step"].as_u64().unwrap_or_default(),
                c["name"].as_str().unwrap_or_default(),
            )
        })
        .collect();
    assert_eq!(
        rows,
        EXPECTED_ROWS.to_vec(),
        "the report must carry every check, in order: {stdout}"
    );

    for name in MUST_PASS {
        let status = checks
            .iter()
            .find(|c| c["name"].as_str() == Some(name))
            .and_then(|c| c["status"].as_str())
            .unwrap_or_default();
        assert_eq!(
            status, "pass",
            "check {name:?} is fixture-controlled and must pass, not degrade to {status:?}:\n{stdout}"
        );
    }
}
