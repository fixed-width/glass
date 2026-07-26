//! Linux process-tree introspection via `/proc`.
//!
//! A small, backend-agnostic utility shared by the Linux display backends
//! (`glass-x11`, `glass-wayland`): given the pid glass spawned, enumerate that
//! process **and all its descendants**. Both backends need this because the
//! process they spawn is frequently *not* the app — it's a wrapper (a `bwrap`
//! sandbox, sway's `exec`, a shell launcher), and the real app is a descendant
//! with a different pid. The full set is used to correlate windows
//! (`_NET_WM_PID`) and the accessibility tree (the AT-SPI connection pid) back
//! to the launch.
//!
//! This is deliberately *not* in `glass-core` (it is OS-specific `/proc` I/O,
//! which belongs behind the `Platform` seam, not in the portable core) nor in
//! the sandbox crate (it is generic process introspection, unrelated to
//! bubblewrap). The Windows peer (`descendant_pids`, Toolhelp-based) lives with
//! the Windows backend for the same reason — the OS APIs can't share an impl.

use std::collections::{HashMap, HashSet, VecDeque};
use std::io::{BufRead, BufReader, Read};
use std::process::Child;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

/// Grace period a process gets to exit after SIGTERM before SIGKILL.
pub const REAP_GRACE: Duration = Duration::from_secs(2);

/// How long an app that was *asked* to close gets to leave through its own shutdown path
/// before glass falls back to signalling it.
///
/// Signals give a GUI toolkit no shutdown path at all: GTK and Qt install no `SIGTERM`
/// handler, so a signalled app never runs its close/shutdown handlers — measured with a GTK 4
/// client on both Linux backends. Asking first (an X11 `WM_DELETE_WINDOW` client message, or
/// `kill` on the compositor's container under Wayland) is what lets the app flush its state.
pub const CLOSE_GRACE: Duration = Duration::from_millis(1500);

/// SIGTERM→SIGKILL grace for an app that was already asked to close and did not.
///
/// Shorter than [`REAP_GRACE`] on purpose: this runs *after* [`CLOSE_GRACE`] is already spent,
/// and the two together have to fit in the budget glass-mcp gives teardown as a whole. An app
/// that ignored the close request has had its chance to shut down cleanly; what is left is
/// making sure it is gone.
pub const APP_REAP_GRACE: Duration = Duration::from_millis(1000);

/// How a launched app actually went away, so the backend can say so rather than reporting
/// every teardown as an unqualified success. Signalling an app that was never asked destroys
/// whatever it would have flushed on exit, and the user only learns of it the next time the
/// app starts up in a recovered/crashed state.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Closed {
    /// Asked, and the app left through its own shutdown path.
    Gracefully,
    /// Nothing was asked, so the app was signalled without a chance to shut down. Carries how
    /// many of its windows were there but could not be asked — an X11 client that never opted
    /// into `WM_DELETE_WINDOW`, say — which is a different situation from an app that had no
    /// window to ask (a teardown after a failed launch, or a windowless process).
    SignalledUnasked { unaskable: usize },
    /// Asked, but still running when [`CLOSE_GRACE`] ran out (a modal save prompt, or a hang),
    /// so it was signalled anyway.
    SignalledAfterGrace,
}

/// Classify a teardown from how many of the app's windows were asked to close, how many were
/// there but could not be asked, and whether the app then exited on its own. Pure so it can be
/// tested off a display.
pub fn close_outcome(asked: usize, unaskable: usize, exited: bool) -> Closed {
    match (asked, exited) {
        (0, _) => Closed::SignalledUnasked { unaskable },
        (_, true) => Closed::Gracefully,
        (_, false) => Closed::SignalledAfterGrace,
    }
}

/// Report a teardown that could not be graceful. Silent on [`Closed::Gracefully`] — the good
/// path is the expectation, not news — and on an app with no window to ask, which has no
/// shutdown path to have missed (this also runs on the failed-launch path, where a warning
/// would be pure noise). The rest is something the user can act on, and would otherwise surface
/// only as an unexplained recovery prompt the *next* time the app is launched.
pub fn disclose_teardown(closed: Closed) {
    match closed {
        Closed::Gracefully | Closed::SignalledUnasked { unaskable: 0 } => {}
        Closed::SignalledUnasked { unaskable } => eprintln!(
            "glass: {unaskable} window(s) of the app could not be asked to close, so it was \
             signalled instead. An X11 client that never opted into the WM_DELETE_WINDOW \
             protocol cannot be asked; toolkit apps opt in. An app that flushes state on exit \
             did not get to, and may report a crash on its next launch."
        ),
        Closed::SignalledAfterGrace => eprintln!(
            "glass: the app did not close within {CLOSE_GRACE:?} of being asked (a modal \
             shutdown prompt will do this), so it was signalled. Unsaved state was not flushed, \
             and the app may report a crash on its next launch."
        ),
    }
}

/// Poll `done` until it reports true or `grace` is spent. Returns whether it reported true.
///
/// The polling interval matches [`reap`]'s, and `done` is called once before any sleep so an
/// app that has already gone costs nothing.
pub fn await_condition(grace: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
}

/// Wait up to `grace` for the child to exit on its own, WITHOUT signalling it. Returns whether
/// it did; the caller still has to `wait()` (or reap) it. Used after asking an app to close: an
/// app that leaves through its own shutdown path must not then be signalled for it.
///
/// A `try_wait` error counts as "did not exit", not as an exit: the caller's fallback is the
/// signal ladder, which is what actually guarantees the process is gone, and skipping it on a
/// reading we could not make would leave the app running.
pub fn await_exit(child: &mut Child, grace: Duration) -> bool {
    await_condition(grace, || matches!(child.try_wait(), Ok(Some(_))))
}

/// Gracefully reap a single child: SIGTERM, poll for exit up to `grace`, then
/// SIGKILL as a last resort, then `wait()`. SIGTERM-first lets the process clean
/// up its own children, sockets, and locks; SIGKILL is the escape hatch only.
pub fn reap_graceful(child: &mut Child, grace: Duration) {
    reap(child, grace, false);
}

/// Like [`reap_graceful`] but signals the child's whole process GROUP, so a
/// group leader's descendants are reaped too. The child MUST be a group leader
/// (spawned with `std::os::unix::process::CommandExt::process_group(0)`).
pub fn reap_group(child: &mut Child, grace: Duration) {
    reap(child, grace, true);
}

fn reap(child: &mut Child, grace: Duration, group: bool) {
    if let Some(pid) = Pid::from_raw(child.id() as i32) {
        let _ = if group {
            kill_process_group(pid, Signal::TERM)
        } else {
            kill_process(pid, Signal::TERM)
        };
        let deadline = Instant::now() + grace;
        loop {
            match child.try_wait() {
                Ok(Some(_)) => break,
                Ok(None) if Instant::now() >= deadline => {
                    if group {
                        let _ = kill_process_group(pid, Signal::KILL);
                    } else {
                        let _ = child.kill();
                    }
                    break;
                }
                Ok(None) => std::thread::sleep(Duration::from_millis(20)),
                Err(_) => break,
            }
        }
    }
    let _ = child.wait();
}

/// The pid `root_pid` plus every descendant process, read from `/proc`.
///
/// Returns `[root_pid]` if `/proc` is unavailable, and just `[root_pid]` if it
/// has no children yet (callers poll in a loop, so an empty subtree simply
/// means "retry"). Cycle-safe even if PID reuse mid-scan produces a bogus
/// parent cycle (see [`collect_descendants`]).
pub fn proc_tree_pids(root_pid: u32) -> Vec<u32> {
    // Read all (pid → ppid) pairs from /proc.
    let proc = match std::fs::read_dir("/proc") {
        Ok(d) => d,
        Err(_) => return vec![root_pid],
    };
    let mut parent_of: HashMap<u32, u32> = HashMap::new();
    for entry in proc.flatten() {
        let name = entry.file_name();
        let pid_str = name.to_string_lossy();
        let Ok(pid) = pid_str.parse::<u32>() else {
            continue;
        };
        let status_path = format!("/proc/{pid}/status");
        let Ok(content) = std::fs::read_to_string(&status_path) else {
            continue;
        };
        for line in content.lines() {
            if let Some(rest) = line.strip_prefix("PPid:") {
                if let Ok(ppid) = rest.trim().parse::<u32>() {
                    parent_of.insert(pid, ppid);
                }
                break;
            }
        }
    }
    collect_descendants(root_pid, &parent_of)
}

/// Collect `root` and all its descendants given a child→parent-pid map.
/// Cycle-safe (a `seen` set guarantees termination even if the map contains a
/// cycle, e.g. from PID reuse mid-scan).
fn collect_descendants(root: u32, parent_of: &HashMap<u32, u32>) -> Vec<u32> {
    let mut seen: HashSet<u32> = HashSet::new();
    let mut out = Vec::new();
    let mut q = VecDeque::from([root]);
    while let Some(pid) = q.pop_front() {
        if !seen.insert(pid) {
            continue;
        }
        out.push(pid);
        for (&child, &ppid) in parent_of {
            if ppid == pid && !seen.contains(&child) {
                q.push_back(child);
            }
        }
    }
    out
}

/// Drain a child's piped output line-by-line into a shared buffer on a background
/// thread, tagging each line with `tag`. The X11 and Wayland backends both spawn an
/// app and capture its stdout/stderr this way; sharing it here keeps them from
/// re-implementing the reader. Generic over the tag so this crate needn't depend on
/// glass-core's `Stream` enum.
pub fn spawn_reader<S: Copy + Send + 'static, R: Read + Send + 'static>(
    reader: R,
    tag: S,
    sink: Arc<Mutex<Vec<(S, String)>>>,
) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(text) => sink.lock().expect("log sink mutex").push((tag, text)),
                Err(_) => break,
            }
        }
    });
}

#[cfg(test)]
mod reap_tests {
    use super::{REAP_GRACE, reap_graceful, reap_group};
    use std::io::{BufRead, BufReader};
    use std::os::unix::process::CommandExt;
    use std::process::{Command, Stdio};
    use std::time::{Duration, Instant};

    fn alive(pid: u32) -> bool {
        std::path::Path::new(&format!("/proc/{pid}")).exists()
    }

    #[test]
    fn reap_graceful_exits_fast_when_sigterm_is_honored() {
        // `sleep` terminates on SIGTERM via its DEFAULT disposition — immediate, not deferred —
        // so it honors the graceful SIGTERM at once, no trap or ready-barrier needed. NB: a shell
        // `trap 'exit 0' TERM; sleep 30` does NOT work here: the shell defers the trap action
        // until the foreground `sleep` returns, so it rides out the whole grace and gets SIGKILLed
        // (the earlier version only passed by racing SIGTERM in ahead of the trap install).
        let mut c = Command::new("sleep").arg("30").spawn().unwrap();
        let t = Instant::now();
        reap_graceful(&mut c, Duration::from_secs(5));
        assert!(
            t.elapsed() < Duration::from_secs(2),
            "a process that terminates on SIGTERM should be reaped promptly, not ride out the grace"
        );
    }

    #[test]
    fn reap_graceful_sigkills_after_grace_when_sigterm_ignored() {
        // Echo "ready" after the trap is installed so we don't race the signal.
        let mut c = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; sleep 30"])
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(c.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "ready");
        let grace = Duration::from_millis(300);
        let t = Instant::now();
        reap_graceful(&mut c, grace);
        let el = t.elapsed();
        assert!(
            el >= grace,
            "should wait the full grace before SIGKILL (waited {el:?})"
        );
        assert!(
            el < grace + Duration::from_secs(2),
            "but must not hang (waited {el:?})"
        );
    }

    #[test]
    fn reap_group_reaps_a_forked_grandchild() {
        let mut leader = Command::new("sh")
            .args(["-c", "sleep 30 & echo $!; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let grandchild: u32 = line.trim().parse().expect("grandchild pid");
        assert!(
            alive(grandchild),
            "grandchild should be alive before reaping"
        );
        reap_group(&mut leader, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !alive(grandchild),
            "reap_group must reap the forked grandchild, not orphan it"
        );
    }

    #[test]
    fn grace_constant_is_two_seconds() {
        assert_eq!(REAP_GRACE, Duration::from_secs(2));
    }

    #[test]
    fn await_exit_reports_a_child_that_leaves_on_its_own() {
        let mut c = Command::new("true").spawn().unwrap();
        assert!(
            super::await_exit(&mut c, Duration::from_secs(5)),
            "a child that exits by itself must be reported as exited"
        );
        let _ = c.wait();
    }

    #[test]
    fn await_exit_does_not_signal_the_child_it_waits_for() {
        // The whole point of the wait: an app that was ASKED to close must be left alone to run
        // its own shutdown path. If await_exit signalled, this would return true and the pid
        // would be gone.
        let mut c = Command::new("sleep").arg("30").spawn().unwrap();
        let grace = Duration::from_millis(200);
        assert!(
            !super::await_exit(&mut c, grace),
            "a still-running child must be reported as not exited"
        );
        assert!(alive(c.id()), "await_exit must not signal the child");
        let _ = c.kill();
        let _ = c.wait();
    }

    #[test]
    fn await_condition_returns_before_the_grace_when_already_done() {
        let t = Instant::now();
        assert!(super::await_condition(Duration::from_secs(30), || true));
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "a condition already met must not wait out the grace"
        );
    }

    #[test]
    fn await_condition_gives_up_after_the_grace() {
        let grace = Duration::from_millis(200);
        let t = Instant::now();
        assert!(!super::await_condition(grace, || false));
        assert!(
            t.elapsed() >= grace,
            "must wait the full grace before giving up"
        );
    }

    #[test]
    fn an_app_that_was_never_asked_is_reported_as_signalled_unasked() {
        assert_eq!(
            super::close_outcome(0, 2, false),
            super::Closed::SignalledUnasked { unaskable: 2 }
        );
        // Even if the app happened to exit on its own, nothing asked it to — the caller has no
        // basis for reporting a graceful close.
        assert_eq!(
            super::close_outcome(0, 2, true),
            super::Closed::SignalledUnasked { unaskable: 2 }
        );
    }

    #[test]
    fn an_asked_app_that_exits_is_reported_as_graceful() {
        assert_eq!(super::close_outcome(1, 0, true), super::Closed::Gracefully);
    }

    #[test]
    fn an_asked_app_that_stays_is_reported_as_signalled_after_the_grace() {
        assert_eq!(
            super::close_outcome(1, 0, false),
            super::Closed::SignalledAfterGrace
        );
    }

    #[test]
    fn an_app_with_no_window_to_ask_is_distinguishable_from_one_that_refused_the_ask() {
        // Teardown also runs on the failed-launch path, where there is no window and nothing was
        // missed. `disclose_teardown` stays silent on that, so the two must not collapse into one
        // outcome.
        assert_eq!(
            super::close_outcome(0, 0, false),
            super::Closed::SignalledUnasked { unaskable: 0 }
        );
        assert_ne!(
            super::close_outcome(0, 0, false),
            super::close_outcome(0, 1, false)
        );
    }

    #[test]
    fn the_close_and_signal_graces_leave_the_teardown_budget_headroom() {
        // The backends bind these to glass_core::TEARDOWN_BUDGET at compile time; this crate
        // has no glass-core dependency, so assert the shape they rely on: the ask gets the
        // larger share (it is the one that produces a clean shutdown) and the pair stays well
        // under the 3s budget.
        assert!(super::CLOSE_GRACE > super::APP_REAP_GRACE);
        assert!(super::CLOSE_GRACE + super::APP_REAP_GRACE < Duration::from_secs(3));
    }
}

#[cfg(test)]
mod tests {
    use super::{collect_descendants, proc_tree_pids};
    use std::collections::HashMap;

    #[test]
    fn descendants_normal_tree() {
        // root 100 → children 200, 201; 200 → child 300
        let mut parent_of = HashMap::new();
        parent_of.insert(200u32, 100u32);
        parent_of.insert(201u32, 100u32);
        parent_of.insert(300u32, 200u32);
        let mut result = collect_descendants(100, &parent_of);
        result.sort();
        assert_eq!(result, vec![100, 200, 201, 300]);
    }

    #[test]
    fn descendants_cycle_terminates() {
        // Cycle: parent_of[100] = 200, parent_of[200] = 100
        // (simulates PID-reuse creating a bogus cycle in the map mid-scan)
        let mut parent_of = HashMap::new();
        parent_of.insert(100u32, 200u32);
        parent_of.insert(200u32, 100u32);
        // Must terminate and include the root.
        let result = collect_descendants(100, &parent_of);
        assert!(
            result.contains(&100),
            "root must be present even with a cycle"
        );
        assert!(result.len() <= 2, "cycle must not cause unbounded growth");
    }

    #[test]
    fn descendants_root_only() {
        let parent_of: HashMap<u32, u32> = HashMap::new();
        assert_eq!(collect_descendants(42, &parent_of), vec![42]);
    }

    #[test]
    fn proc_tree_pids_includes_a_real_descendant() {
        // For a wrapped launch (bwrap / sway exec / shell) the spawned child is
        // the wrapper and the real app is a *descendant* with a different pid;
        // proc_tree_pids must walk down to it (a plain `[child_pid]` would not).
        use std::process::{Command, Stdio};
        let mut child = Command::new("sleep")
            .arg("30")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let child_pid = child.id();
        let pids = proc_tree_pids(std::process::id());
        let _ = child.kill();
        let _ = child.wait();
        assert!(
            pids.contains(&std::process::id()),
            "must include the root pid"
        );
        assert!(
            pids.contains(&child_pid),
            "must include the spawned descendant pid {child_pid}; got {pids:?}"
        );
    }
}
