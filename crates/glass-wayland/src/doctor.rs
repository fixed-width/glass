//! Environment checks for the Wayland backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`wayland_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without sway.

use std::io::Read as _;
use std::path::{Path, PathBuf};
use std::process::{ChildStderr, Stdio};
use std::sync::mpsc;
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

/// What the deep probe learned: whether the compositor came up, and what its teardown left
/// running.
///
/// Two answers rather than one. A compositor that came up and was not torn down is a working sway
/// and a leaked process at once, and the check that reports only the first says "started and
/// stopped" of a launch that did neither half it claims (glass#380).
struct SwaySpawn {
    /// Whether sway's IPC answered inside the probe's budget.
    came_up: Result<(), ProbeFailure>,
    /// What the last IPC connect attempt said — the difference between a socket that never
    /// appeared and one that is there and refusing, which a timeout alone does not carry.
    last_ipc_error: Option<String>,
    /// What sway wrote to stderr, captured rather than inherited: on the failure paths it is the
    /// compositor's own account of what went wrong.
    said: Option<String>,
    /// Pids of the probe's own launch still running once its teardown was done.
    left_behind: Vec<u32>,
    /// The probe's private runtime dir, kept rather than deleted — `Some` only when something is
    /// still running out of it, because deleting it takes the survivors' sockets with it.
    kept_runtime_dir: Option<PathBuf>,
}

/// Pure: build the Wayland checks from gathered facts.
fn wayland_checks(
    sway: &Result<(PathBuf, VersionAnswer), NoSway>,
    gl_present: bool,
    deep_spawn: Option<SwaySpawn>,
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
        checks.push(deep_spawn_check(res));
    }
    checks
}

/// The `sway spawn (deep)` check: whether the compositor came up, and whether the probe's own
/// teardown reached everything it started.
fn deep_spawn_check(spawn: SwaySpawn) -> Check {
    const NAME: &str = "sway spawn (deep)";
    let left = survivors(&spawn.left_behind, spawn.kept_runtime_dir.as_deref());
    match (spawn.came_up, left) {
        (Ok(()), None) => Check::new(NAME, CheckStatus::Ok, "headless sway started and stopped"),
        // The start half stands, so the check keeps it and warns about the half that does not.
        (Ok(()), Some(left)) => Check::new(
            NAME,
            CheckStatus::Warn,
            format!("headless sway started, but {}", left.detail),
        )
        .with_remedy(left.remedy),
        // The remedy comes from the failure itself, which withholds `SWAY_START_HINT` from
        // the outcomes that never reached sway (glass#373).
        (Err(failure), left) => {
            let mut detail = failure.detail("headless sway");
            // Only on the timeout: after an exit, or a wait glass could not make, the last connect
            // error is a socket missing for a reason the detail already gives.
            if let (ProbeFailure::TimedOut(_), Some(why)) = (&failure, &spawn.last_ipc_error) {
                detail.push_str(&format!(" — its IPC socket last said: {why}"));
            }
            if let Some(said) = &spawn.said {
                detail.push_str(&format!(" — sway said: {said}"));
            }
            let mut remedy = failure.remedy(SWAY_START_HINT);
            if let Some(left) = left {
                detail.push_str(&format!(" — and {}", left.detail));
                remedy.push_str(&format!(". {}", left.remedy));
            }
            Check::new(NAME, CheckStatus::Fail, detail).with_remedy(remedy)
        }
    }
}

/// What the check says about processes the probe's teardown did not reach.
struct Survivors {
    detail: String,
    remedy: String,
}

/// Describe `left_behind`, or `None` when the teardown reached everything.
fn survivors(left_behind: &[u32], kept_runtime_dir: Option<&Path>) -> Option<Survivors> {
    let (first, rest) = left_behind.split_first()?;
    let list = |sep: &str| {
        std::iter::once(first.to_string())
            .chain(rest.iter().map(u32::to_string))
            .collect::<Vec<_>>()
            .join(sep)
    };
    let (count, pids, whose) = match rest.len() {
        0 => ("1 process".to_string(), format!("pid {first}"), "its"),
        n => (
            format!("{} processes", n + 1),
            format!("pids {}", list(", ")),
            "their",
        ),
    };
    let dir = kept_runtime_dir
        .map(|d| format!("; {whose} runtime dir is kept at {}", d.display()))
        .unwrap_or_default();
    Some(Survivors {
        detail: format!("{count} of its launch outlived the probe's SIGKILL ({pids}){dir}"),
        remedy: format!(
            "kill {} by hand{}; a process that outlives SIGKILL is usually stuck in the kernel \
             (uninterruptible I/O), which only its device or a reboot clears",
            list(" "),
            kept_runtime_dir
                .map(|d| format!(", then remove {}", d.display()))
                .unwrap_or_default()
        ),
    })
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

/// What to check when sway itself is what failed — reached only by the outcomes that got as far
/// as running it.
const SWAY_START_HINT: &str = "sway is present but produced no working compositor — check the \
     host Mesa software GL stack and sway's own dependencies";

/// How long the probe gives sway's IPC to appear — the give-up point, not a target: a working
/// host answers in well under a second.
const IPC_READY_BUDGET: Duration = Duration::from_secs(8);
/// How often the probe asks, between the two answers that end the wait early.
const IPC_POLL: Duration = Duration::from_millis(100);

/// Spawn a headless sway with a no-op client, confirm its IPC comes up, and tear the
/// process group down. Bounded so a wedged sway can't hang doctor.
fn probe_sway(sway: &Path) -> SwaySpawn {
    probe_sway_within(sway, IPC_READY_BUDGET)
}

/// How much of sway's stderr the probe keeps. What explains a failed start is at the front, and
/// the whole of it is rendered into a check, which is no place for a compositor's log.
const STDERR_KEPT: u64 = 2 * 1024;

/// How long the probe waits for the stderr pipe to end after the compositor has been reaped.
const SAID_GRACE: Duration = Duration::from_millis(200);

/// Sway's own stderr, read on a helper thread for as long as the pipe is open.
///
/// A thread rather than a read at the end, because a compositor whose stderr nobody drains stalls
/// on a full pipe — which the probe would then report as a compositor that never came up.
struct SwaySaid(mpsc::Receiver<String>);

impl SwaySaid {
    /// Start reading. `Err` is the host refusing the thread: `Builder`, where `thread::spawn`
    /// panics (glass#454).
    fn drain(mut stderr: ChildStderr) -> std::io::Result<SwaySaid> {
        let (tx, rx) = mpsc::channel();
        std::thread::Builder::new()
            .name("glass-doctor-sway-stderr".into())
            .spawn(move || {
                let mut kept = Vec::new();
                let _ = stderr.by_ref().take(STDERR_KEPT).read_to_end(&mut kept);
                // Past the cap the pipe is still drained, so sway is never blocked writing to it.
                let _ = std::io::copy(&mut stderr, &mut std::io::sink());
                // One line: a check's detail is a line, and a compositor's stderr is not.
                let text = String::from_utf8_lossy(&kept);
                let said = text
                    .lines()
                    .map(str::trim)
                    .filter(|line| !line.is_empty())
                    .collect::<Vec<_>>()
                    .join("; ");
                let _ = tx.send(said);
            })?;
        Ok(SwaySaid(rx))
    }

    /// What it said, once its pipe has ended. `None` when it said nothing, and when the pipe is
    /// still open — a survivor of the teardown holds it, and that survivor is what gets reported.
    fn snapshot(self, grace: Duration) -> Option<String> {
        self.0.recv_timeout(grace).ok().filter(|s| !s.is_empty())
    }
}

/// What ended a wait for sway's IPC, and the last thing a connect attempt said.
struct IpcWait {
    outcome: Result<(), ProbeFailure>,
    last_ipc_error: Option<String>,
}

/// Wait for `connect` to answer, giving up at `budget`, and stopping early when `exited` reports
/// the compositor gone.
///
/// A deadline, not a poll count: the failure names the wait it made, and a count times `poll` is
/// not that wait — each turn also spends a connect attempt (glass#373).
///
/// Both halves are closures because the classification is what this got wrong, and driving it
/// takes no compositor.
fn await_ipc(
    budget: Duration,
    poll: Duration,
    mut exited: impl FnMut() -> std::io::Result<Option<std::process::ExitStatus>>,
    mut connect: impl FnMut() -> Result<(), String>,
) -> IpcWait {
    let deadline = Instant::now() + budget;
    let mut last_ipc_error = None;
    let outcome = loop {
        match exited() {
            // sway exited before its IPC came up — its own answer, and immediate: quoting the
            // budget below would claim a wait nobody made.
            Ok(Some(status)) => {
                break Err(ProbeFailure::Failed(format!(
                    "sway exited before its IPC came up ({status})"
                )));
            }
            // Whether sway is running can no longer be established — something else reaped it, so
            // `waitpid` answers ECHILD. Dropped, this read as "still running" and the wait went on
            // to spend its whole budget and report a timeout.
            Err(e) => {
                break Err(ProbeFailure::Failed(format!(
                    "glass could not wait on sway, so its exit could not be observed: {e}"
                )));
            }
            Ok(None) => {}
        }
        match connect() {
            Ok(()) => break Ok(()),
            Err(e) => last_ipc_error = Some(e),
        }
        if Instant::now() >= deadline {
            break Err(ProbeFailure::TimedOut(budget));
        }
        std::thread::sleep(poll);
    };
    IpcWait {
        outcome,
        last_ipc_error,
    }
}

/// A probe that ended before it spawned anything: nothing ran, so nothing was left behind.
fn never_ran(why: ProbeFailure) -> SwaySpawn {
    SwaySpawn {
        came_up: Err(why),
        last_ipc_error: None,
        said: None,
        left_behind: Vec::new(),
        kept_runtime_dir: None,
    }
}

/// [`probe_sway`] with its budget passed in — the seam a test can drive in milliseconds rather
/// than waiting out [`IPC_READY_BUDGET`].
fn probe_sway_within(sway: &Path, budget: Duration) -> SwaySpawn {
    let rt = match tempfile::Builder::new()
        .prefix("glass-doctor-wl.")
        .tempdir()
    {
        Ok(rt) => rt,
        Err(e) => {
            return never_ran(ProbeFailure::NotStarted(format!(
                "private runtime dir: {e}"
            )));
        }
    };
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
    if let Err(e) = std::fs::write(&config, sway_config(&spec, rt.path(), None)) {
        return never_ran(ProbeFailure::NotStarted(format!("sway config: {e}")));
    }
    // `NotStarted`, not `Failed`: resolution already proved this path executable, so what is left
    // is the host refusing a process (EAGAIN under a `pids` limit, ENOMEM), which sway's own
    // advice does not fit (glass#373).
    let mut child = match build_sway_command(sway, &config, &spec, rt.path(), None)
        .stderr(Stdio::piped())
        .spawn()
    {
        Ok(child) => child,
        Err(e) => return never_ran(ProbeFailure::NotStarted(format!("spawn sway: {e}"))),
    };
    let said = match child.stderr.take().map(SwaySaid::drain) {
        Some(Ok(said)) => Some(said),
        // A refused thread leaves sway's stderr undrained, so it is reaped rather than run blind.
        Some(Err(e)) => {
            let tree = glass_proc_linux::proc_tree_pids(child.id());
            let _ =
                glass_proc_linux::reap_launch(&mut child, &tree, glass_proc_linux::APP_REAP_GRACE);
            return never_ran(ProbeFailure::NotStarted(format!("sway stderr reader: {e}")));
        }
        None => None,
    };

    let wait = await_ipc(
        budget,
        IPC_POLL,
        || child.try_wait(),
        || {
            Ipc::connect(rt.path()).map(|_| ()).map_err(|e| match e {
                // Stripped of `GlassError::Backend`'s Display prefix, which the check would read
                // back as "its IPC socket last said: backend error: …".
                glass_core::GlassError::Backend(why) => why,
                other => other.to_string(),
            })
        },
    );
    // Snapshot the launch before any of it exits: once sway is reaped its descendants are
    // reparented to init and can no longer be found from its pid.
    //
    // A group signal is not enough: sway `setsid`s every app it `exec`s, so the client is in
    // neither its group nor its session and the signal reaches the compositor alone.
    let tree = glass_proc_linux::proc_tree_pids(child.id());
    let left_behind =
        glass_proc_linux::reap_launch(&mut child, &tree, glass_proc_linux::APP_REAP_GRACE);
    // Deleting the runtime dir out from under a process still running in it takes its sockets
    // with it, which is how a leak becomes one nothing can reach or place.
    let kept_runtime_dir = (!left_behind.is_empty()).then(|| rt.keep());

    SwaySpawn {
        came_up: wait.outcome,
        last_ipc_error: wait.last_ipc_error,
        said: said.and_then(|said| said.snapshot(SAID_GRACE)),
        left_behind,
        kept_runtime_dir,
    }
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
        // `Ok` is now the stop as well as the start: a leaked compositor warns (glass#380), so
        // this fails on a survivor even before the count below.
        assert_eq!(deep.status, CheckStatus::Ok, "{}", deep.detail);
        // Counted from outside glass as well, so the verdict is not the only witness to it.
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
        let err = probe_sway(Path::new("/bin/false"))
            .came_up
            .expect_err("no IPC ever appears");
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

        // Long enough that a loaded CI box has written the pidfile below before the probe gives
        // up on it; short enough that the test is not the slow one in the suite.
        let budget = Duration::from_millis(500);
        let started = Instant::now();
        let err = probe_sway_within(&fake, budget)
            .came_up
            .expect_err("no IPC ever appears");
        assert_eq!(err, ProbeFailure::TimedOut(budget));
        // The budget plus the teardown that follows it — the units this actually spans. A wait
        // that ignored its argument would blow this by `IPC_READY_BUDGET`.
        let ceiling = budget * 2 + glass_proc_linux::APP_REAP_GRACE * 2;
        assert!(
            started.elapsed() < ceiling,
            "the wait must be the budget it reports: {:?} against {ceiling:?}",
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

    /// glass#380: sway's stderr was inherited, so what it said about its own failure went to
    /// glass's stderr and never to the check — the only surface an MCP client reads.
    #[test]
    fn a_compositor_that_failed_says_why_in_the_check() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = dir.path().join("sway");
        // Rejects an argv that is not the one glass builds, so a probe that stopped passing
        // sway's arguments cannot pass this test by accident.
        std::fs::write(
            &fake,
            "#!/bin/sh\ncase \"$*\" in *--unsupported-gpu*) ;; *) exit 64;; esac\n\
             echo 'sway: could not create GLES2 renderer' >&2\nexit 1\n",
        )
        .expect("write fake sway");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake sway");

        let spawn = probe_sway_within(&fake, Duration::from_millis(500));
        let cs = wayland_checks(&answered("1.12"), true, Some(spawn));
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail, "{deep:?}");
        assert!(
            deep.detail.contains("could not create GLES2 renderer"),
            "{deep:?}"
        );
    }

    /// glass#380: `try_wait().ok()` dropped the error, which the loop then read as "still
    /// running" — so a wait glass could not make spent the whole budget and was reported as a
    /// compositor that took too long.
    #[test]
    fn a_wait_that_could_not_be_made_is_not_reported_as_a_slow_compositor() {
        let budget = Duration::from_secs(2);
        let started = Instant::now();
        let wait = await_ipc(
            budget,
            Duration::from_millis(5),
            || Err(std::io::Error::other("No child processes")),
            || Err("no socket".into()),
        );
        let Err(ProbeFailure::Failed(why)) = &wait.outcome else {
            panic!(
                "a question glass could not ask is not sway's answer: {wait:?}",
                wait = wait.outcome
            );
        };
        assert!(why.contains("No child processes"), "{why}");
        assert!(
            started.elapsed() < budget / 2,
            "the wait ended on the error, so none of the budget is spent: {:?}",
            started.elapsed()
        );
    }

    /// The other dropped error: `Ipc::connect(..).is_ok()` threw away what the connection said,
    /// so a socket that exists and refuses read exactly like one that never appeared.
    #[test]
    fn a_timeout_carries_the_last_thing_the_ipc_connect_said() {
        let budget = Duration::from_millis(30);
        let wait = await_ipc(
            budget,
            Duration::from_millis(5),
            || Ok(None),
            || Err("Connection refused (os error 111)".into()),
        );
        assert_eq!(wait.outcome, Err(ProbeFailure::TimedOut(budget)));
        assert_eq!(
            wait.last_ipc_error.as_deref(),
            Some("Connection refused (os error 111)")
        );
    }

    /// And it has to reach the check, which is the only surface an MCP client reads.
    #[test]
    fn the_check_shows_what_the_ipc_connect_said() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: Err(ProbeFailure::TimedOut(Duration::from_secs(8))),
                last_ipc_error: Some("Connection refused (os error 111)".into()),
                said: None,
                left_behind: Vec::new(),
                kept_runtime_dir: None,
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert!(deep.detail.contains("Connection refused"), "{deep:?}");
    }

    /// A probe outcome with nothing left running — the shape every case but the leak tests.
    fn spawned(came_up: Result<(), ProbeFailure>) -> SwaySpawn {
        SwaySpawn {
            came_up,
            last_ipc_error: None,
            said: None,
            left_behind: Vec::new(),
            kept_runtime_dir: None,
        }
    }

    /// glass#380: the detail said "started and stopped" on the strength of the start alone, so a
    /// probe that leaked its compositor reported the same green line as one that tore it down.
    #[test]
    fn a_probe_that_left_its_compositor_running_is_not_reported_as_stopped() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: Ok(()),
                last_ipc_error: None,
                said: None,
                left_behind: vec![4242, 4243],
                kept_runtime_dir: Some(PathBuf::from("/tmp/glass-doctor-wl.abc123")),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Warn, "{deep:?}");
        // An operator can only kill what is named.
        assert!(deep.detail.contains("4242"), "{deep:?}");
        assert!(deep.detail.contains("4243"), "{deep:?}");
        assert!(deep.remedy.is_some(), "{deep:?}");
    }

    /// The probe keeps the runtime dir when something is still using it, so the check has to say
    /// where it is — otherwise the operator is left a directory they cannot find and sockets whose
    /// processes they cannot place.
    #[test]
    fn a_kept_runtime_dir_is_named_so_it_can_be_cleaned_up() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: Ok(()),
                last_ipc_error: None,
                said: None,
                left_behind: vec![4242],
                kept_runtime_dir: Some(PathBuf::from("/tmp/glass-doctor-wl.abc123")),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        let said = format!("{} {}", deep.detail, deep.remedy.as_deref().unwrap_or(""));
        assert!(said.contains("/tmp/glass-doctor-wl.abc123"), "{deep:?}");
        // Counted, so a single survivor is not reported in the plural.
        assert!(
            deep.detail.contains("1 process ") && deep.detail.contains("pid 4242"),
            "{deep:?}"
        );
    }

    /// A failed probe that also leaked is worse than one that only failed, so the survivors ride
    /// on the failure rather than replacing it.
    #[test]
    fn survivors_are_named_on_the_failure_path_too() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: Err(ProbeFailure::TimedOut(Duration::from_secs(8))),
                last_ipc_error: None,
                said: None,
                left_behind: vec![4242],
                kept_runtime_dir: Some(PathBuf::from("/tmp/glass-doctor-wl.abc123")),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail, "{deep:?}");
        assert!(deep.detail.contains("8s"), "the failure survives: {deep:?}");
        assert!(deep.detail.contains("4242"), "{deep:?}");
    }

    #[test]
    fn deep_spawn_failure_is_reported() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(spawned(Err(ProbeFailure::Failed("no come up".into())))),
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

    /// glass#373 in this backend's terms: a probe that never started says nothing about sway, so
    /// Mesa is the wrong lead — for a `pids` limit that refused the fork, or for a temp dir the
    /// probe could not write.
    #[test]
    fn a_probe_that_never_started_does_not_point_at_the_compositor() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(spawned(Err(ProbeFailure::NotStarted(
                "private runtime dir: No space left on device".into(),
            )))),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail);
        assert!(
            deep.detail.contains("No space left on device"),
            "the cause is the only text that names the resource: {deep:?}"
        );
        assert!(
            deep.remedy.as_deref().is_some_and(|r| !r.contains("Mesa")),
            "{deep:?}"
        );
    }

    /// A spawn that never happened is not sway failing to render, and the classification is what
    /// decides which remedy the operator reads.
    #[test]
    fn a_spawn_that_never_happened_is_not_reported_as_sway_failing() {
        let missing = std::path::Path::new("/nonexistent/glass-doctor/sway");
        let err = probe_sway(missing).came_up.expect_err("nothing to spawn");
        assert!(
            matches!(err, ProbeFailure::NotStarted(_)),
            "a spawn that never happened is not sway's answer: {err:?}"
        );
        assert!(
            !err.remedy(SWAY_START_HINT).contains("Mesa"),
            "{:?}",
            err.remedy(SWAY_START_HINT)
        );
    }
}
