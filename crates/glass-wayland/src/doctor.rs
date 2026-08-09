//! Environment checks for the Wayland backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`wayland_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without sway.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glass_core::{AppSpec, Check, CheckStatus};

use crate::command::{build_sway_command, sway_config};
use crate::platform::{NoSway, resolve_sway_verdict};
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
    sway: &Result<(PathBuf, String), NoSway>,
    gl_present: bool,
    deep_spawn: Option<Result<(), String>>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(match sway {
        Ok((path, ver)) => Check::new(
            "sway >=1.12",
            CheckStatus::Ok,
            format!("{ver} at {}", path.display()),
        ),
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
            Err(e) => Check::new("sway spawn (deep)", CheckStatus::Fail, e).with_remedy(
                "sway is present but failed to start headless — check Mesa software GL",
            ),
        });
    }
    checks
}

/// Resolve sway (path) and read its version string for display.
fn discover_sway() -> Result<(PathBuf, String), NoSway> {
    let path = resolve_sway_verdict()?;
    let ver = sway_version(&path);
    Ok((path, ver))
}

fn sway_version(path: &Path) -> String {
    std::process::Command::new(path)
        .arg("--version")
        .output()
        .ok()
        .map(|o| String::from_utf8_lossy(&o.stdout).trim().to_string())
        .filter(|s| !s.is_empty())
        .unwrap_or_else(|| "sway (version unknown)".into())
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

/// Spawn a headless sway with a no-op client, confirm its IPC comes up, and tear the
/// process group down. Bounded so a wedged sway can't hang doctor.
fn probe_sway(sway: &Path) -> Result<(), String> {
    let rt = tempfile::Builder::new()
        .prefix("glass-doctor-wl.")
        .tempdir()
        .map_err(|e| e.to_string())?;
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
    std::fs::write(&config, sway_config(&spec, rt.path(), None)).map_err(|e| e.to_string())?;
    let mut child = build_sway_command(sway, &config, &spec, rt.path(), None)
        .spawn()
        .map_err(|e| format!("spawn sway: {e}"))?;

    let mut up = false;
    for _ in 0..80 {
        if child.try_wait().ok().flatten().is_some() {
            break; // sway exited before its IPC came up
        }
        if Ipc::connect(rt.path()).is_ok() {
            up = true;
            break;
        }
        std::thread::sleep(Duration::from_millis(100));
    }

    // Snapshot the launch before any of it exits: once sway is reaped its descendants are
    // reparented to init and can no longer be found from its pid.
    //
    // A group signal is not enough: sway `setsid`s every app it `exec`s, so the client is in
    // neither its group nor its session and the signal reaches the compositor alone.
    let tree = glass_proc_linux::proc_tree_pids(child.id());
    glass_proc_linux::reap_launch(&mut child, &tree, glass_proc_linux::APP_REAP_GRACE);

    if up {
        Ok(())
    } else {
        Err("headless sway did not come up within ~8s".into())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sway_found_is_ok_with_version() {
        let cs = wayland_checks(
            &Ok((PathBuf::from("/usr/bin/sway"), "sway version 1.12".into())),
            true,
            None,
        );
        assert_eq!(cs[0].status, CheckStatus::Ok);
        assert!(cs[0].detail.contains("1.12"));
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
        let cs = wayland_checks(&Ok((PathBuf::from("/x"), "1.12".into())), false, None);
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
        let (path, ver) = discover_sway().expect("this box has a discoverable sway");
        assert!(
            glass_exec_unix::is_executable_file(&path),
            "discovery must yield something glass can spawn: {}",
            path.display()
        );
        assert!(ver.contains("sway version"), "{ver}");
    }

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn sway_version_reads_the_binarys_own_output() {
        let (path, _) = discover_sway().expect("sway");
        assert!(sway_version(&path).contains("sway version"));
    }

    /// A binary that prints nothing is not a version, and doctor's job is to say what it found.
    ///
    /// Not `/bin/true`: GNU coreutils answers `--version` with its own version string.
    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn a_silent_binary_reports_an_unknown_version_not_an_empty_one() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join("mute");
        std::fs::write(&bin, b"#!/bin/sh\nexit 0\n").expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
        assert_eq!(sway_version(&bin), "sway (version unknown)");
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

    #[test]
    #[ignore = "starts a real compositor or X server; needs sway, Mesa, Xwayland or Xvfb"]
    fn probing_something_that_is_not_a_compositor_fails() {
        let err = probe_sway(Path::new("/bin/false")).expect_err("no IPC ever appears");
        assert!(err.contains("did not come up"), "{err}");
    }

    #[test]
    fn deep_spawn_failure_is_reported() {
        let cs = wayland_checks(
            &Ok((PathBuf::from("/x"), "1.12".into())),
            true,
            Some(Err("no come up".into())),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail);
        assert_eq!(deep.detail, "no come up");
    }
}
