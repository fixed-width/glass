//! What the two `#[ignore]`d smoke gates share: the private Xvfb the x11 run needs (the MCP
//! server connects to an X display at startup), the sway probe the wayland run needs, and the
//! report readers and assertions both apply to whichever backend they drove.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Output, Stdio};

pub struct Xvfb {
    child: Child,
    pub display: String,
    _displayfd: ChildStdout,
}

impl Xvfb {
    pub fn start() -> Xvfb {
        let mut child = Command::new("Xvfb")
            .args(["-displayfd", "1", "-screen", "0", "1024x768x24"])
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap_or_else(|e| panic!("could not spawn Xvfb (is it installed?): {e}"));
        let stdout = child.stdout.take().expect("piped stdout");
        let mut reader = BufReader::new(stdout);
        let mut line = String::new();
        if reader.read_line(&mut line).unwrap_or(0) == 0 {
            let _ = child.kill();
            let _ = child.wait();
            panic!("Xvfb exited without reporting a display");
        }
        let num: u32 = line.trim().parse().unwrap_or_else(|_| {
            let _ = child.kill();
            panic!("unexpected Xvfb -displayfd output: {line:?}");
        });
        Xvfb {
            child,
            display: format!(":{num}"),
            _displayfd: reader.into_inner(),
        }
    }
}

impl Drop for Xvfb {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
        if let Some(num) = self.display.strip_prefix(':') {
            let _ = std::fs::remove_file(format!("/tmp/.X{num}-lock"));
            let _ = std::fs::remove_file(format!("/tmp/.X11-unix/X{num}"));
        }
    }
}

/// The `(step, name)` pairs a report carries, in order.
pub fn rows(json: &serde_json::Value) -> Vec<(u64, String)> {
    json["checks"]
        .as_array()
        .expect("checks array")
        .iter()
        .map(|c| {
            // Not `unwrap_or_default`: this helper renders both operands of the real-vs-plan
            // comparison, so an unreadable field degrades both sides identically and the
            // assertion goes on passing over rows that no longer carry a step or a name.
            (
                c["step"].as_u64().expect("every row carries a step"),
                c["name"]
                    .as_str()
                    .expect("every row carries a name")
                    .to_string(),
            )
        })
        .collect()
}

pub fn read_report(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("report written"))
        .expect("report is JSON")
}

/// The checks a CI fixture controls end to end: the runner installs the target app and the
/// accessibility bus, so each must actually be `pass`. Neither `skip` nor `xfail` fails a run, so
/// a check that degraded to either would otherwise keep CI green while proving nothing.
///
/// `capabilities+doctor` is listed because it is the only check that establishes which backend the
/// run graded — it passes on a `warn` verdict, so requiring it forbids nothing a healthy run does.
pub const MUST_PASS: [&str; 8] = [
    "start",
    "capabilities+doctor",
    "screenshot",
    "a11y snapshot",
    "interaction",
    "logs",
    "error honesty",
    "stop",
];

/// Checks deliberately exempt from [`MUST_PASS`]. Empty today; a check the fixture cannot control
/// belongs here with its reason, which is the decision the test below refuses to let anyone skip.
pub const MAY_NOT_PASS: [&str; 0] = [];

/// Everything both gates assert about a real run: it succeeded, the report describes the backend
/// asked for, and its rows are exactly what the same binary's `--dry-run` previews. `extra_env` is
/// the harness's own isolation — the x11 gate's private display — applied to both invocations.
pub fn assert_smoke_gate(server: &str, backend: &str, extra_env: &[(&str, &str)]) {
    let dir = tempfile::tempdir().expect("tempdir");
    let report = dir.path().join("smoke.json");
    let out = run_smoke(server, backend, &report, extra_env, &[]);

    let stdout = String::from_utf8_lossy(&out.stdout);
    // A setup failure (no target app, no accessibility bus) writes no report and explains itself
    // on stderr, so a stdout-only message would leave nothing to triage from.
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(
        out.status.success(),
        "smoke failed:\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}"
    );

    let json = read_report(&report);
    assert_eq!(json["backend"], backend);
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
    // Sourcing the expectation from the same binary's own plan, rather than a list copied into a
    // test, is what keeps the two from drifting apart unnoticed.
    let plan_path = dir.path().join("plan.json");
    let plan = run_smoke(server, backend, &plan_path, extra_env, &["--dry-run"]);
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

fn run_smoke(
    server: &str,
    backend: &str,
    report: &Path,
    extra_env: &[(&str, &str)],
    extra_args: &[&str],
) -> Output {
    let mut cmd = Command::new(server);
    cmd.args(["smoke", "--backend", backend, "--report"])
        .arg(report)
        .args(extra_args);
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    cmd.output().expect("run smoke")
}

pub fn assert_fixture_checks_pass(json: &serde_json::Value, stdout: &str) {
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

/// Whether this host has a sway the Wayland backend can spawn, or why the question could not be
/// answered. A probe that cannot answer is not a host without sway: the first must fail the
/// test, the second must skip it.
#[derive(Debug)]
#[must_use]
pub enum SwayProbe {
    Available,
    Absent,
    Broken(String),
}

/// Set in the environment to make a missing sway a failure rather than a skip: CI sets it so a
/// green wayland gate always means the gate ran, and a developer laptop leaves it unset and skips.
pub const REQUIRE_WAYLAND: &str = "GLASS_SMOKE_REQUIRE_WAYLAND";

impl SwayProbe {
    /// Whether the gate can run, panicking rather than returning `false` for every answer a skip
    /// would misreport as a pass: a probe that could not answer, and — where [`REQUIRE_WAYLAND`]
    /// demands a real run — a host with no sway at all.
    #[must_use]
    pub fn can_run(self) -> bool {
        match self {
            Self::Available => true,
            Self::Absent if std::env::var_os(REQUIRE_WAYLAND).is_some() => panic!(
                "{REQUIRE_WAYLAND} is set, so this gate must run, but no glass-discoverable \
                 sway >=1.12 was found for the Wayland backend to spawn"
            ),
            Self::Absent => {
                eprintln!("no glass-discoverable sway >=1.12; skipping");
                false
            }
            Self::Broken(why) => panic!("cannot tell whether this host has sway: {why}"),
        }
    }
}

/// Does this host have a sway the Wayland backend can spawn? Asks the shipped binary's own probe
/// rather than re-implementing discovery, so the guard and the backend cannot disagree.
pub fn sway_probe(server: &str) -> SwayProbe {
    let out = match Command::new(server).args(["doctor", "--json"]).output() {
        Ok(out) => out,
        Err(e) => return SwayProbe::Broken(format!("could not run {server} doctor --json: {e}")),
    };
    match serde_json::from_slice(&out.stdout) {
        Ok(json) => classify(&json),
        // A doctor that died before printing leaves an empty stdout, so its status and stderr
        // are the only cause there is to report.
        Err(e) => SwayProbe::Broken(format!(
            "{server} doctor --json printed no readable JSON ({e}); {}, stdout: {:?}, \
             stderr tail: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            tail(&out.stderr),
        )),
    }
}

/// The last few lines of a captured stream, joined onto one line for a panic message.
fn tail(stream: &[u8]) -> String {
    let text = String::from_utf8_lossy(stream);
    let lines: Vec<&str> = text.lines().collect();
    lines[lines.len().saturating_sub(5)..].join(" | ")
}

/// Read a `doctor --json` document's verdict on sway. Split from running the binary because the
/// six ways the document can be unreadable are what needs testing, and none of them need a host.
fn classify(doctor_json: &serde_json::Value) -> SwayProbe {
    let Some(sections) = doctor_json["sections"].as_array() else {
        return SwayProbe::Broken(format!(
            "doctor --json had no \"sections\" array: {doctor_json}"
        ));
    };
    let Some(wayland) = sections.iter().find(|s| s["title"] == "wayland") else {
        return SwayProbe::Broken(format!(
            "doctor --json had no \"wayland\" section: {doctor_json}"
        ));
    };
    let Some(check) = wayland["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == "sway >=1.12")
    else {
        return SwayProbe::Broken(format!(
            "wayland section had no \"sway >=1.12\" check: {wayland}"
        ));
    };
    match check["status"].as_str() {
        Some("ok") => SwayProbe::Available,
        Some("warn" | "fail" | "skip") => SwayProbe::Absent,
        other => SwayProbe::Broken(format!(
            "sway >=1.12 check had unrecognized status {other:?}: {wayland}"
        )),
    }
}

#[cfg(test)]
mod must_pass_tests {
    use super::*;

    /// [`MUST_PASS`] is hand-maintained, and only renames and removals fail loudly: land a ninth
    /// check, forget to list it, and CI stays green forever while the new check is free to skip —
    /// the exact failure `MUST_PASS` exists to prevent.
    #[test]
    fn every_check_the_runner_emits_has_a_decided_status() {
        let mut decided: Vec<&str> = MUST_PASS
            .iter()
            .chain(MAY_NOT_PASS.iter())
            .copied()
            .collect();
        decided.sort_unstable();
        let mut emitted = glass_mcp::smoke::all_check_names();
        emitted.sort_unstable();
        assert_eq!(
            decided, emitted,
            "every check a report can carry must be listed in MUST_PASS or MAY_NOT_PASS"
        );
    }
}

#[cfg(test)]
mod sway_probe_tests {
    use super::*;
    use serde_json::json;

    /// A `doctor --json` document shaped like the real one, with the sway check reporting
    /// `status`. A shape that drifted from doctor's own would make every host `Broken`, which
    /// panics — so this fixture cannot rot quietly.
    fn doctor_reporting(status: &str) -> serde_json::Value {
        json!({
            "sections": [{
                "title": "wayland",
                "backend": "wayland",
                "checks": [
                    { "name": "sway >=1.12", "status": status, "detail": "sway version 1.12 at …" },
                    { "name": "software GL (Mesa)", "status": "ok", "detail": "…" },
                ],
            }],
        })
    }

    fn why_broken(json: &serde_json::Value) -> String {
        match classify(json) {
            SwayProbe::Broken(why) => why,
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn an_ok_sway_check_means_the_gate_can_run() {
        assert!(matches!(
            classify(&doctor_reporting("ok")),
            SwayProbe::Available
        ));
    }

    /// Every verdict doctor can reach without sway. Reading one of these as `Broken` would fail
    /// a developer laptop that simply has no sway; reading it as `Available` would run the gate
    /// against a compositor that is not there.
    #[test]
    fn every_non_ok_sway_verdict_means_the_host_has_no_sway() {
        for status in ["warn", "fail", "skip"] {
            assert!(
                matches!(classify(&doctor_reporting(status)), SwayProbe::Absent),
                "status {status:?} must read as a host without sway"
            );
        }
    }

    #[test]
    fn a_status_outside_the_known_set_cannot_be_read_as_an_answer() {
        let why = why_broken(&doctor_reporting("green"));
        assert!(why.contains("green"), "must name what it got: {why}");
    }

    #[test]
    fn a_document_with_no_sections_cannot_be_read_as_an_answer() {
        let why = why_broken(&json!({}));
        assert!(
            why.contains("sections"),
            "must name what was missing: {why}"
        );
    }

    #[test]
    fn a_document_with_no_wayland_section_cannot_be_read_as_an_answer() {
        let why = why_broken(&json!({ "sections": [{ "title": "x11", "checks": [] }] }));
        assert!(why.contains("wayland"), "must name what was missing: {why}");
    }

    /// A doctor that dies before printing leaves an empty stdout, so its exit status and stderr
    /// are the only cause the resulting panic can name.
    #[cfg(unix)]
    #[test]
    fn a_doctor_that_prints_no_json_reports_its_status_and_stderr() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("glass-mcp");
        std::fs::write(
            &fake,
            b"#!/bin/sh\necho 'cannot open display' >&2\nexit 3\n",
        )
        .expect("write");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        let why = match sway_probe(fake.to_str().expect("utf-8 path")) {
            SwayProbe::Broken(why) => why,
            other => panic!("expected Broken, got {other:?}"),
        };
        assert!(why.contains('3'), "must name the exit status: {why}");
        assert!(
            why.contains("cannot open display"),
            "must carry the stderr tail: {why}"
        );
    }

    #[test]
    fn a_wayland_section_with_no_sway_check_cannot_be_read_as_an_answer() {
        let why = why_broken(&json!({
            "sections": [{
                "title": "wayland",
                "checks": [{ "name": "software GL (Mesa)", "status": "ok" }],
            }],
        }));
        assert!(why.contains("sway >=1.12"), "must name the check: {why}");
    }
}
