//! Process-tree probes: child-process window discovery (validation item 4) and
//! Job-Object kill-tree teardown (item 6). Shares a Toolhelp process snapshot.

use std::ffi::c_void;
use std::time::Duration;
use windows::Win32::Foundation::CloseHandle;
use std::os::windows::process::CommandExt;
use windows::Win32::System::Diagnostics::ToolHelp::{
    CreateToolhelp32Snapshot, Process32FirstW, Process32NextW, Thread32First, Thread32Next,
    PROCESSENTRY32W, THREADENTRY32, TH32CS_SNAPPROCESS, TH32CS_SNAPTHREAD,
};
use windows::Win32::System::JobObjects::{
    AssignProcessToJobObject, CreateJobObjectW, SetInformationJobObject,
    JobObjectExtendedLimitInformation, JOBOBJECT_BASIC_LIMIT_INFORMATION,
    JOBOBJECT_EXTENDED_LIMIT_INFORMATION, JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
};
use windows::Win32::System::Threading::{
    OpenProcess, OpenThread, ResumeThread, CREATE_SUSPENDED, PROCESS_QUERY_INFORMATION,
    PROCESS_SET_QUOTA, PROCESS_TERMINATE, THREAD_SUSPEND_RESUME,
};

use crate::util::enum_top_windows;

struct Proc {
    pid: u32,
    ppid: u32,
    exe: String,
}

/// Snapshot every process as (pid, parent pid, exe name).
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
                let end = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                out.push(Proc {
                    pid: e.th32ProcessID,
                    ppid: e.th32ParentProcessID,
                    exe: String::from_utf16_lossy(&e.szExeFile[..end]),
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

/// All live descendants of `root` (inclusive), via the parent links in a snapshot.
fn descendants(root: u32) -> Vec<Proc> {
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
    all.into_iter().filter(|p| keep.contains(&p.pid)).collect()
}

pub fn spawn(cmd: &[String]) -> anyhow::Result<u32> {
    anyhow::ensure!(!cmd.is_empty(), "need a command to --spawn");
    let child = std::process::Command::new(&cmd[0]).args(&cmd[1..]).spawn()?;
    Ok(child.id())
}

/// item 4 — discover the app's window via its *descendant* PID set, not the root PID.
pub fn discover(root: u32, spawned: bool) -> anyhow::Result<()> {
    if !spawned {
        println!("waiting 3s for windows to appear (pid {root})...");
    }
    std::thread::sleep(Duration::from_secs(3));

    let descs = descendants(root);
    let pids: Vec<u32> = descs.iter().map(|p| p.pid).collect();
    println!("\nprocess tree under pid {root} ({} processes):", descs.len());
    for p in &descs {
        let marker = if p.pid == root { " (root)" } else { "" };
        println!("  pid {:>6}  ppid {:>6}  {}{}", p.pid, p.ppid, p.exe, marker);
    }

    let wins: Vec<_> = enum_top_windows()
        .into_iter()
        .filter(|w| w.looks_like_app_window() && pids.contains(&w.pid))
        .collect();

    println!("\napp windows owned by the process tree:");
    if wins.is_empty() {
        println!("  (none yet — app may still be starting, or hands off to an unrelated process)");
    }
    let mut via_descendant = false;
    for w in &wins {
        let who = if w.pid == root { "ROOT pid" } else { "DESCENDANT pid" };
        if w.pid != root {
            via_descendant = true;
        }
        println!("  '{}'  (class {})  {} {}", w.title, w.class, who, w.pid);
    }

    println!();
    if via_descendant {
        println!("PASS: a visible window is owned by a DESCENDANT pid — root-pid-only discovery would have missed it.");
    } else if !wins.is_empty() {
        println!("NOTE: all windows are owned by the root pid (single-process app); the descendant path still applies for Electron/Java.");
    } else {
        println!("FAIL/RETRY: no app window found for the tree — check the title or increase the wait.");
    }
    Ok(())
}

/// item 6 — Job-Object kill-tree done correctly: create the process **SUSPENDED**, assign it
/// to a KILL_ON_JOB_CLOSE Job, *then* resume — so every child is born inside the Job and a
/// launcher that exits and hands off (Chromium/Electron) cannot escape it. (Assigning after a
/// normal spawn races that handoff — the previous version left 15 Edge children alive.)
pub fn killtree(cmd: &[String]) -> anyhow::Result<()> {
    anyhow::ensure!(!cmd.is_empty(), "need a command to --spawn");
    let child = std::process::Command::new(&cmd[0])
        .args(&cmd[1..])
        .creation_flags(CREATE_SUSPENDED.0)
        .spawn()?;
    let root = child.id();
    println!("spawned pid {root} SUSPENDED; putting it in the Job before any child can spawn...");

    // SAFETY: each FFI call is checked and handles are closed. The process is suspended, so it
    // has created no children yet — assigning it now captures the entire future tree.
    let job = unsafe {
        let job = CreateJobObjectW(None, windows::core::PCWSTR::null())?;
        let info = JOBOBJECT_EXTENDED_LIMIT_INFORMATION {
            BasicLimitInformation: JOBOBJECT_BASIC_LIMIT_INFORMATION {
                LimitFlags: JOB_OBJECT_LIMIT_KILL_ON_JOB_CLOSE,
                ..Default::default()
            },
            ..Default::default()
        };
        SetInformationJobObject(
            job,
            JobObjectExtendedLimitInformation,
            &info as *const _ as *const c_void,
            std::mem::size_of::<JOBOBJECT_EXTENDED_LIMIT_INFORMATION>() as u32,
        )?;
        let proc = OpenProcess(
            PROCESS_SET_QUOTA | PROCESS_TERMINATE | PROCESS_QUERY_INFORMATION,
            false,
            root,
        )?;
        AssignProcessToJobObject(job, proc)?;
        let _ = CloseHandle(proc);
        job
    };

    resume_process(root);
    println!("resumed; waiting 3s for it to build its process tree...");
    std::thread::sleep(Duration::from_secs(3));

    let before = descendants(root);
    println!("process tree before teardown ({} processes):", before.len());
    for p in &before {
        println!("  pid {:>6}  ppid {:>6}  {}", p.pid, p.ppid, p.exe);
    }

    println!("\nclosing the job handle (KILL_ON_JOB_CLOSE)...");
    unsafe { CloseHandle(job)? }; // closing the last handle terminates the whole job tree

    std::thread::sleep(Duration::from_secs(1));
    let after = descendants(root);
    let survivors: Vec<&Proc> = after
        .iter()
        .filter(|p| p.pid == root || before.iter().any(|b| b.pid == p.pid))
        .collect();

    println!();
    if survivors.is_empty() {
        println!("PASS: every process in the tree is gone after the job closed.");
    } else {
        println!("FAIL: {} process(es) survived the job close (possible breakaway):", survivors.len());
        for p in survivors {
            println!("  pid {:>6}  {}", p.pid, p.exe);
        }
    }
    Ok(())
}

/// Resume every thread of `pid` — a CREATE_SUSPENDED process has one suspended main thread.
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
