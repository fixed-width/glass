//! Process lifecycle for the Windows backend: spawn the app **CREATE_SUSPENDED** without a
//! launcher console window,
//! assign it to a `KILL_ON_JOB_CLOSE` Job before it can fork any children, wire
//! up log readers, then resume — so a launcher that exits and hands off
//! (Chromium/Electron) cannot escape the Job. Teardown asks the app's windows to close and
//! only then closes the job handle, which is the backstop for whatever did not go (see
//! [`LaunchedApp::kill`] and [`crate::teardown`]).
//!
//! The FFI here is a verbatim port of the validated
//! `tools/windows-validation/src/proc.rs` probe (proven on real hardware): the
//! CREATE_SUSPENDED spawn, then `CreateJobObjectW` with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and `AssignProcessToJobObject`, then resume;
//! plus the Toolhelp thread enum with `ResumeThread`, and the Toolhelp parent-link
//! descendant walk. It is restructured into reusable lib types but the unsafe bodies
//! are unchanged. The `PostMessageW` close request in [`request_close`] is not from that
//! probe — it was added later, with its own on-box evidence.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use glass_core::Stream;
use glass_core::{AppSpec, Deadline, GlassError, Result, SandboxLevel, TEARDOWN_BUDGET};

use crate::teardown::{CloseStep, close_wait_step};
use windows::Win32::Foundation::{CloseHandle, HANDLE, LPARAM, WPARAM};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW, TH32CS_SNAPPROCESS,
    TH32CS_SNAPTHREAD, THREADENTRY32, Thread32First, Thread32Next,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JOB_OBJECT_LIMIT, JOB_OBJECT_LIMIT_ACTIVE_PROCESS,
    JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION,
    JobObjectBasicProcessIdList, JobObjectExtendedLimitInformation, QueryInformationJobObject,
    SetInformationJobObject,
};
use windows::Win32::System::Threading::{
    CREATE_NO_WINDOW, CREATE_SUSPENDED, OpenProcess, OpenThread, PROCESS_QUERY_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, ResumeThread, THREAD_SUSPEND_RESUME,
};
use windows::Win32::UI::WindowsAndMessaging::{PostMessageW, WM_CLOSE};

/// How long the app tree gets to close itself after the `WM_CLOSE` broadcast, before the Job is
/// closed (`TerminateProcess` for whatever is left). Only an app that will not close pays this in
/// full — a cooperating one ends the wait the moment its tree is gone. Measured on-box, an
/// isolated Edge tree closed in well under half a second, so this is several times the observed
/// cost while still leaving the Job close room inside [`TEARDOWN_BUDGET`].
///
/// It has to leave that room: when the *process* is going away, `glass-mcp` abandons teardown
/// once the budget is spent. A grace that filled it would mean the Job was never closed from
/// here — the tree would instead be terminated as a side effect of the handle closing at process
/// exit, which is the ungraceful teardown this whole path exists to avoid, arrived at slowly.
const CLOSE_GRACE: Duration = Duration::from_millis(1500);

const APP_CREATION_FLAGS: u32 = CREATE_SUSPENDED.0 | CREATE_NO_WINDOW.0;

// Teardown must finish inside the budget it is given, or `glass-mcp` walks away mid-close. This
// is the only thing keeping the two numbers — in different crates — honest about each other.
const _: () = assert!(
    CLOSE_GRACE.as_millis() * 2 <= TEARDOWN_BUDGET.as_millis(),
    "CLOSE_GRACE must leave the Job close comfortable room inside glass_core::TEARDOWN_BUDGET"
);

/// How often the close wait re-checks the tree. Short relative to [`CLOSE_GRACE`] so a quick
/// shutdown is noticed promptly rather than rounded up to the next poll.
const CLOSE_POLL: Duration = Duration::from_millis(50);

/// Run the optional build step in `cwd` via `cmd /C` (the Windows shell), failing
/// if it exits non-zero. Mirrors the X11 backend's `sh -c` build step. Unconfined
/// variant (the Unconfined containment provider's build path).
pub(crate) fn run_build_unconfined(spec: &AppSpec) -> Result<()> {
    let Some(build) = &spec.build else {
        return Ok(());
    };
    let mut cmd = Command::new("cmd");
    cmd.arg("/C").arg(build);
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    let status = cmd
        .status()
        .map_err(|e| GlassError::AppNotStarted(format!("build command: {e}")))?;
    if !status.success() {
        return Err(GlassError::AppNotStarted(format!(
            "build command failed with status {status}"
        )));
    }
    Ok(())
}

/// Build the launch command from `spec.run` (+ env + cwd). No `DISPLAY` env on
/// Windows; the active interactive session is the display.
pub(crate) fn build_command(spec: &AppSpec) -> Command {
    let mut cmd = Command::new(&spec.run[0]);
    cmd.args(&spec.run[1..]);
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    if let Some(dir) = &spec.cwd {
        cmd.current_dir(dir);
    }
    cmd
}

/// A Windows kernel HANDLE wrapped to be Send. Kernel handles are process-global and
/// usable from any thread (not thread-affine), so sending one across threads is sound.
pub(crate) struct SendHandle(pub HANDLE);
// SAFETY: a Windows kernel HANDLE is a process-global value, valid from any thread; we
// only ever CloseHandle it. It is not thread-affine (unlike a window's message queue).
unsafe impl Send for SendHandle {}

/// A launched app: its CREATE_SUSPENDED-in-a-Job root process. Closing `job` (the last
/// handle) terminates the entire tree via JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE.
pub(crate) struct LaunchedApp {
    job: SendHandle,
    child: Child,
    /// The app's stdout/stderr readers, ended by `kill`. The app's write ends are inherited by
    /// everything it spawns, so an EOF-only reader parks on a survivor's pipe, holding a thread
    /// and a handle for the life of the process (glass#477).
    taps: Vec<crate::logtap::LogTap>,
}

impl LaunchedApp {
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Start the log readers over the piped stdout/stderr, filing into `logs`. Call once, before
    /// `resume`, so nothing the app writes at startup is missed.
    ///
    /// The readers stay here rather than being handed back: they are made from this handle's own
    /// pipes, and `kill` ends them. `Err` is the host refusing a thread, with that stream's pipe
    /// closed by then — the caller must fail the launch rather than resume an app whose output
    /// goes nowhere.
    pub(crate) fn tap_logs(&mut self, logs: &crate::logtap::LogSink) -> std::io::Result<()> {
        if let Some(out) = self.child.stdout.take() {
            self.taps.push(crate::logtap::LogTap::start(
                out,
                Stream::Stdout,
                logs.clone(),
            )?);
        }
        if let Some(err) = self.child.stderr.take() {
            self.taps.push(crate::logtap::LogTap::start(
                err,
                Stream::Stderr,
                logs.clone(),
            )?);
        }
        Ok(())
    }

    /// Terminate a launch that never started, closing the Job handle with it.
    ///
    /// `LaunchedApp` releases the Job only in [`LaunchedApp::kill`], and neither `SendHandle` nor
    /// `Child` frees anything on drop — so a launch abandoned between `spawn_suspended_in_job` and
    /// `resume` sits `CREATE_SUSPENDED` and alive for the life of this process, its Job handle
    /// leaked and `KILL_ON_JOB_CLOSE` therefore never firing. The root has never run, so it has no
    /// children; `spawn_suspended_in_job`'s own error arm reasons the same way.
    ///
    /// Any tap already started is ended by the drop that follows the kill.
    pub(crate) fn abandon(mut self) {
        // SAFETY: closing the last job handle terminates anything left in the tree.
        unsafe {
            let _ = CloseHandle(self.job.0);
        }
        let _ = self.child.kill();
        let _ = self.child.wait(); // reap, so an abandoned launch leaves no zombie
    }

    /// Resume the suspended root thread(s) — call AFTER log readers are wired.
    pub(crate) fn resume(&self) {
        resume_process(self.pid());
    }

    /// The Job's authoritative (kernel-tracked) PID set. Captures launcher-handoff
    /// children a Toolhelp parent-link walk can miss; empty on any query failure.
    pub(crate) fn job_pids(&self) -> Vec<u32> {
        job_pids(self.job.0)
    }

    /// Non-blocking check whether the root process has already exited (mirrors the X11
    /// backend's `child.try_wait()` so discovery can fail fast with `AppExited`).
    pub(crate) fn try_wait(&mut self) -> std::io::Result<Option<std::process::ExitStatus>> {
        self.child.try_wait()
    }

    /// Tear the whole tree down, asking it to close first: broadcast `WM_CLOSE` to the app's own
    /// top-level windows, give it up to [`CLOSE_GRACE`] to leave through its own shutdown path,
    /// then close the Job (terminating anything left) and reap the root.
    ///
    /// The Job close alone is `TerminateProcess`, which runs no shutdown path at all — an app that
    /// records whether it exited cleanly then reports a crash on its *next* launch and greets the
    /// agent with a recovery prompt instead of its normal first screen. See [`crate::teardown`] for
    /// the measurement behind that and for both decisions this drives.
    pub(crate) fn kill(mut self) -> Closed {
        let asked = request_close(&self.live_pids());
        let closed_itself = self.await_close(asked.posted);
        // SAFETY: closing the last job handle terminates the entire tree (KILL_ON_JOB_CLOSE).
        unsafe {
            let _ = CloseHandle(self.job.0);
        }
        let _ = self.child.kill(); // belt-and-suspenders: ensure the root is terminated so wait() can't block
        let _ = self.child.wait(); // reap the now-terminated root; avoids a zombie
        // Terminated first, so each tap's final drain sees what the app wrote on the way out.
        // Dropping them releases the pipe handles anything the launch left running still holds
        // (glass#477).
        self.taps.clear();
        asked.outcome(closed_itself)
    }

    /// The app's live process set: the Job's authoritative pid list, falling back to the Toolhelp
    /// parent-link walk when the Job query fails. Without the fallback, a failed query would leave
    /// a handed-off app with no window to ask.
    ///
    /// Job-authoritative rather than a union with the walk (which is what *discovery* uses): this
    /// set drives a `WM_CLOSE` broadcast, and Windows parent links go stale when a pid is recycled,
    /// so a union could put an unrelated application's window in the destructive set. Discovery can
    /// afford that risk because it only ever reads.
    fn live_pids(&self) -> Vec<u32> {
        job_pids_checked(self.job.0).unwrap_or_else(|| descendant_pids(self.pid()))
    }

    /// Poll until the tree has closed itself or [`CLOSE_GRACE`] is spent, per
    /// [`close_wait_step`]. `posted` is how many windows accepted the `WM_CLOSE`.
    ///
    /// Note this reads the Job pid-set through [`job_pids_checked`] rather than [`Self::live_pids`]
    /// — the two want different things from a failed query, and neither substitutes for the other.
    /// `live_pids` needs *some* pid set to find windows in, so it falls back to the Toolhelp walk;
    /// that walk always contains the root, so using it here would report the tree as live on every
    /// poll and spend the full grace on even a cleanly-closing app. What this needs is the honest
    /// third answer, `None`, which [`close_wait_step`] reads as "unknown, keep waiting".
    /// Returns whether the tree closed itself before the grace ran out — a `false` means whatever
    /// is left is about to be terminated. A `try_wait` error counts as "still running": the pid is
    /// ours, so a failure is unexpected, and waiting is the safe reading of an unreadable state
    /// (the deadline bounds the cost). Mirrors `glass-macos`'s `wait_for_exit`.
    fn await_close(&mut self, posted: usize) -> bool {
        let deadline = Instant::now() + CLOSE_GRACE;
        loop {
            let root_exited = matches!(self.child.try_wait(), Ok(Some(_)));
            let job_live = job_pids_checked(self.job.0).map(|pids| !pids.is_empty());
            if Instant::now() >= deadline {
                return false;
            }
            if close_wait_step(posted, root_exited, job_live, false) == CloseStep::Proceed {
                // Distinguish the two non-deadline exits: nothing was asked (so nothing could
                // have closed itself) from the tree having actually gone.
                return posted > 0;
            }
            std::thread::sleep(CLOSE_POLL);
        }
    }
}

/// How a launched app's process tree actually went away, so `stop_app` can say so. Terminating an
/// app that was never asked, or asked and refused, is the outcome this whole path exists to avoid
/// — reporting it as an unqualified success would leave the agent to rediscover it as a recovery
/// prompt on the next launch, with nothing connecting the two.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Closed {
    /// Asked, and the tree left through its own shutdown path.
    Gracefully,
    /// Nothing accepted a close request — a windowless app, or every post was refused — so the
    /// tree was terminated without being asked. Carries how many windows were asked but refused
    /// (UIPI blocks posting to a higher-integrity process), which is actionable in a way the
    /// windowless case is not.
    TerminatedUnasked { refused: usize },
    /// Asked, but still alive when the grace ran out: a modal shutdown prompt, or a hang.
    TerminatedAfterGrace,
}

/// Report a teardown that could not be graceful. Silent on [`Closed::Gracefully`] — the good
/// path is the expectation, not news — and on a windowless app, which has no shutdown path to
/// have missed. Everything else is something the user can act on, and would otherwise only
/// surface as an unexplained recovery prompt the *next* time the app is launched.
pub(crate) fn disclose_teardown(closed: Closed) {
    match closed {
        Closed::Gracefully | Closed::TerminatedUnasked { refused: 0 } => {}
        Closed::TerminatedUnasked { refused } => eprintln!(
            "glass: {refused} close request(s) were refused, so the app was terminated instead \
             of asked to close. Windows blocks posting to a higher-integrity process (UIPI); \
             running glass elevated lets it close the app cleanly. An app that records clean \
             shutdown may report a crash on its next launch."
        ),
        Closed::TerminatedAfterGrace => eprintln!(
            "glass: the app did not close within {CLOSE_GRACE:?} of being asked (a modal \
             shutdown prompt will do this), so its process tree was terminated. Unsaved state \
             was not flushed, and the app may report a crash on its next launch."
        ),
    }
}

/// The result of asking an app's windows to close: how many accepted, and how many refused.
struct Asked {
    posted: usize,
    refused: usize,
}

impl Asked {
    fn outcome(&self, closed_itself: bool) -> Closed {
        match (self.posted, closed_itself) {
            (0, _) => Closed::TerminatedUnasked {
                refused: self.refused,
            },
            (_, true) => Closed::Gracefully,
            (_, false) => Closed::TerminatedAfterGrace,
        }
    }
}

/// Post `WM_CLOSE` to every top-level window belonging to a process in `pids` that
/// [`wants_close_request`] accepts, reporting how many posts the OS accepted and how many it
/// refused.
///
/// The split matters twice over. A refused window is not one that is going to close, so counting
/// it would make the caller wait out the whole grace for a shutdown nobody was asked to perform.
/// And a refusal is a *diagnosable* condition — UIPI blocks posting to a higher-integrity process,
/// so every teardown of that app will be a hard kill until glass is run elevated — which is worth
/// telling the user rather than absorbing.
fn request_close(pids: &[u32]) -> Asked {
    let candidates: Vec<crate::util::WinInfo> = crate::util::enum_top_windows()
        .into_iter()
        .filter(|w| pids.contains(&w.pid) && w.wants_close_request())
        .collect();
    let posted = candidates
        .iter()
        // SAFETY: PostMessageW only queues a message on the target window's thread queue — it
        // writes through no pointer we own and does not block waiting for the app to handle it.
        .filter(|w| unsafe { PostMessageW(Some(w.hwnd()), WM_CLOSE, WPARAM(0), LPARAM(0)) }.is_ok())
        .count();
    Asked {
        posted,
        refused: candidates.len() - posted,
    }
}

/// Spawn `cmd` CREATE_SUSPENDED and CREATE_NO_WINDOW with piped stdout/stderr, create a
/// KILL_ON_JOB_CLOSE Job,
/// assign the (still-suspended, child-less) process to it, and return the LaunchedApp
/// WITHOUT resuming (so the caller can wire log readers before output starts).
///
/// The process is created suspended, so it has spawned no children yet — assigning it to
/// the Job now captures the entire future tree. A launcher that exits and hands off
/// (Chromium/Electron) therefore cannot escape; assigning after a normal spawn races that
/// handoff (the validation probe left 15 Edge children alive that way).
pub(crate) fn spawn_suspended_in_job(
    cmd: &mut Command,
    level: SandboxLevel,
) -> Result<LaunchedApp> {
    let mut child = cmd
        .creation_flags(APP_CREATION_FLAGS)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GlassError::AppNotStarted(format!("spawn (suspended): {e}")))?;
    let root = child.id();
    match build_kill_on_close_job(root, level) {
        Ok(job) => Ok(LaunchedApp {
            job: SendHandle(job),
            child,
            taps: Vec::new(),
        }),
        Err(e) => {
            // The child is still SUSPENDED and not yet job-assigned, so it has no children:
            // a single TerminateProcess on the root is sufficient. Kill + reap so we never
            // leave a suspended orphan (Child::drop closes the handle but does NOT kill) and
            // never leak the job/proc handles when job setup fails.
            let _ = child.kill();
            let _ = child.wait();
            Err(e)
        }
    }
}

/// Create a job and assign `root` to it, applying the Job-limit set for `level`
/// (kill-on-close + crash-dialog suppression always; an active-process cap for `Default`).
/// Closes every handle it acquires on its own error paths; returns the job handle on success.
fn build_kill_on_close_job(root: u32, level: SandboxLevel) -> Result<HANDLE> {
    let cfg = crate::jobcfg::job_config(level);
    // SAFETY: each FFI result is checked; every handle acquired here is closed on the
    // error path before returning, and `proc` is always closed before a successful return.
    // The process is suspended, so it has created no children yet — assigning it now
    // captures the entire future tree.
    unsafe {
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null())
            .map_err(|e| GlassError::AppNotStarted(format!("CreateJobObjectW: {e}")))?;
        let mut limit_flags = JOB_OBJECT_LIMIT(0);
        if cfg.kill_on_close {
            limit_flags |= JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE;
        }
        if cfg.suppress_crash_dialog {
            limit_flags |= JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION;
        }
        let mut basic = JOBOBJECT_BASIC_LIMIT_INFORMATION {
            LimitFlags: limit_flags,
            ..Default::default()
        };
        if let Some(limit) = cfg.active_process_limit {
            basic.LimitFlags |= JOB_OBJECT_LIMIT_ACTIVE_PROCESS;
            basic.ActiveProcessLimit = limit;
        }
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: basic,
            ..Default::default()
        };
        if let Err(e) = SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        ) {
            let _ = CloseHandle(job);
            return Err(GlassError::AppNotStarted(format!(
                "SetInformationJobObject: {e}"
            )));
        }
        let proc = match OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION,
            false,
            root,
        ) {
            Ok(p) => p,
            Err(e) => {
                let _ = CloseHandle(job);
                return Err(GlassError::AppNotStarted(format!("OpenProcess: {e}")));
            }
        };
        if let Err(e) = AssignProcessToJobObject(job, proc) {
            let _ = CloseHandle(proc);
            let _ = CloseHandle(job);
            return Err(GlassError::AppNotStarted(format!(
                "AssignProcessToJobObject: {e}"
            )));
        }
        let _ = CloseHandle(proc);
        Ok(job)
    }
}

/// One process snapshot entry: (pid, parent pid).
struct Proc {
    pid: u32,
    ppid: u32,
}

/// Snapshot every process as (pid, parent pid). Verbatim port of proc.rs::snapshot
/// (the exe name is dropped — descendant discovery only needs the parent links).
fn snapshot_by(deadline: Deadline) -> Result<Vec<Proc>> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started(
            "Windows accessibility process discovery",
        ));
    }
    let mut out = Vec::new();
    // SAFETY: standard Toolhelp snapshot walk; handle closed before return.
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return Ok(out),
        };
        if deadline.has_passed() {
            let _ = CloseHandle(snap);
            return Err(GlassError::caller_deadline_elapsed(
                "Windows accessibility process discovery",
            ));
        }
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                if deadline.has_passed() {
                    let _ = CloseHandle(snap);
                    return Err(GlassError::caller_deadline_elapsed(
                        "Windows accessibility process discovery",
                    ));
                }
                out.push(Proc {
                    pid: e.th32ProcessID,
                    ppid: e.th32ParentProcessID,
                });
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    if deadline.has_passed() {
        Err(GlassError::caller_deadline_elapsed(
            "Windows accessibility process discovery",
        ))
    } else {
        Ok(out)
    }
}

/// All live descendant PIDs of `root` (inclusive) via a Toolhelp snapshot's parent links.
/// Verbatim port of proc.rs::descendants, returning just the pids.
pub(crate) fn descendant_pids(root: u32) -> Vec<u32> {
    descendant_pids_by(root, Deadline::UNBOUNDED).unwrap_or_else(|_| vec![root])
}

pub(crate) fn descendant_pids_by(root: u32, deadline: Deadline) -> Result<Vec<u32>> {
    let all = snapshot_by(deadline)?;
    let mut keep = vec![root];
    let mut i = 0;
    while i < keep.len() {
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed(
                "Windows accessibility process discovery",
            ));
        }
        let parent = keep[i];
        for p in &all {
            if deadline.has_passed() {
                return Err(GlassError::caller_deadline_elapsed(
                    "Windows accessibility process discovery",
                ));
            }
            if p.ppid == parent && !keep.contains(&p.pid) {
                keep.push(p.pid);
            }
        }
        i += 1;
    }
    Ok(keep)
}

/// [`job_pids_checked`], flattening an unreadable pid-set to an empty one — for callers that
/// only need a list to search and treat "couldn't ask" and "nothing there" alike.
pub(crate) fn job_pids(job: HANDLE) -> Vec<u32> {
    job_pids_checked(job).unwrap_or_default()
}

/// The job's authoritative PID set (kernel-tracked) — captures children of an exited launcher
/// (Electron/Chromium handoff) that a Toolhelp parent-link walk can miss.
///
/// `None` means the query itself failed, which is a genuinely different answer from
/// `Some(vec![])` ("the job is empty, the tree is gone"). Teardown turns on that difference:
/// reading an unreadable pid-set as "gone" would let [`LaunchedApp::await_close`] terminate a
/// handed-off app's children on the first poll, immediately after asking them to close.
pub(crate) fn job_pids_checked(job: HANDLE) -> Option<Vec<u32>> {
    // JOBOBJECT_BASIC_PROCESS_ID_LIST is variable-length: { NumberOfAssignedProcesses: u32,
    // NumberOfProcessIdsInList: u32, ProcessIdList: [usize; 1] (flexible) }. Size the buffer
    // for `cap` pids and grow on failure (too-small buffer), capped to avoid a pathological loop.
    let mut cap = 64usize;
    loop {
        let bytes = std::mem::size_of::<u32>() * 2 + cap * std::mem::size_of::<usize>();
        let mut buf = vec![0u8; bytes];
        // SAFETY: buf is `bytes` long; QueryInformationJobObject writes at most that many bytes into
        // it and records the count in the header. `job` is a valid job handle we own. We read the
        // buffer back only via the safe parse_job_pid_list (from_ne_bytes) — no typed pointer is ever
        // formed over `buf`, so its (byte) alignment is irrelevant.
        let ok = unsafe {
            QueryInformationJobObject(
                Some(job),
                JobObjectBasicProcessIdList,
                buf.as_mut_ptr() as *mut c_void,
                bytes as u32,
                None,
            )
        };
        if ok.is_err() {
            if cap < 8192 {
                cap *= 4;
                continue;
            } // too-small buffer: grow; terminal error: report the state as unknown
            return None;
        }
        return Some(crate::jobpids::parse_job_pid_list(&buf));
    }
}

/// Resume every thread of `pid` — a CREATE_SUSPENDED process has one suspended main
/// thread. Verbatim port of proc.rs::resume_process.
fn resume_process(pid: u32) {
    // SAFETY: Toolhelp thread snapshot; each opened thread handle is closed.
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPTHREAD, 0) {
            Ok(h) => h,
            Err(_) => return,
        };
        let mut te = THREADENTRY32 {
            dwSize: std::mem::size_of::<THREADENTRY32>() as u32,
            ..Default::default()
        };
        if Thread32First(snap, &mut te).is_ok() {
            loop {
                if te.th32OwnerProcessID == pid
                    && let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, te.th32ThreadID)
                {
                    ResumeThread(h);
                    let _ = CloseHandle(h);
                }
                if Thread32Next(snap, &mut te).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn app_spawn_is_suspended_without_a_console_window() {
        assert_ne!(APP_CREATION_FLAGS & CREATE_SUSPENDED.0, 0);
        assert_ne!(APP_CREATION_FLAGS & CREATE_NO_WINDOW.0, 0);
    }
}
