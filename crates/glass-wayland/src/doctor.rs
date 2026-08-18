//! Environment checks for the Wayland backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`wayland_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without sway.

use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

use glass_core::{AppSpec, Check, CheckStatus, ProbeFailure};

use crate::command::{build_sway_command, sway_config};
use crate::platform::{
    CHECK_THAT_SWAY, NoSway, VERSION_PROBE_BUDGET, VersionAnswer, ask_sway_version,
    resolve_sway_verdict,
};
use crate::swayipc::Ipc;

/// Probe the Wayland backend's environment. `deep` additionally spawns and tears down
/// a headless sway to prove it actually starts.
pub fn checks(deep: bool) -> Vec<Check> {
    let sway = discover_sway();
    let gl = gl_present_in(EGL_SONAMES, DRI_DIRS);
    let deep_spawn = match (deep, &sway) {
        (true, Ok((path, _))) => Some(probe_sway(path)),
        _ => None,
    };
    wayland_checks(&sway, gl, deep_spawn)
}

/// Pure: build the Wayland checks from gathered facts.
fn wayland_checks(
    sway: &Result<(PathBuf, VersionAnswer), NoSway>,
    gl_present: bool,
    deep_spawn: Option<Result<(), ProbeFailure>>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(match sway {
        Ok((path, answer)) => sway_check(path, answer),
        // The detail is the cause, not a fixed "not found": a present sway reported as missing
        // reads as a build that never ran.
        Err(no) => {
            Check::new("sway >=1.12", CheckStatus::Fail, no.cause.clone()).with_remedy(no.remedy)
        }
    });
    checks.push(if gl_present {
        Check::new(
            "software GL (Mesa)",
            CheckStatus::Ok,
            "libEGL + swrast DRI driver present",
        )
    } else {
        Check::new(
            "software GL (Mesa)",
            CheckStatus::Warn,
            "libEGL / swrast DRI driver not found",
        )
        .with_remedy("install Mesa software GL: `apt install libegl1 libgl1-mesa-dri`")
    });
    if let Some(res) = deep_spawn {
        checks.push(match res {
            Ok(()) => Check::new(
                "sway spawn (deep)",
                CheckStatus::Ok,
                "headless sway started and stopped",
            ),
            // The remedy comes from the failure itself, which withholds [`SWAY_START_HINT`] from
            // the outcomes that never reached sway (glass#373).
            Err(failure) => Check::new(
                "sway spawn (deep)",
                CheckStatus::Fail,
                failure.detail("headless sway"),
            )
            .with_remedy(failure.remedy(SWAY_START_HINT)),
        });
    }
    checks
}

/// Resolve sway (path) and ask it its version.
///
/// `resolve_sway_verdict` version-probes only its `PATH` walk, so a `GLASS_SWAY` override and the
/// bundled sway reach here having never been asked — doctor's is the first `--version` either is
/// put to.
fn discover_sway() -> Result<(PathBuf, VersionAnswer), NoSway> {
    let path = resolve_sway_verdict()?;
    let answer = ask_sway_version(&path, VERSION_PROBE_BUDGET);
    Ok((path, answer))
}

/// The `sway >=1.12` check for a sway that resolved.
///
/// A binary that gave no version is not a proven >=1.12, so it does not get a tick. `Warn` and not
/// `Fail` because resolution stands: an override and the bundle skip the version gate by design,
/// so glass will still try to launch it.
fn sway_check(path: &Path, answer: &VersionAnswer) -> Check {
    let at = path.display();
    match answer {
        VersionAnswer::Answered(v) if !v.trim().is_empty() => Check::new(
            "sway >=1.12",
            CheckStatus::Ok,
            format!("{} at {at}", v.trim()),
        ),
        VersionAnswer::Answered(_) => Check::new(
            "sway >=1.12",
            CheckStatus::Warn,
            format!("{at} ran but reported no version"),
        )
        .with_remedy(CHECK_THAT_SWAY),
        VersionAnswer::TimedOut(budget) => Check::new(
            "sway >=1.12",
            CheckStatus::Warn,
            format!("{at} did not answer `--version` within {budget:?}"),
        )
        .with_remedy(CHECK_THAT_SWAY),
        VersionAnswer::NoReply(why) => {
            Check::new("sway >=1.12", CheckStatus::Warn, format!("{at}: {why}"))
                .with_remedy(CHECK_THAT_SWAY)
        }
    }
}

/// Where a distro puts libEGL.
const EGL_SONAMES: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu/libEGL.so.1",
    "/usr/lib/libEGL.so.1",
    "/lib/x86_64-linux-gnu/libEGL.so.1",
    "/usr/lib64/libEGL.so.1",
];
/// Where a distro puts the Mesa DRI drivers.
const DRI_DIRS: &[&str] = &[
    "/usr/lib/x86_64-linux-gnu/dri",
    "/usr/lib/dri",
    "/usr/lib64/dri",
];

/// Heuristic check for the host Mesa software-GL stack the headless sway needs.
///
/// Both halves must be present — a loader cannot render without a driver, or a driver be reached
/// without the loader. Either swrast name counts: a host may carry only one.
fn gl_present_in(egl_sonames: &[&str], dri_dirs: &[&str]) -> bool {
    let egl = egl_sonames.iter().any(|p| Path::new(p).exists());
    let swrast = dri_dirs.iter().any(|d| {
        let d = Path::new(d);
        d.join("swrast_dri.so").exists() || d.join("kms_swrast_dri.so").exists()
    });
    egl && swrast
}

/// What to check when sway itself is what failed.
const SWAY_START_HINT: &str = "sway is present but produced no working compositor — check the \
     host Mesa software GL stack and sway's own dependencies";

/// How long the probe gives sway's IPC to appear — the give-up point, not a target: a working
/// host answers in well under a second.
const IPC_READY_BUDGET: Duration = Duration::from_secs(8);
/// How often the probe asks, between the two answers that end the wait early.
const IPC_POLL: Duration = Duration::from_millis(100);

/// Spawn a headless sway with a no-op client, confirm its IPC comes up, and tear the
/// process group down. Bounded so a wedged sway can't hang doctor.
fn probe_sway(sway: &Path) -> Result<(), ProbeFailure> {
    probe_sway_within(sway, IPC_READY_BUDGET)
}

/// [`probe_sway`] with its budget passed in — the seam a test can drive in milliseconds rather
/// than waiting out [`IPC_READY_BUDGET`].
fn probe_sway_within(sway: &Path, budget: Duration) -> Result<(), ProbeFailure> {
    let rt = tempfile::Builder::new()
        .prefix("glass-doctor-wl.")
        .tempdir()
        .map_err(|e| ProbeFailure::NotStarted(format!("private runtime dir: {e}")))?;
    let config = rt.path().join("sway.cfg");
    let spec = AppSpec {
        build: None,
        // Exits immediately, on purpose: the readiness loop breaks as soon as IPC answers, which
        // is before sway has finished `exec`ing its client, so a tree snapshot can miss one that
        // outlives it. A client already gone cannot be leaked by losing that race.
        run: vec!["true".into()],
        cwd: None,
        env: vec![],
        window_hint: None,
        timeout_ms: 5000,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    };
    std::fs::write(&config, sway_config(&spec, rt.path(), None))
        .map_err(|e| ProbeFailure::NotStarted(format!("sway config: {e}")))?;
    let mut child = build_sway_command(sway, &config, &spec, rt.path(), None)
        .spawn()
        .map_err(|e| ProbeFailure::Failed(format!("spawn sway: {e}")))?;

    // A deadline, not a poll count: the failure names the wait it made, and a count times
    // `IPC_POLL` is not that wait — each turn also spends a connect attempt (glass#373).
    let deadline = Instant::now() + budget;
    let outcome = loop {
        if let Some(status) = child.try_wait().ok().flatten() {
            // sway exited before its IPC came up — its own answer, and immediate: quoting the
            // budget below would claim a wait nobody made.
            break Err(ProbeFailure::Failed(format!(
                "sway exited before its IPC came up ({status})"
            )));
        }
        if Ipc::connect(rt.path()).is_ok() {
            break Ok(());
        }
        if Instant::now() >= deadline {
            break Err(ProbeFailure::TimedOut(budget));
        }
        std::thread::sleep(IPC_POLL);
    };

    // Snapshot the launch before any of it exits: once sway is reaped its descendants are
    // reparented to init and can no longer be found from its pid.
    //
    // A group signal is not enough: sway `setsid`s every app it `exec`s, so the client is in
    // neither its group nor its session and the signal reaches the compositor alone.
    let tree = glass_proc_linux::proc_tree_pids(child.id());
    glass_proc_linux::reap_launch(&mut child, &tree, glass_proc_linux::APP_REAP_GRACE);

    outcome
}

#[cfg(test)]
mod tests {
    use std::os::unix::fs::PermissionsExt;
    use std::time::Instant;

    use super::*;

    /// A resolved sway that answered, trimmed of the newline its `--version` ends with.
    #[test]
    fn sway_found_is_ok_with_version() {
        let cs = wayland_checks(&answered("sway version 1.12\n"), true, None);
        assert_eq!(cs[0].status, CheckStatus::Ok);
        assert_eq!(cs[0].detail, "sway version 1.12 at /usr/bin/sway");
    }

    fn answered(v: &str) -> Result<(PathBuf, VersionAnswer), NoSway> {
        Ok((
            PathBuf::from("/usr/bin/sway"),
            VersionAnswer::Answered(v.into()),
        ))
    }

    #[test]
    fn sway_missing_fails_with_remedy() {
        let cs = wayland_checks(
            &Err(NoSway {
                cause: "no sway >=1.12 found".into(),
                remedy: "build it with sway-build",
            }),
            true,
            None,
        );
        assert_eq!(cs[0].status, CheckStatus::Fail);
        assert_eq!(cs[0].detail, "no sway >=1.12 found");
        assert_eq!(cs[0].remedy.as_deref(), Some("build it with sway-build"));
    }

    /// The fixed "not found" detail sent the user to build a sway they had already built, at the
    /// path in front of them.
    #[test]
    fn a_sway_that_cannot_be_run_is_reported_as_such_not_as_missing() {
        let cs = wayland_checks(
            &Err(NoSway {
                cause: "/opt/glass/sway/bin/sway is not executable".into(),
                remedy: "chmod +x it",
            }),
            true,
            None,
        );
        assert_eq!(cs[0].status, CheckStatus::Fail);
        assert_eq!(cs[0].detail, "/opt/glass/sway/bin/sway is not executable");
        assert_eq!(cs[0].remedy.as_deref(), Some("chmod +x it"));
    }

    #[test]
    fn missing_gl_is_a_warning_with_remedy() {
        let cs = wayland_checks(&answered("1.12"), false, None);
        let gl = cs.iter().find(|c| c.name == "software GL (Mesa)").unwrap();
        assert_eq!(gl.status, CheckStatus::Warn);
        assert!(gl.remedy.as_deref().unwrap().contains("libgl1-mesa-dri"));
    }

    /// Everything below drives the real environment rather than the pure mapper — the gathering
    /// half is where doctor goes quietly wrong.
    ///
    /// A private tree per case, so the answer comes from the files named rather than this host's
    /// `/usr/lib`.
    fn gl_tree(egl: bool, dri: Option<&str>) -> (tempfile::TempDir, Vec<String>, Vec<String>) {
        let root = tempfile::tempdir().expect("tempdir");
        let so = root.path().join("libEGL.so.1");
        if egl {
            std::fs::write(&so, b"").expect("write libEGL");
        }
        let dir = root.path().join("dri");
        std::fs::create_dir(&dir).expect("mkdir dri");
        if let Some(name) = dri {
            std::fs::write(dir.join(name), b"").expect("write driver");
        }
        let egls = vec![so.to_string_lossy().into_owned()];
        let dris = vec![dir.to_string_lossy().into_owned()];
        (root, egls, dris)
    }

    fn gl_present_for(egl: bool, dri: Option<&str>) -> bool {
        let (_root, egls, dris) = gl_tree(egl, dri);
        let egls: Vec<&str> = egls.iter().map(String::as_str).collect();
        let dris: Vec<&str> = dris.iter().map(String::as_str).collect();
        gl_present_in(&egls, &dris)
    }

    #[test]
    fn gl_is_present_only_with_both_the_loader_and_a_driver() {
        assert!(gl_present_for(true, Some("swrast_dri.so")));
        assert!(!gl_present_for(true, None), "no driver");
        assert!(!gl_present_for(false, Some("swrast_dri.so")), "no libEGL");
        assert!(!gl_present_for(false, None));
    }

    #[test]
    fn either_swrast_driver_name_counts() {
        assert!(gl_present_for(true, Some("kms_swrast_dri.so")));
        assert!(!gl_present_for(true, Some("nouveau_dri.so")), "not swrast");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn discover_sway_reports_the_real_binary_and_its_version() {
        let (path, answer) = discover_sway().expect("this box has a discoverable sway");
        assert!(
            glass_exec_unix::is_executable_file(&path),
            "discovery must yield something glass can spawn: {}",
            path.display()
        );
        let VersionAnswer::Answered(ver) = &answer else {
            panic!("a real sway answers --version: {answer:?}");
        };
        assert!(ver.contains("sway version"), "{ver}");
    }

    /// glass#392: a binary that never answers used to hang doctor here, and now gets no tick —
    /// the check is named `sway >=1.12` and nothing established that.
    #[test]
    fn a_sway_that_never_answered_is_not_reported_as_a_working_one() {
        let cs = wayland_checks(
            &Ok((
                PathBuf::from("/opt/sway/bin/sway"),
                VersionAnswer::TimedOut(Duration::from_millis(1500)),
            )),
            true,
            None,
        );
        assert_eq!(cs[0].status, CheckStatus::Warn);
        assert!(cs[0].detail.contains("/opt/sway/bin/sway"), "{:?}", cs[0]);
        // The budget it actually waited, not whatever constant doctor happens to import.
        assert!(cs[0].detail.contains("1.5s"), "{:?}", cs[0]);
        assert_eq!(cs[0].remedy.as_deref(), Some(CHECK_THAT_SWAY));
    }

    /// Ran and said nothing, and could not be asked at all — neither is a proven >=1.12, and the
    /// second carries the runner's reason.
    #[test]
    fn a_sway_that_gave_no_version_is_not_reported_as_a_working_one() {
        let silent = wayland_checks(&answered(" \n"), true, None);
        assert_eq!(silent[0].status, CheckStatus::Warn);
        assert!(
            silent[0].detail.contains("reported no version"),
            "{:?}",
            silent[0]
        );

        let cs = wayland_checks(
            &Ok((
                PathBuf::from("/usr/bin/sway"),
                VersionAnswer::NoReply("failed to start: Exec format error".into()),
            )),
            true,
            None,
        );
        assert_eq!(cs[0].status, CheckStatus::Warn);
        assert!(cs[0].detail.contains("Exec format error"), "{:?}", cs[0]);
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_shallow_checks_cover_sway_and_gl_and_stop_there() {
        let cs = checks(false);
        let names: Vec<&str> = cs.iter().map(|c| c.name.as_str()).collect();
        assert!(names.contains(&"sway >=1.12"), "{names:?}");
        assert!(names.contains(&"software GL (Mesa)"), "{names:?}");
        assert!(
            !names.contains(&"sway spawn (deep)"),
            "the deep probe must not run unasked: {names:?}"
        );
    }

    /// A `deep: true` that quietly skipped the spawn reports the same clean bill of health as one
    /// that ran it.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn the_deep_check_really_starts_and_stops_a_compositor() {
        let before = compositors_running();
        let deep = checks(true)
            .into_iter()
            .find(|c| c.name == "sway spawn (deep)")
            .expect("deep asks for the spawn check");
        assert_eq!(deep.status, CheckStatus::Ok, "{}", deep.detail);
        // The detail claims "started and stopped" and nothing else asserts the second half: a
        // probe that leaks its compositor still answers Ok, and each survivor holds a display.
        assert_eq!(
            compositors_running(),
            before,
            "the deep probe left a compositor behind"
        );
    }

    /// How many of the deep probe's own compositors are running, matched on its private
    /// runtime-dir prefix so another test's session or a real sway is never counted.
    fn compositors_running() -> usize {
        let out = std::process::Command::new("ps")
            .args(["-eo", "args"])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("glass-doctor-wl."))
            .count()
    }

    /// glass#373: `/bin/false` is gone before the first poll, and this reported "did not come up
    /// within ~8s" — an eight-second wait that took none. Needs no sway.
    #[test]
    fn a_compositor_that_exited_is_not_reported_as_one_that_never_answered() {
        let err = probe_sway(Path::new("/bin/false")).expect_err("no IPC ever appears");
        let ProbeFailure::Failed(why) = &err else {
            panic!("an exit is the compositor's own answer, not a wait: {err:?}");
        };
        assert!(why.contains("exited"), "{why}");
        assert!(
            !why.contains("8s"),
            "no budget elapsed, so none may be quoted: {why}"
        );
    }

    /// The other end of the same wait: a "compositor" that starts, stays up and never opens an
    /// IPC socket. Its budget is the one that elapsed, and it must not outlive the probe.
    #[test]
    fn a_compositor_that_never_answers_times_out_and_is_taken_down() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("sway");
        let pidfile = dir.path().join("pid");
        // Rejects an argv that is not the one glass builds, so a probe that stopped passing
        // sway's arguments cannot pass this test by accident.
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\ncase \"$*\" in *--unsupported-gpu*) ;; *) exit 64;; esac\n\
                 echo $$ > {}\nexec sleep 30\n",
                pidfile.display()
            ),
        )
        .expect("write fake sway");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake sway");

        let budget = Duration::from_millis(200);
        let started = Instant::now();
        let err = probe_sway_within(&fake, budget).expect_err("no IPC ever appears");
        assert_eq!(err, ProbeFailure::TimedOut(budget));
        assert!(
            started.elapsed() < budget * 10,
            "the wait must be the budget it reports: {:?}",
            started.elapsed()
        );
        let pid: u32 = std::fs::read_to_string(&pidfile)
            .expect("the fixture records its pid before it sleeps")
            .trim()
            .parse()
            .expect("a pid");
        assert!(
            !glass_proc_linux::any_alive(&[pid]),
            "the probe left its compositor ({pid}) running"
        );
    }

    #[test]
    fn deep_spawn_failure_is_reported() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(Err(ProbeFailure::Failed("no come up".into()))),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail);
        assert!(deep.detail.contains("no come up"), "{:?}", deep.detail);
        assert!(
            deep.remedy.as_deref().is_some_and(|r| r.contains("Mesa")),
            "a probe that reached sway gets sway's advice: {:?}",
            deep.remedy
        );
    }

    /// glass#373 in this backend's terms: a probe the host refused or killed says nothing about
    /// sway, and Mesa is the wrong lead for a pids limit.
    #[test]
    fn a_probe_the_host_stopped_does_not_point_at_the_compositor() {
        for failure in [
            ProbeFailure::NotStarted("Resource temporarily unavailable".into()),
            ProbeFailure::Vanished,
        ] {
            let cs = wayland_checks(&answered("1.12"), true, Some(Err(failure)));
            let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
            assert_eq!(deep.status, CheckStatus::Fail);
            assert!(
                deep.remedy
                    .as_deref()
                    .is_some_and(|r| r.contains("limits") && !r.contains("Mesa")),
                "{deep:?}"
            );
        }
    }
}
