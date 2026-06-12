//! Environment checks for the Wayland backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`wayland_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without sway.

use std::path::{Path, PathBuf};
use std::time::Duration;

use glass_core::{AppSpec, Check, CheckStatus};
use rustix::process::{kill_process_group, Pid, Signal};

use crate::command::{build_sway_command, sway_config};
use crate::platform::resolve_sway;
use crate::swayipc::Ipc;

/// Probe the Wayland backend's environment. `deep` additionally spawns and tears down
/// a headless sway to prove it actually starts.
pub fn checks(deep: bool) -> Vec<Check> {
    let sway = discover_sway();
    let gl = gl_present();
    let deep_spawn = match (deep, &sway) {
        (true, Ok((path, _))) => Some(probe_sway(path)),
        _ => None,
    };
    wayland_checks(&sway, gl, deep_spawn)
}

/// Pure: build the Wayland checks from gathered facts.
fn wayland_checks(
    sway: &Result<(PathBuf, String), String>,
    gl_present: bool,
    deep_spawn: Option<Result<(), String>>,
) -> Vec<Check> {
    let mut checks = Vec::new();
    checks.push(match sway {
        Ok((path, ver)) => {
            Check::new("sway >=1.12", CheckStatus::Ok, format!("{ver} at {}", path.display()))
        }
        Err(remedy) => {
            Check::new("sway >=1.12", CheckStatus::Fail, "not found").with_remedy(remedy.clone())
        }
    });
    checks.push(if gl_present {
        Check::new("software GL (Mesa)", CheckStatus::Ok, "libEGL + swrast DRI driver present")
    } else {
        Check::new("software GL (Mesa)", CheckStatus::Warn, "libEGL / swrast DRI driver not found")
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
fn discover_sway() -> Result<(PathBuf, String), String> {
    match resolve_sway() {
        Ok(path) => {
            let ver = sway_version(&path);
            Ok((path, ver))
        }
        Err(e) => Err(e.to_string()),
    }
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

/// Heuristic check for the host Mesa software-GL stack the headless sway needs.
fn gl_present() -> bool {
    let egl = [
        "/usr/lib/x86_64-linux-gnu/libEGL.so.1",
        "/usr/lib/libEGL.so.1",
        "/lib/x86_64-linux-gnu/libEGL.so.1",
        "/usr/lib64/libEGL.so.1",
    ]
    .iter()
    .any(|p| Path::new(p).exists());
    let swrast = ["/usr/lib/x86_64-linux-gnu/dri", "/usr/lib/dri", "/usr/lib64/dri"]
        .iter()
        .any(|d| {
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
        run: vec!["sleep".into(), "3600".into()],
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

    // Tear down the whole compositor group (sway is its own group leader).
    if let Some(pgid) = Pid::from_raw(child.id() as i32) {
        let _ = kill_process_group(pgid, Signal::TERM);
    }
    let _ = child.wait();

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
        let cs = wayland_checks(&Err("build it with sway-build".into()), true, None);
        assert_eq!(cs[0].status, CheckStatus::Fail);
        assert_eq!(cs[0].remedy.as_deref(), Some("build it with sway-build"));
    }

    #[test]
    fn missing_gl_is_a_warning_with_remedy() {
        let cs = wayland_checks(&Ok((PathBuf::from("/x"), "1.12".into())), false, None);
        let gl = cs.iter().find(|c| c.name == "software GL (Mesa)").unwrap();
        assert_eq!(gl.status, CheckStatus::Warn);
        assert!(gl.remedy.as_deref().unwrap().contains("libgl1-mesa-dri"));
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
