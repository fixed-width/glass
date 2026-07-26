//! End-to-end: the shipped `smoke` subcommand against a real app under the Wayland backend's
//! own headless sway. #[ignore]d (needs sway >=1.12 and a target app); run via:
//!   cargo test -p glass-mcp --test smoke_wayland -- --ignored

mod common;

use common::{SwayProbe, assert_fixture_checks_pass, read_report, rows, sway_probe};
use std::process::Command;

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
#[ignore = "requires sway >=1.12 and a target app; run via: cargo test -p glass-mcp --test smoke_wayland -- --ignored"]
fn smoke_wayland_passes_against_a_real_app() {
    // The backend spawns its own compositor, so there is nothing to start here — but a host
    // without sway must skip rather than report a failure it cannot act on.
    match sway_probe(SERVER) {
        SwayProbe::Broken(why) => panic!("cannot tell whether this host has sway: {why}"),
        SwayProbe::Absent => {
            eprintln!("no glass-discoverable sway >=1.12; skipping");
            return;
        }
        SwayProbe::Available => {}
    }

    let dir = tempfile::tempdir().expect("tempdir");
    let report = dir.path().join("smoke.json");

    let out = Command::new(SERVER)
        .args(["smoke", "--backend", "wayland", "--report"])
        .arg(&report)
        .output()
        .expect("run smoke");

    let stdout = String::from_utf8_lossy(&out.stdout);
    // A setup failure (no target app, no accessibility bus) writes no report and explains itself
    // on stderr, so a stdout-only message would leave nothing to triage from.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "smoke failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let json = read_report(&report);
    assert_eq!(json["backend"], "wayland");
    assert_eq!(json["mode"], "full", "a real run is not a plan: {json}");
    assert_eq!(
        json["app"]["state"], "selected",
        "a real run drove an app, so the report must say which: {json}"
    );
    assert!(
        json["app"]["value"].as_str().is_some_and(|a| !a.is_empty()),
        "the selected app must be named: {json}"
    );

    // The invariant `smoke/mod.rs` declares: a real run's rows are the `--dry-run` preview's rows.
    // Sourcing the expectation from the same binary's own plan, rather than a list copied into
    // this file, is what keeps the two from drifting apart unnoticed.
    let plan_path = dir.path().join("plan.json");
    let plan = Command::new(SERVER)
        .args(["smoke", "--backend", "wayland", "--dry-run", "--report"])
        .arg(&plan_path)
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

    assert_fixture_checks_pass(&json, &stdout);
}
