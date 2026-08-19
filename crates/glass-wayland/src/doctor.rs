//! Environment checks for the Wayland backend ("glass doctor").
//!
//! [`checks`] gathers the real environment; the pure [`wayland_checks`] maps gathered
//! facts to [`Check`]s and is unit-tested without sway.

use std::path::{Path, PathBuf};
use std::process::{Child, Stdio};
use std::time::{Duration, Instant};

use glass_core::{AppSpec, Check, CheckStatus, ProbeFailure};
use glass_proc_linux::StderrTail;

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

/// What the deep probe learned: whether the compositor came up, and what it left running.
///
/// A compositor that came up and was not torn down is a working sway and a leaked process at
/// once; reporting only the first is what claimed a stop nobody had checked (glass#380).
#[derive(Debug)]
struct SwaySpawn {
    came_up: CameUp,
    /// What the last IPC connect attempt was told.
    last_ipc: Option<String>,
    /// What sway wrote to stderr, captured rather than inherited — its own account of a failure.
    said: Option<String>,
    /// What the probe started and did not manage to stop.
    leaked: Option<Leaked>,
}

/// Whether sway's IPC answered inside the probe's budget.
#[derive(Debug)]
enum CameUp {
    Yes,
    /// It did not, for a reason the shared vocabulary names.
    No(ProbeFailure),
    /// Neither: the probe ran and glass lost track of what it started, so nothing here is about
    /// sway (glass#373).
    Unknown(String),
}

/// What the probe started and did not stop. Non-empty by construction — the absence is `None`.
#[derive(Debug)]
struct Leaked {
    /// Processes of the probe's own session still running.
    pids: Vec<u32>,
    /// The probe's runtime dir, kept because those processes still have sockets in it.
    runtime_dir: PathBuf,
}

impl Leaked {
    /// `rt` is consumed either way: deleted when nothing survived, kept when something did,
    /// because deleting it would take that process's sockets with it.
    fn after_reap(pids: Vec<u32>, rt: tempfile::TempDir) -> Option<Leaked> {
        (!pids.is_empty()).then(|| Leaked {
            pids,
            runtime_dir: rt.keep(),
        })
    }
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
    if let Some(spawn) = deep_spawn {
        checks.push(deep_spawn_check(spawn));
    }
    checks
}

/// What glass can do about a probe whose own machinery failed — never the backend's advice, which
/// nothing here established (glass#373).
const NOTHING_ABOUT_SWAY: &str = "the compositor was started and then lost track of, so nothing \
     here is about sway or the host's GL stack. Re-run `glass doctor --deep`; a host out of \
     threads (a low `pids` cgroup limit) or file descriptors does this repeatably";

/// The `sway spawn (deep)` check: whether the compositor came up, and whether the probe stopped
/// everything it started.
fn deep_spawn_check(spawn: SwaySpawn) -> Check {
    const NAME: &str = "sway spawn (deep)";
    let leak = spawn.leaked.as_ref().map(leak_notice);
    let (status, mut detail, remedy) = match spawn.came_up {
        CameUp::Yes => match &leak {
            None => {
                return Check::new(NAME, CheckStatus::Ok, "headless sway started and stopped");
            }
            // The start half stands, so the check keeps it and warns about the half that does not.
            Some(leak) => (
                CheckStatus::Warn,
                format!("headless sway started, but {}", leak.detail),
                leak.remedy.clone(),
            ),
        },
        // The remedy comes from the failure itself, which withholds `SWAY_START_HINT` from the
        // outcomes that never reached sway (glass#373).
        CameUp::No(failure) => {
            let mut detail = failure.detail("headless sway");
            // Only on the timeout: after an exit, the missing socket has a reason the detail
            // already gives.
            if let (ProbeFailure::TimedOut(_), Some(why)) = (&failure, &spawn.last_ipc) {
                detail.push_str(&format!(" — {why}"));
            }
            with_leak(
                CheckStatus::Fail,
                detail,
                failure.remedy(SWAY_START_HINT),
                leak.as_ref(),
            )
        }
        // `Warn`, not `Fail`: a probe that established nothing is not evidence against the
        // backend, and `Skip` would say the check did not apply.
        CameUp::Unknown(why) => with_leak(
            CheckStatus::Warn,
            format!("glass lost track of the headless sway it started: {why}"),
            NOTHING_ABOUT_SWAY.to_string(),
            leak.as_ref(),
        ),
    };
    // Sway's own words go last and quoted: undelimited, the one span glass did not write can
    // imitate the clauses around it (glass#348).
    if let Some(said) = &spawn.said {
        detail.push_str(&format!(" — sway said: {said:?}"));
    }
    Check::new(NAME, status, detail).with_remedy(remedy)
}

/// Fold what the probe left running into a verdict already described.
fn with_leak(
    status: CheckStatus,
    mut detail: String,
    remedy: String,
    leak: Option<&Survivors>,
) -> (CheckStatus, String, String) {
    let Some(leak) = leak else {
        return (status, detail, remedy);
    };
    detail.push_str(&format!(" — and {}", leak.detail));
    (status, detail, join_remedies(&remedy, &leak.remedy))
}

/// Two remedies as one, dropping either if it is empty.
fn join_remedies(first: &str, second: &str) -> String {
    match (first.trim(), second.trim()) {
        ("", other) | (other, "") => other.to_string(),
        (first, second) => format!("{first}. {second}"),
    }
}

/// What the check says about what the probe left running.
struct Survivors {
    detail: String,
    remedy: String,
}

/// Describe what was left running.
fn leak_notice(leaked: &Leaked) -> Survivors {
    let pids: Vec<String> = leaked.pids.iter().map(u32::to_string).collect();
    let (count, label) = match pids.len() {
        1 => ("1 process".to_string(), "pid"),
        n => (format!("{n} processes"), "pids"),
    };
    let dir = leaked.runtime_dir.display();
    Survivors {
        detail: format!(
            "{count} it started was still running {LEAK_GRACE:?} after the probe signalled it \
             ({label} {}); the probe's runtime dir is kept at {dir} rather than deleted, so \
             whatever is still using it keeps its sockets",
            pids.join(", ")
        ),
        // No cause is named: the same observation is produced by a process on its way out, one
        // the kernel cannot kill, and a pid that has been reused since.
        remedy: format!(
            "look first — `ps -p {} -o pid,stat,args` says whether they are still the probe's \
             sway; if they are, `kill -9 {}` and remove {dir}",
            pids.join(","),
            pids.join(" ")
        ),
    }
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

/// How much of the kept stderr a check quotes: a check's detail is one line an operator reads,
/// where what was kept is [`glass_proc_linux::STDERR_KEPT`].
const STDERR_SHOWN: usize = 512;

/// How long the probe waits for the stderr pipe to close after the compositor has been reaped.
const SAID_GRACE: Duration = Duration::from_millis(200);

/// Sway's stderr as one line of a check: lines joined, clipped to [`STDERR_SHOWN`] with the cut
/// disclosed. `None` when it said nothing.
fn said_line(text: &str) -> Option<String> {
    let said = text
        .lines()
        .map(str::trim)
        .filter(|line| !line.is_empty())
        .collect::<Vec<_>>()
        .join("; ");
    if said.is_empty() {
        return None;
    }
    if said.len() <= STDERR_SHOWN {
        return Some(said);
    }
    let cut = said.floor_char_boundary(STDERR_SHOWN);
    Some(format!(
        "{}… (first {cut} bytes of {})",
        &said[..cut],
        said.len()
    ))
}

/// What ended a wait for sway's IPC, and what the last connect attempt was told.
#[derive(Debug)]
struct IpcWait {
    came_up: CameUp,
    last_ipc: Option<String>,
}

/// Wait for `connect` to answer, giving up at `budget`, and stopping early when `exited` reports
/// the compositor gone.
///
/// A deadline, not a poll count: the failure names the wait it made, and a count times
/// [`IPC_POLL`] is not that wait — each turn also spends a connect attempt (glass#373).
///
/// Both halves are closures so the classification can be driven without a compositor.
fn await_ipc(
    budget: Duration,
    mut exited: impl FnMut() -> std::io::Result<Option<std::process::ExitStatus>>,
    mut connect: impl FnMut() -> Result<(), String>,
) -> IpcWait {
    let deadline = Instant::now() + budget;
    let mut last_ipc = None;
    let came_up = loop {
        match exited() {
            // sway exited before its IPC came up — its own answer, and immediate: quoting the
            // budget below would claim a wait nobody made.
            Ok(Some(status)) => {
                break CameUp::No(ProbeFailure::Failed(format!(
                    "sway exited before its IPC came up ({status})"
                )));
            }
            // Whether sway is running can no longer be established — something else reaped it, so
            // `waitpid` answers ECHILD. Dropped, this read as "still running", so the wait spent
            // its whole budget and blamed a timeout.
            Err(e) => break CameUp::Unknown(format!("waiting on it failed: {e}")),
            Ok(None) => {}
        }
        match connect() {
            Ok(()) => break CameUp::Yes,
            Err(e) => last_ipc = Some(e),
        }
        if Instant::now() >= deadline {
            break CameUp::No(ProbeFailure::TimedOut(budget));
        }
        std::thread::sleep(IPC_POLL);
    };
    IpcWait { came_up, last_ipc }
}

/// A probe that ended before it spawned anything: nothing ran, so nothing was left behind.
fn never_ran(why: ProbeFailure) -> SwaySpawn {
    SwaySpawn {
        came_up: CameUp::No(why),
        last_ipc: None,
        said: None,
        leaked: None,
    }
}

/// How long the probe waits for what it started to leave before calling it left behind.
///
/// A signal lands when its target is next scheduled, so a check taken straight after one still
/// sees processes that are already going. Longer than a teardown's, because doctor has no budget
/// to fit inside.
const LEAK_GRACE: Duration = Duration::from_millis(500);

/// Tear the launch down and account for it: what the probe started that is still running, and the
/// runtime dir kept for it.
fn reap_and_account(child: &mut Child, rt: tempfile::TempDir) -> Option<Leaked> {
    // Snapshot the launch before any of it exits: once sway is reaped its descendants are
    // reparented to init and can no longer be found from its pid.
    let tree = glass_proc_linux::proc_tree_pids(child.id());
    let after_reap = glass_proc_linux::reap_launch(child, &tree, glass_proc_linux::APP_REAP_GRACE);
    Leaked::after_reap(left_running(rt.path(), &after_reap, LEAK_GRACE), rt)
}

/// Everything of the probe's own still running after `grace`, by both of the answers available —
/// neither is the set on its own.
///
/// `after_reap` is what the reaper could still see of the tree it signalled; only
/// [`session_processes`] finds a compositor's Xwayland or the app it `exec`s (glass#380).
fn left_running(runtime_dir: &Path, after_reap: &[u32], grace: Duration) -> Vec<u32> {
    let look = || {
        let mut left = crate::xwayland::session_processes(runtime_dir);
        left.extend(glass_proc_linux::live_pids(after_reap));
        left.sort_unstable();
        left.dedup();
        left
    };
    glass_proc_linux::await_condition(grace, || look().is_empty());
    look()
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
    let said = match StderrTail::drain(child.stderr.take().expect("piped stderr")) {
        Ok(said) => said,
        // `drain` consumed the pipe, so the read end is already closed and sway's next words
        // take SIGPIPE.
        Err(e) => {
            return SwaySpawn {
                came_up: CameUp::Unknown(format!(
                    "glass could not set up the reader for its stderr: {e}"
                )),
                last_ipc: None,
                said: None,
                leaked: reap_and_account(&mut child, rt),
            };
        }
    };

    let wait = await_ipc(budget, || child.try_wait(), || connect_ipc(rt.path()));
    // Reap first: closing the read end under a live compositor would EPIPE the writes being
    // collected.
    let leaked = reap_and_account(&mut child, rt);
    SwaySpawn {
        came_up: wait.came_up,
        last_ipc: wait.last_ipc,
        said: said_line(&said.finish(SAID_GRACE)),
        leaked,
    }
}

/// One attempt at sway's IPC, phrased for the check that quotes it: `Ipc::connect` answers both
/// "no socket" and "refused" with a `Backend` error, and those are different diagnoses.
fn connect_ipc(runtime_dir: &Path) -> Result<(), String> {
    match Ipc::connect(runtime_dir) {
        Ok(_) => Ok(()),
        Err(glass_core::GlassError::Backend(why)) if why == crate::swayipc::NO_IPC_SOCKET => {
            Err("no sway IPC socket ever appeared in its runtime dir".into())
        }
        Err(e) => Err(format!("its IPC socket refused the connection: {e}")),
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
        let (sway, _) = discover_sway().expect("this box has a discoverable sway");
        let before = compositors_running(&sway);
        let deep = checks(true)
            .into_iter()
            .find(|c| c.name == "sway spawn (deep)")
            .expect("deep asks for the spawn check");
        // `Ok` is the stop as well as the start now: a leaked compositor warns (glass#380).
        assert_eq!(deep.status, CheckStatus::Ok, "{}", deep.detail);
        // Counted from outside glass as well, so the verdict is not the only witness to it.
        assert_eq!(
            compositors_running(&sway),
            before,
            "the deep probe left a compositor behind"
        );
    }

    /// How many of the deep probe's own compositors are running, matched on both the probe's
    /// runtime-dir prefix and `sway` itself so a real session is never counted — nor a sibling
    /// test's fixture compositor, which the prefix alone does not tell apart.
    fn compositors_running(sway: &Path) -> usize {
        let sway = sway.display().to_string();
        let out = std::process::Command::new("ps")
            .args(["-eo", "args"])
            .output()
            .expect("ps");
        String::from_utf8_lossy(&out.stdout)
            .lines()
            .filter(|l| l.contains("glass-doctor-wl.") && l.contains(&sway))
            .count()
    }

    /// glass#373: `/bin/false` is gone before the first poll, and this reported "did not come up
    /// within ~8s" — an eight-second wait that took none. Needs no sway.
    #[test]
    fn a_compositor_that_exited_is_not_reported_as_one_that_never_answered() {
        let spawn = probe_sway(Path::new("/bin/false"));
        let CameUp::No(ProbeFailure::Failed(why)) = &spawn.came_up else {
            panic!("an exit is the compositor's own answer, not a wait: {spawn:?}");
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
        let pidfile = dir.path().join("pid");
        let fake = fake_sway(
            dir.path(),
            &format!("echo $$ > {}\nexec sleep 30\n", pidfile.display()),
        );

        // Long enough that a loaded CI box has written the pidfile below before the probe gives
        // up on it; short enough that the test is not the slow one in the suite.
        let budget = Duration::from_millis(500);
        let started = Instant::now();
        let spawn = probe_sway_within(&fake, budget);
        assert!(
            matches!(&spawn.came_up, CameUp::No(ProbeFailure::TimedOut(b)) if *b == budget),
            "{spawn:?}"
        );
        // The real path's answer, not one a test supplied: no socket ever appeared here.
        assert_eq!(
            spawn.last_ipc.as_deref(),
            Some("no sway IPC socket ever appeared in its runtime dir")
        );
        // A teardown that reached everything keeps nothing, and deletes its runtime dir.
        assert!(spawn.leaked.is_none(), "{spawn:?}");
        assert!(
            !dir.path().join("glass-doctor-wl.").exists(),
            "the probe's own runtime dir is not this one"
        );
        // The budget plus the teardown that follows it — the units this actually spans. A wait
        // that ignored its argument would blow this by `IPC_READY_BUDGET`.
        let ceiling = budget * 2 + glass_proc_linux::APP_REAP_GRACE * 2 + LEAK_GRACE;
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
        let fake = fake_sway(
            dir.path(),
            "echo 'sway: could not create GLES2 renderer' >&2\n\
             echo 'sway: Unable to create renderer' >&2\nexit 1\n",
        );

        // Generous: the wait ends the moment the exit is observed, so this bounds nothing the
        // test is about.
        let spawn = probe_sway_within(&fake, Duration::from_secs(5));
        let cs = wayland_checks(&answered("1.12"), true, Some(spawn));
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail, "{deep:?}");
        // Both lines, joined: a compositor's account of a failure is rarely one line.
        assert!(
            deep.detail
                .contains("could not create GLES2 renderer; sway: Unable to create renderer"),
            "{deep:?}"
        );
    }

    /// glass#471: the write end of sway's stderr is inherited by anything it leaves outside its
    /// own process tree — an Xwayland, an `exec`ed client — so EOF is not a deadline the reader
    /// can count on reaching.
    #[test]
    fn a_compositor_that_left_something_holding_its_stderr_still_reports_what_it_said() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pidfile = dir.path().join("survivor");
        let fake = fake_sway(
            dir.path(),
            // `setsid` puts it in its own session, so the pid-tree snapshot cannot see it and
            // the reap cannot signal it. The `exec` keeps the pid the shell reported, so the
            // guard can reach it, and the sleep self-limits if it cannot.
            &format!(
                "echo 'sway: Unable to create renderer' >&2\n\
                 setsid sh -c 'echo $$ > {}; exec sleep 30' &\n\
                 exit 1\n",
                pidfile.display()
            ),
        );

        // Off-thread: a reader with no exit of its own must fail this test, not hang the suite.
        let (tx, rx) = std::sync::mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(probe_sway_within(&fake, Duration::from_secs(5)));
        });
        let spawn = rx
            .recv_timeout(Duration::from_secs(30))
            .expect("the probe must return; a reader parked in read() never does");
        let _reap = Survivor(&pidfile);

        assert!(
            spawn
                .said
                .as_deref()
                .is_some_and(|said| said.contains("Unable to create renderer")),
            "what sway said must survive a survivor holding its stderr: {spawn:?}"
        );
    }

    /// glass#471: `finish` closes the read end, so collecting before the reap takes sway's
    /// stderr away from it while it is still running.
    #[test]
    fn a_compositor_says_what_it_said_on_the_way_out_too() {
        let dir = tempfile::tempdir().expect("tempdir");
        let fake = fake_sway(
            dir.path(),
            // `sleep &` + `wait`, not `sleep`: a foreground child defers the trap until it
            // returns, so the compositor would ride out the whole reap grace instead of
            // answering the SIGTERM.
            "trap 'echo \"sway: caught SIGTERM\" >&2; exit 0' TERM\n\
             sleep 30 &\n\
             wait\n",
        );

        // Short: the compositor never brings its IPC up, so this budget is pure waiting.
        let spawn = probe_sway_within(&fake, Duration::from_secs(1));

        assert!(
            spawn
                .said
                .as_deref()
                .is_some_and(|said| said.contains("caught SIGTERM")),
            "the reader must outlive the reap it is reporting on: {spawn:?}"
        );
    }

    /// Kills what a fixture left running, however the test ends. The pid is read at drop time:
    /// the fixture writes it asynchronously, and a test that failed early may never have looked.
    struct Survivor<'a>(&'a Path);

    impl Drop for Survivor<'_> {
        fn drop(&mut self) {
            let pid = std::fs::read_to_string(self.0)
                .ok()
                .and_then(|p| p.trim().parse::<i32>().ok())
                .and_then(rustix::process::Pid::from_raw);
            if let Some(pid) = pid {
                let _ = rustix::process::kill_process(pid, rustix::process::Signal::KILL);
            }
        }
    }

    /// A fake sway at `dir/sway` running `body`, which rejects an argv that is not the one glass
    /// builds — so a probe that stopped passing sway's arguments, its config or its runtime dir
    /// cannot pass a test by accident.
    fn fake_sway(dir: &Path, body: &str) -> PathBuf {
        let fake = dir.join("sway");
        std::fs::write(
            &fake,
            format!(
                "#!/bin/sh\n\
                 case \"$*\" in *--unsupported-gpu*-c*sway.cfg*) ;; *) exit 64;; esac\n\
                 [ -n \"$XDG_RUNTIME_DIR\" ] || exit 64\n{body}"
            ),
        )
        .expect("write fake sway");
        std::fs::set_permissions(&fake, std::fs::Permissions::from_mode(0o755))
            .expect("chmod fake sway");
        fake
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
            || Err(std::io::Error::other("No child processes")),
            || Err("no socket".into()),
        );
        let CameUp::Unknown(why) = &wait.came_up else {
            panic!("a question glass could not ask is not sway's answer: {wait:?}");
        };
        assert!(why.contains("No child processes"), "{why}");
        assert!(
            started.elapsed() < budget / 2,
            "the wait ended on the error, so none of the budget is spent: {:?}",
            started.elapsed()
        );
    }

    /// And what it means: nothing about sway, so nothing pointing at sway's dependencies.
    #[test]
    fn a_probe_that_lost_track_of_sway_does_not_blame_it() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(spawned(CameUp::Unknown(
                "waiting on it failed: ECHILD".into(),
            ))),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Warn, "{deep:?}");
        assert!(deep.detail.contains("ECHILD"), "{deep:?}");
        assert_ne!(
            deep.remedy.as_deref(),
            Some(SWAY_START_HINT),
            "a probe that established nothing is not evidence against sway: {deep:?}"
        );
    }

    /// The other dropped error: `Ipc::connect(..).is_ok()` threw away what the connection said,
    /// so a socket that exists and refuses read exactly like one that never appeared.
    #[test]
    fn a_timeout_carries_the_last_thing_the_ipc_connect_said() {
        let budget = Duration::from_millis(250);
        let attempts = std::cell::Cell::new(0);
        let wait = await_ipc(
            budget,
            || Ok(None),
            || {
                attempts.set(attempts.get() + 1);
                Err(format!("attempt {}", attempts.get()))
            },
        );
        assert!(matches!(
            wait.came_up,
            CameUp::No(ProbeFailure::TimedOut(b)) if b == budget
        ));
        // The last, not the first: they differ, so an implementation that kept the earliest fails.
        assert_eq!(
            wait.last_ipc.as_deref(),
            Some(format!("attempt {}", attempts.get()).as_str())
        );
        assert!(attempts.get() > 1, "the wait made one attempt only");
    }

    /// And it has to reach the check.
    #[test]
    fn the_check_shows_what_the_ipc_connect_said() {
        let refused = "its IPC socket refused the connection: Connection refused (os error 111)";
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: CameUp::No(ProbeFailure::TimedOut(Duration::from_secs(8))),
                last_ipc: Some(refused.into()),
                said: None,
                leaked: None,
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert!(deep.detail.contains(refused), "{deep:?}");

        // Withheld after an exit, where the missing socket has a reason the detail already gives.
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: CameUp::No(ProbeFailure::Failed("sway exited …".into())),
                last_ipc: Some(refused.into()),
                said: None,
                leaked: None,
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert!(!deep.detail.contains(refused), "{deep:?}");
    }

    /// A probe outcome with nothing left running — the shape of every case but the leak tests.
    fn spawned(came_up: CameUp) -> SwaySpawn {
        SwaySpawn {
            came_up,
            last_ipc: None,
            said: None,
            leaked: None,
        }
    }

    /// What the probe reports when it left `pids` behind.
    fn leaked(pids: Vec<u32>) -> Option<Leaked> {
        Some(Leaked {
            pids,
            runtime_dir: PathBuf::from("/tmp/glass-doctor-wl.abc123"),
        })
    }

    /// glass#380: the detail said "started and stopped" on the strength of the start alone, so a
    /// probe that leaked its compositor reported the same green line as one that tore it down.
    #[test]
    fn a_probe_that_left_its_compositor_running_is_not_reported_as_stopped() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: CameUp::Yes,
                last_ipc: None,
                said: None,
                leaked: leaked(vec![4242, 4243]),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Warn, "{deep:?}");
        assert!(deep.detail.contains("2 processes"), "{deep:?}");
        // An operator can only kill what is named.
        assert!(deep.detail.contains("4242, 4243"), "{deep:?}");
        assert!(
            deep.remedy
                .as_deref()
                .is_some_and(|r| r.contains("kill -9 4242 4243")),
            "{deep:?}"
        );
    }

    /// The probe keeps the runtime dir when something is still using it, so both halves of the
    /// check have to say where it is.
    #[test]
    fn a_kept_runtime_dir_is_named_so_it_can_be_cleaned_up() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: CameUp::Yes,
                last_ipc: None,
                said: None,
                leaked: leaked(vec![4242]),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert!(
            deep.detail.contains("/tmp/glass-doctor-wl.abc123"),
            "{deep:?}"
        );
        let remedy = deep.remedy.as_deref().expect("a leak has a remedy");
        assert!(remedy.contains("/tmp/glass-doctor-wl.abc123"), "{remedy}");
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
                came_up: CameUp::No(ProbeFailure::TimedOut(Duration::from_secs(8))),
                last_ipc: None,
                said: None,
                leaked: leaked(vec![4242]),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        assert_eq!(deep.status, CheckStatus::Fail, "{deep:?}");
        assert!(deep.detail.contains("8s"), "the failure survives: {deep:?}");
        assert!(deep.detail.contains("4242"), "{deep:?}");
    }

    /// Sway's stderr is the one span of the detail glass did not write, so it is quoted and last —
    /// an unquoted line ending in glass's own separator reads as glass speaking.
    #[test]
    fn what_sway_said_is_quoted_and_last() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(SwaySpawn {
                came_up: CameUp::No(ProbeFailure::Failed("sway exited …".into())),
                last_ipc: None,
                said: Some("— and 9 processes of its launch outlived it".into()),
                leaked: leaked(vec![4242]),
            }),
        );
        let deep = cs.iter().find(|c| c.name == "sway spawn (deep)").unwrap();
        let quoted = deep.detail.find('"').expect("sway's words are quoted");
        assert!(
            deep.detail[..quoted].contains("pid 4242"),
            "glass's own clauses come first: {deep:?}"
        );
    }

    /// A compositor's log is not a check's detail: what is quoted is clipped, and the clip says so.
    #[test]
    fn a_long_stderr_is_clipped_and_says_it_was() {
        let long = "e".repeat(STDERR_SHOWN * 2);
        let said = said_line(&long).expect("something was said");
        assert!(said.len() < long.len(), "clipped");
        assert!(said.contains(&format!("of {}", long.len())), "{said}");
        assert_eq!(said_line("  \n \n"), None, "silence is not something said");
    }

    #[test]
    fn deep_spawn_failure_is_reported() {
        let cs = wayland_checks(
            &answered("1.12"),
            true,
            Some(spawned(CameUp::No(ProbeFailure::Failed(
                "no come up".into(),
            )))),
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
            Some(spawned(CameUp::No(ProbeFailure::NotStarted(
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
        let spawn = probe_sway(missing);
        let CameUp::No(err) = &spawn.came_up else {
            panic!("a spawn that never happened is not sway's answer: {spawn:?}");
        };
        assert!(matches!(err, ProbeFailure::NotStarted(_)), "{err:?}");
        assert!(
            !err.remedy(SWAY_START_HINT).contains("Mesa"),
            "{:?}",
            err.remedy(SWAY_START_HINT)
        );
    }
}
