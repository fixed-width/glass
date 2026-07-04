//! Process lifecycle for the Windows backend: spawn the app **CREATE_SUSPENDED**,
//! assign it to a `KILL_ON_JOB_CLOSE` Job before it can fork any children, wire
//! up log readers, then resume — so a launcher that exits and hands off
//! (Chromium/Electron) cannot escape the Job. Teardown is "close the job handle".
//!
//! The FFI here is a verbatim port of the validated
//! `tools/windows-validation/src/proc.rs` probe (proven on real hardware): the
//! CREATE_SUSPENDED spawn, then `CreateJobObjectW` with
//! `JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE` and `AssignProcessToJobObject`, then resume;
//! plus the Toolhelp thread enum with `ResumeThread`, and the Toolhelp parent-link
//! descendant walk. It is restructured into reusable lib types but the unsafe bodies
//! are unchanged.

use std::ffi::c_void;
use std::os::windows::process::CommandExt;
use std::process::{Child, Command, Stdio};

use glass_core::{AppSpec, GlassError, Result, SandboxLevel};

use windows::Win32::Foundation::{CloseHandle, HANDLE};
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    PROCESSENTRY32W, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD, THREADENTRY32,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, JobObjectBasicProcessIdList,
    JobObjectExtendedLimitInformation, QueryInformationJobObject, SetInformationJobObject,
    JOBOBJECT_BASIC_LIMIT_INFORMATION, JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT,
    JOB_OBJECT_LIMIT_ACTIVE_PROCESS, JOB_OBJECT_LIMIT_DIE_ON_UNHANDLED_EXCEPTION,
    JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, CREATE_SUSPENDED, PROCESS_QUERY_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
};

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
}

impl LaunchedApp {
    pub(crate) fn pid(&self) -> u32 {
        self.child.id()
    }

    /// Take the piped stdout/stderr (call once, before resume, to wire log readers).
    pub(crate) fn take_pipes(
        &mut self,
    ) -> (
        Option<std::process::ChildStdout>,
        Option<std::process::ChildStderr>,
    ) {
        (self.child.stdout.take(), self.child.stderr.take())
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

    /// Kill the whole tree (close the job) and reap the root.
    pub(crate) fn kill(mut self) {
        // SAFETY: closing the last job handle terminates the entire tree (KILL_ON_JOB_CLOSE).
        unsafe {
            let _ = CloseHandle(self.job.0);
        }
        let _ = self.child.kill(); // belt-and-suspenders: ensure the root is terminated so wait() can't block
        let _ = self.child.wait(); // reap the now-terminated root; avoids a zombie
    }
}

/// Spawn `cmd` CREATE_SUSPENDED with piped stdout/stderr, create a KILL_ON_JOB_CLOSE Job,
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
        .creation_flags(CREATE_SUSPENDED.0)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .map_err(|e| GlassError::AppNotStarted(format!("spawn (suspended): {e}")))?;
    let root = child.id();
    match build_kill_on_close_job(root, level) {
        Ok(job) => Ok(LaunchedApp {
            job: SendHandle(job),
            child,
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
fn snapshot() -> Vec<Proc> {
    let mut out = Vec::new();
    // SAFETY: standard Toolhelp snapshot walk; handle closed before return.
    unsafe {
        let snap = match CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) {
            Ok(h) => h,
            Err(_) => return out,
        };
        let mut e = PROCESSENTRY32W {
            dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32,
            ..Default::default()
        };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
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
    out
}

/// All live descendant PIDs of `root` (inclusive) via a Toolhelp snapshot's parent links.
/// Verbatim port of proc.rs::descendants, returning just the pids.
pub(crate) fn descendant_pids(root: u32) -> Vec<u32> {
    let all = snapshot();
    let mut keep = vec![root];
    let mut i = 0;
    while i < keep.len() {
        let parent = keep[i];
        for p in &all {
            if p.ppid == parent && !keep.contains(&p.pid) {
                keep.push(p.pid);
            }
        }
        i += 1;
    }
    keep
}

/// The job's authoritative PID set (kernel-tracked) — captures children of an exited launcher
/// (Electron/Chromium handoff) that a Toolhelp parent-link walk can miss. Empty on any query
/// failure, so the caller can fall back to descendant_pids().
pub(crate) fn job_pids(job: HANDLE) -> Vec<u32> {
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
            } // too-small buffer: grow; terminal error: give up empty
            return Vec::new();
        }
        return crate::jobpids::parse_job_pid_list(&buf);
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
                if te.th32OwnerProcessID == pid {
                    if let Ok(h) = OpenThread(THREAD_SUSPEND_RESUME, false, te.th32ThreadID) {
                        ResumeThread(h);
                        let _ = CloseHandle(h);
                    }
                }
                if Thread32Next(snap, &mut te).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
}
