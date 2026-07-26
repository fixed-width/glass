//! End-to-end: the shipped `smoke` subcommand against a real app under Xvfb.
//! #[ignore]d (needs Xvfb + a target app); run via:
//!   cargo test -p glass-mcp --test smoke_x11 -- --ignored

mod common;

use common::Xvfb;
use std::process::Command;

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

/// The `(step, name)` pairs a report carries, in order.
fn rows(json: &serde_json::Value) -> Vec<(u64, String)> {
    json["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .map(|c| {
            (
                c["step"].as_u64().unwrap_or_default(),
                c["name"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

fn read_report(path: &std::path::Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("report written"))
        .expect("report is JSON")
}

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

    let json = read_report(&report);
    assert_eq!(json["backend"], "x11");
    assert_eq!(json["mode"], "full", "a real run is not a plan: {json}");
    assert_eq!(
        json["app"]["state"], "selected",
        "a real run drove an app, so the report must say which: {json}"
    );
    assert!(
        json["app"]["value"].as_str().is_some_and(|a| !a.is_empty()),
        "the selected app must be named: {json}"
    );

    // The invariant `smoke/mod.rs` declares: a real run's rows are the `--dry-run` preview's
    // rows. Sourcing the expectation from the same binary's own plan — rather than a list
    // copied into this file — is what keeps the two from drifting apart unnoticed.
    let plan_path = dir.path().join("plan.json");
    let plan = Command::new(SERVER)
        .args(["smoke", "--backend", "x11", "--dry-run", "--report"])
        .arg(&plan_path)
        .env("DISPLAY", &xvfb.display)
        .output()
        .expect("run smoke --dry-run");
    assert!(
        plan.status.success(),
        "smoke --dry-run failed: {}",
        String::from_utf8_lossy(&plan.stderr)
    );
    let plan = read_report(&plan_path);
    assert_eq!(plan["mode"], "dry_run", "a plan must say so: {plan}");
    assert_eq!(
        rows(&json),
        rows(&plan),
        "a real run must carry exactly the checks --dry-run previews: {stdout}"
    );

    let checks = json["checks"].as_array().expect("checks array");
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
