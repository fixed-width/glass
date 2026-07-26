//! Xvfb harness (same approach as glass-testapp): the MCP server connects to an
//! X display at startup, so the smoke test gives it a private Xvfb.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
use std::path::Path;
use std::process::{Child, ChildStdout, Command, Stdio};

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
            (
                c["step"].as_u64().unwrap_or_default(),
                c["name"].as_str().unwrap_or_default().to_string(),
            )
        })
        .collect()
}

pub fn read_report(path: &Path) -> serde_json::Value {
    serde_json::from_str(&std::fs::read_to_string(path).expect("report written"))
        .expect("report is JSON")
}

/// The checks a CI fixture controls end to end: the runner installs the target app and the
/// accessibility bus, so each must actually be `pass`. `Skip` exits 0, so a check that degraded
/// to a skip would otherwise keep CI green while proving nothing.
///
/// `capabilities+doctor` grades the host environment rather than the fixture, so it is absent.
pub const MUST_PASS: [&str; 7] = [
    "start",
    "screenshot",
    "a11y snapshot",
    "interaction",
    "logs",
    "error honesty",
    "stop",
];

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
pub enum SwayProbe {
    Available,
    Absent,
    Broken(String),
}

/// Does this host have a sway the Wayland backend can spawn? Asks the shipped binary's own probe
/// rather than re-implementing discovery, so the guard and the backend cannot disagree.
pub fn sway_probe(server: &str) -> SwayProbe {
    let out = match Command::new(server).args(["doctor", "--json"]).output() {
        Ok(out) => out,
        Err(e) => return SwayProbe::Broken(format!("could not run {server} doctor --json: {e}")),
    };
    let json: serde_json::Value = match serde_json::from_slice(&out.stdout) {
        Ok(json) => json,
        Err(e) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            return SwayProbe::Broken(format!(
                "doctor --json printed non-JSON stdout ({e}): {stdout}"
            ));
        }
    };
    let Some(sections) = json["sections"].as_array() else {
        return SwayProbe::Broken(format!("doctor --json had no \"sections\" array: {json}"));
    };
    let Some(wayland) = sections.iter().find(|s| s["title"] == "wayland") else {
        return SwayProbe::Broken(format!("doctor --json had no \"wayland\" section: {json}"));
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
