//! What the three `#[ignore]`d smoke gates share: the private Xvfb the x11 run needs (the MCP
//! server connects to an X display at startup), the host probes the wayland and android runs need,
//! and the report readers and assertions all three apply to whichever backend they drove.

#![allow(dead_code)]

use std::io::{BufRead, BufReader};
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

/// Whether this host has what a gate needs, or why it cannot be used. A host that simply lacks the
/// feature is not the same as one that has it and cannot run it: the first must skip, and the
/// second — like a probe that could not answer at all — must fail loudly.
#[derive(Debug)]
#[must_use]
pub enum HostProbe {
    Available,
    /// The probe found no way for this gate to run here. Skipping is the honest answer to that;
    /// where a run is mandatory, the `require_*` variable turns the skip into a failure.
    Absent,
    /// Either the probe could not answer, or this host has the feature and glass will not use it.
    /// Neither is a host to skip: the first would pass off an unasked question as an absent
    /// feature, the second a setup gap the operator can act on.
    Broken(String),
}

/// Set in the environment to make a missing sway a failure rather than a skip: CI sets it so a
/// green wayland gate always means the gate ran, and a developer laptop leaves it unset and skips.
pub const REQUIRE_WAYLAND: &str = "GLASS_SMOKE_REQUIRE_WAYLAND";

/// Set in the environment to make an absent android host a failure rather than a skip, matching
/// [`REQUIRE_WAYLAND`].
pub const REQUIRE_ANDROID: &str = "GLASS_SMOKE_REQUIRE_ANDROID";

impl HostProbe {
    /// Whether the gate can run, panicking rather than returning `false` for every answer a skip
    /// would misreport as a pass: a probe that could not answer, and — where `require_var`
    /// demands a real run — a host the probe found no way to run on.
    #[must_use]
    pub fn can_run(self, require_var: &str, what: &str) -> bool {
        match self {
            Self::Available => true,
            Self::Absent if std::env::var_os(require_var).is_some() => {
                panic!("{require_var} is set, so this gate must run, but this host has no {what}")
            }
            Self::Absent => {
                eprintln!("no {what}; skipping");
                false
            }
            Self::Broken(why) => {
                panic!("the gate cannot run here, and this host is not one to skip: {why}")
            }
        }
    }
}

/// Does this host have a sway the Wayland backend can spawn? Asks the shipped binary's own probe
/// rather than re-implementing discovery, so the guard and the backend cannot disagree.
pub fn sway_probe(server: &str) -> HostProbe {
    let out = match Command::new(server).args(["doctor", "--json"]).output() {
        Ok(out) => out,
        Err(e) => return HostProbe::Broken(format!("could not run {server} doctor --json: {e}")),
    };
    match serde_json::from_slice(&out.stdout) {
        Ok(json) => classify(&json),
        // A doctor that died before printing leaves an empty stdout, so its status and stderr
        // are the only cause there is to report.
        Err(e) => HostProbe::Broken(format!(
            "{server} doctor --json printed no readable JSON ({e}); {}, stdout: {:?}, \
             stderr tail: {}",
            out.status,
            String::from_utf8_lossy(&out.stdout),
            tail(&out.stderr),
        )),
    }
}

/// Does this host have an android device the backend can drive? Runs doctor with
/// `GLASS_BACKEND=android`, because doctor softens an inactive android section's failures to
/// warnings — a `device` check that says `fail` would otherwise arrive as `warn` and read as
/// "will boot an AVD".
pub fn android_probe(server: &str) -> HostProbe {
    let out = match Command::new(server)
        .args(["doctor", "--json"])
        .env("GLASS_BACKEND", "android")
        .output()
    {
        Ok(out) => out,
        Err(e) => return HostProbe::Broken(format!("could not run {server} doctor --json: {e}")),
    };
    match serde_json::from_slice(&out.stdout) {
        Ok(json) => classify_android(&json),
        Err(e) => HostProbe::Broken(format!(
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

/// One check's status from a `doctor --json` document, or why the document could not answer.
fn check_status<'a>(
    doc: &'a serde_json::Value,
    section: &str,
    check: &str,
) -> Result<(&'a str, &'a str), String> {
    let Some(sections) = doc["sections"].as_array() else {
        return Err(format!("doctor --json had no \"sections\" array: {doc}"));
    };
    let Some(s) = sections.iter().find(|s| s["title"] == section) else {
        return Err(format!("doctor --json had no {section:?} section: {doc}"));
    };
    let Some(c) = s["checks"]
        .as_array()
        .into_iter()
        .flatten()
        .find(|c| c["name"] == check)
    else {
        return Err(format!("{section} section had no {check:?} check: {s}"));
    };
    let status = c["status"]
        .as_str()
        .ok_or_else(|| format!("{check:?} check carried no status string: {c}"))?;
    Ok((status, c["detail"].as_str().unwrap_or_default()))
}

/// Read a `doctor --json` document's verdict on sway. Split from running the binary because the
/// six ways the document can be unreadable are what needs testing, and none of them need a host.
fn classify(doc: &serde_json::Value) -> HostProbe {
    let (status, _) = match check_status(doc, "wayland", "sway >=1.12") {
        Ok(v) => v,
        Err(why) => return HostProbe::Broken(why),
    };
    match status {
        "ok" => HostProbe::Available,
        "warn" | "fail" | "skip" => HostProbe::Absent,
        other => HostProbe::Broken(format!(
            "sway >=1.12 check had unrecognized status {other:?}: {doc}"
        )),
    }
}

/// Read a `doctor --json` document's verdict on android: can the gate run here, did the probe find
/// no way to get a device, or does this host have an AVD and refuse to use one? Split from running
/// the binary because those readings are what needs testing, and none of them need a device.
fn classify_android(doc: &serde_json::Value) -> HostProbe {
    let (adb, _) = match check_status(doc, "android", "adb") {
        Ok(v) => v,
        Err(why) => return HostProbe::Broken(why),
    };
    // No adb is a host with no android SDK: nothing to report, nothing to fix for this gate.
    if adb != "ok" {
        return HostProbe::Absent;
    }
    let (device, detail) = match check_status(doc, "android", "device") {
        Ok(v) => v,
        Err(why) => return HostProbe::Broken(why),
    };
    match device {
        // Attached, or about to boot: the runner inherits glass's auto-boot lifecycle.
        "ok" | "warn" => HostProbe::Available,
        // `fail` is two conditions — no device and nothing to boot, and a refusal such as several
        // online devices with no serial chosen. The `emulator` check is the only signal that
        // separates them: doctor reports `device` as `warn` (will boot) whenever an AVD exists, so
        // a `fail` beside AVDs is a refusal.
        "fail" => match check_status(doc, "android", "emulator") {
            Ok(("ok", _)) => HostProbe::Broken(format!(
                "glass will not use any device on this host: {detail}"
            )),
            // Every non-`ok` verdict means no AVD to fall back on — no emulator binary, or none
            // listed. Usually a host with no android set up; a refusal on a host with devices
            // attached but no AVDs is indistinguishable from that in what doctor exposes, so it
            // skips too. `REQUIRE_ANDROID` is what makes that safe where a run is mandatory.
            Ok(("warn" | "fail" | "skip", _)) => HostProbe::Absent,
            Ok((other, _)) => HostProbe::Broken(format!(
                "the android emulator check had unrecognized status {other:?}: {doc}"
            )),
            Err(why) => HostProbe::Broken(why),
        },
        other => HostProbe::Broken(format!(
            "the android device check had unrecognized status {other:?}: {doc}"
        )),
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
            HostProbe::Broken(why) => why,
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    #[test]
    fn an_ok_sway_check_means_the_gate_can_run() {
        assert!(matches!(
            classify(&doctor_reporting("ok")),
            HostProbe::Available
        ));
    }

    /// Every verdict doctor can reach without sway. Reading one of these as `Broken` would fail
    /// a developer laptop that simply has no sway; reading it as `Available` would run the gate
    /// against a compositor that is not there.
    #[test]
    fn every_non_ok_sway_verdict_means_the_host_has_no_sway() {
        for status in ["warn", "fail", "skip"] {
            assert!(
                matches!(classify(&doctor_reporting(status)), HostProbe::Absent),
                "status {status:?} must read as a host without sway"
            );
        }
    }

    #[test]
    fn a_status_outside_the_known_set_cannot_be_read_as_an_answer() {
        let why = why_broken(&doctor_reporting("green"));
        assert!(
            why.contains("unrecognized status \"green\""),
            "must name what it got: {why}"
        );
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
            HostProbe::Broken(why) => why,
            other => panic!("expected Broken, got {other:?}"),
        };
        assert!(
            why.contains("exit status: 3"),
            "must name the exit status: {why}"
        );
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

#[cfg(test)]
mod android_probe_tests {
    use super::*;
    use serde_json::json;

    /// A `doctor --json` document shaped like the real one's android section: all seven checks
    /// doctor always emits, in its own order. The four this classifier does not read carry what a
    /// plain (non-`--deep`) run produces — optional companions and unrequested deep probes all
    /// skip. A shape that drifted from doctor's own would make every host `Broken`, which panics,
    /// so this fixture cannot rot quietly; holding only the checks read today would instead answer
    /// a classifier that reaches for a fifth with a document doctor never emits.
    fn doctor_reporting(
        adb: &str,
        emulator: &str,
        device: &str,
        device_detail: &str,
    ) -> serde_json::Value {
        json!({
            "sections": [{
                "title": "android",
                "backend": "android",
                "checks": [
                    { "name": "adb", "status": adb, "detail": "adb at …" },
                    { "name": "emulator", "status": emulator, "detail": "emulator at …" },
                    { "name": "device", "status": device, "detail": device_detail },
                    { "name": "agent", "status": "skip", "detail": "not configured …" },
                    { "name": "a11y-service", "status": "skip", "detail": "not configured …" },
                    { "name": "screencap", "status": "skip", "detail": "…" },
                    { "name": "uiautomator", "status": "skip", "detail": "…" },
                ],
            }],
        })
    }

    fn why_broken(json: &serde_json::Value) -> String {
        match classify_android(json) {
            HostProbe::Broken(why) => why,
            other => panic!("expected Broken, got {other:?}"),
        }
    }

    /// Both verdicts a healthy host can reach: attached to a device, or about to boot one. The
    /// runner inherits glass's auto-boot lifecycle, so "will boot" is a host that can run.
    #[test]
    fn an_attached_or_bootable_device_means_the_gate_can_run() {
        for device in ["ok", "warn"] {
            assert!(
                matches!(
                    classify_android(&doctor_reporting("ok", "ok", device, "…")),
                    HostProbe::Available
                ),
                "device {device:?} must read as a host that can run"
            );
        }
    }

    /// No adb is a host without android set up — skip, do not fail.
    #[test]
    fn a_host_without_adb_is_absent_whatever_the_device_check_says() {
        for device in ["skip", "fail", "ok"] {
            assert!(
                matches!(
                    classify_android(&doctor_reporting("fail", "ok", device, "…")),
                    HostProbe::Absent
                ),
                "no adb must read as absent even when device says {device:?}"
            );
        }
    }

    /// The ordinary contributor: a distro-packaged `adb`, no phone and no AVD. Nothing here is
    /// set up for android and nothing is misconfigured, so this must skip — the same shape as a
    /// host with no sway — rather than fail a gate the developer never asked to run.
    #[test]
    fn a_host_with_adb_but_nothing_to_run_is_absent() {
        for emulator in ["warn", "fail", "skip"] {
            assert!(
                matches!(
                    classify_android(&doctor_reporting(
                        "ok",
                        emulator,
                        "fail",
                        "no online device and no AVD to boot",
                    )),
                    HostProbe::Absent
                ),
                "emulator {emulator:?} means no AVD to boot, so this host has nothing to run"
            );
        }
    }

    /// The other half of a failing `device` check: an AVD exists, so doctor would have said "will
    /// boot" had that been the whole story — the failure is a refusal. That is a host with android
    /// set up, which is worth reporting rather than skipping quietly, and doctor's own detail is
    /// the only thing that says what glass refused over.
    #[test]
    fn a_refusal_on_a_host_with_an_avd_is_reported_not_skipped() {
        let why = why_broken(&doctor_reporting(
            "ok",
            "ok",
            "fail",
            "2 online devices; set GLASS_ANDROID_SERIAL to one of: [emulator-5554, emulator-5556]",
        ));
        assert!(
            why.contains("GLASS_ANDROID_SERIAL"),
            "must carry doctor's own detail: {why}"
        );
    }

    #[test]
    fn a_device_status_outside_the_known_set_cannot_be_read_as_an_answer() {
        let why = why_broken(&doctor_reporting("ok", "ok", "green", "…"));
        // The fuller phrase, not just "green": the fixture document also carries the raw
        // `"status": "green"` field, so a bare `contains("green")` would pass even if the
        // extracted status were never read into the message at all.
        assert!(
            why.contains("unrecognized status \"green\""),
            "must name what it got: {why}"
        );
    }

    /// The emulator check is what decides skip-or-report, so a verdict nobody mapped cannot be
    /// guessed at in either direction.
    #[test]
    fn an_emulator_status_outside_the_known_set_cannot_be_read_as_an_answer() {
        let why = why_broken(&doctor_reporting("ok", "green", "fail", "…"));
        assert!(
            why.contains("emulator check had unrecognized status \"green\""),
            "must name the check and what it got: {why}"
        );
    }

    /// Without the emulator check there is nothing to tell a refusal from an empty host, and
    /// guessing either way is worse than saying so.
    #[test]
    fn an_android_section_with_no_emulator_check_cannot_be_read_as_an_answer() {
        let why = why_broken(&json!({
            "sections": [{
                "title": "android",
                "checks": [
                    { "name": "adb", "status": "ok", "detail": "…" },
                    { "name": "device", "status": "fail", "detail": "…" },
                ],
            }],
        }));
        assert!(why.contains("emulator"), "must name the check: {why}");
    }

    #[test]
    fn a_document_with_no_android_section_cannot_be_read_as_an_answer() {
        let why = why_broken(&json!({ "sections": [{ "title": "x11", "checks": [] }] }));
        assert!(why.contains("android"), "must name what was missing: {why}");
    }

    #[test]
    fn an_android_section_with_no_device_check_cannot_be_read_as_an_answer() {
        let why = why_broken(&json!({
            "sections": [{
                "title": "android",
                "checks": [{ "name": "adb", "status": "ok", "detail": "…" }],
            }],
        }));
        assert!(why.contains("device"), "must name the check: {why}");
    }
}
