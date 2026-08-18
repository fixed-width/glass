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

#![cfg(target_os = "linux")]

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
/// A signalled toolkit app runs no shutdown path at all, because the toolkit installs no
/// `SIGTERM` handler and the default disposition kills the process outright: verified with GTK 4
/// on both Linux backends, where a signalled client recorded nothing on the way out and an asked
/// one ran its close and shutdown handlers (Qt is believed to behave the same way; that was not
/// tested). Asking first — an X11 `WM_DELETE_WINDOW` client message, or a close request through
/// the compositor under Wayland — is what lets the app flush its state.
///
/// The value also has to leave room for the signal ladder inside the budget teardown gets as a
/// whole; each backend asserts that against `glass_core::TEARDOWN_BUDGET` at compile time.
pub const CLOSE_GRACE: Duration = Duration::from_millis(1500);

/// SIGTERM→SIGKILL grace for an app that was already asked to close and did not.
///
/// Shorter than [`REAP_GRACE`] on purpose: this runs *after* [`CLOSE_GRACE`] is already spent,
/// and the two together have to fit in the budget glass-mcp gives teardown as a whole. An app
/// that ignored the close request has had its chance to shut down cleanly; what is left is
/// making sure it is gone.
pub const APP_REAP_GRACE: Duration = Duration::from_millis(1000);

/// How long [`reap_launch`] waits for a signalled process to actually leave before naming it a
/// survivor.
///
/// A signal lands when its target is next scheduled, so a `/proc` check taken straight after one
/// still sees pids that are already dying. Small because it is spent only where something is
/// still there — a launch that went away is confirmed gone on the first look — and because each
/// backend counts it into the ladder it asserts against `glass_core::TEARDOWN_BUDGET`.
pub const KILL_CONFIRM: Duration = Duration::from_millis(50);

/// How a launched app actually went away, so the backend can say so rather than reporting
/// every teardown as an unqualified success. Signalling an app that was never asked destroys
/// whatever it would have flushed on exit, and the user only learns of it the next time the
/// app starts up in a recovered/crashed state.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closed {
    /// Asked, and the app left on its own before the grace ran out.
    Gracefully,
    /// Nothing was asked, so the app was signalled without a chance to shut down. `reason` says
    /// why when there is something to say — windows that were there but could not be asked, or
    /// an enumeration that failed. `None` means there was simply no window to ask (a teardown
    /// after a failed launch, or a windowless process), which is not worth reporting.
    SignalledUnasked { reason: Option<String> },
    /// Asked, but still running when [`CLOSE_GRACE`] ran out (a modal save prompt, or a hang),
    /// so it was signalled anyway.
    SignalledAfterGrace,
}

/// The result of asking an app's windows to close: how many the request reached, and why the
/// rest could not be asked. Backends build one of these and hand it to [`Asked::outcome`], so
/// the counts are never two bare numbers a caller could swap.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Asked {
    asked: usize,
    reason: Option<String>,
}

impl Asked {
    /// Nothing to ask: the app has no window. Reported as nothing at all — there was no
    /// shutdown path to miss.
    pub const fn none() -> Asked {
        Asked {
            asked: 0,
            reason: None,
        }
    }

    /// `total` windows were found and `asked` of them accepted a close request. `why_not`
    /// describes the rest in the backend's own terms — the caller knows whether they lacked a
    /// protocol, sat outside the launched process tree, or were refused by a compositor.
    pub fn counted(total: usize, asked: usize, why_not: impl FnOnce(usize) -> String) -> Asked {
        let unaskable = total.saturating_sub(asked);
        Asked {
            asked,
            reason: (unaskable > 0).then(|| why_not(unaskable)),
        }
    }

    /// glass could not find out what to ask — the window enumeration itself failed. Distinct
    /// from [`Asked::none`] on purpose: "the app has no window" is routine, "glass could not
    /// look" is a failure of glass's own machinery and must not pass for it.
    pub fn blocked(reason: impl Into<String>) -> Asked {
        Asked {
            asked: 0,
            reason: Some(reason.into()),
        }
    }

    /// Whether anything was actually asked to close.
    pub fn any(&self) -> bool {
        self.asked > 0
    }

    /// Wait for the app to leave on its own — but only if something was asked. With nothing
    /// asked there is no shutdown to wait for, so the grace is skipped rather than spent.
    pub fn await_close(&self, grace: Duration, done: impl FnMut() -> bool) -> bool {
        self.any() && await_condition(grace, done)
    }

    /// [`Asked::await_close`] for the common case: wait for the spawned child itself to exit.
    ///
    /// The wait NEVER signals — that is the whole point, an app asked to close must be left
    /// alone to run its shutdown path. A `try_wait` error counts as "did not exit", not as an
    /// exit: the caller's fallback is the signal ladder, which is what actually guarantees the
    /// process is gone, and skipping it on a reading we could not make would leave the app
    /// running.
    pub fn await_child_exit(&self, child: &mut Child, grace: Duration) -> bool {
        self.await_close(grace, || matches!(child.try_wait(), Ok(Some(_))))
    }

    /// Classify the teardown: `closed_itself` is whether the app left before the grace ran out.
    pub fn outcome(self, closed_itself: bool) -> Closed {
        match (self.asked, closed_itself) {
            (0, _) => Closed::SignalledUnasked {
                reason: self.reason,
            },
            (_, true) => Closed::Gracefully,
            (_, false) => Closed::SignalledAfterGrace,
        }
    }
}

/// What the user should be told about a teardown, or `None` when there is nothing to say.
///
/// Silent on [`Closed::Gracefully`] — the good path is the expectation, not news — and on an app
/// with no window to ask, which had no shutdown path to miss (that arm also covers the
/// failed-launch teardown, where a warning would be pure noise). Split from the printing so the
/// wording is testable; [`disclose_teardown`] is the printing half.
pub fn teardown_notice(closed: &Closed) -> Option<String> {
    match closed {
        Closed::Gracefully => None,
        Closed::SignalledUnasked { reason } => reason.as_ref().map(|why| {
            format!(
                "glass: the app was signalled instead of being asked to close ({why}). An app \
                 that flushes state on exit did not get to, and may report a crash on its next \
                 launch."
            )
        }),
        Closed::SignalledAfterGrace => Some(format!(
            "glass: the app did not close within {CLOSE_GRACE:?} of being asked (a modal \
             shutdown prompt, or an app that ignores close requests, will do this), so it was \
             signalled. Unsaved state was not flushed, and the app may report a crash on its \
             next launch."
        )),
    }
}

/// Print [`teardown_notice`] to stderr (stdout is the MCP channel).
pub fn disclose_teardown(closed: &Closed) {
    if let Some(notice) = teardown_notice(closed) {
        eprintln!("{notice}");
    }
}

/// How often the waits and the signal ladder re-check. One constant so a change here cannot
/// leave a doc comment elsewhere claiming they match.
const POLL_INTERVAL: Duration = Duration::from_millis(20);

/// Poll `done` until it reports true or `grace` is spent. Returns whether it reported true.
///
/// `done` is called once before any sleep, so an app that has already gone costs nothing. Note
/// the deadline is only checked *between* calls: `done` must not block for longer than the
/// caller is willing to wait.
pub fn await_condition(grace: Duration, mut done: impl FnMut() -> bool) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        if done() {
            return true;
        }
        if Instant::now() >= deadline {
            return false;
        }
        std::thread::sleep(POLL_INTERVAL);
    }
}

/// Which of `pids` are still live processes — [`any_alive`]'s answer, named.
///
/// A caller that reports a teardown needs the list rather than the bool: an operator can kill a
/// pid, and cannot act on "something survived".
pub fn still_alive(pids: &[u32]) -> Vec<u32> {
    pids.iter()
        .copied()
        .filter(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
        .collect()
}

/// Whether any of `pids` is still a live process.
pub fn any_alive(pids: &[u32]) -> bool {
    pids.iter()
        .any(|pid| std::path::Path::new(&format!("/proc/{pid}")).exists())
}

/// Reap a whole launch: the child glass spawned, its process group, and every pid in `tree` — a
/// snapshot of the launch's `/proc` subtree taken *before* any of it exited. SIGTERM everything,
/// wait up to `grace` for it all to go, then SIGKILL whatever is left, then reap the child.
///
/// The snapshot is load-bearing, because neither of the other two handles is enough on its own:
///
/// - The parent link goes away with the parent. Once the child is reaped its descendants are
///   reparented to init, so a later `proc_tree_pids` no longer finds them.
/// - The process group does not contain everything the launch produced. sway calls `setsid` for
///   every app it `exec`s, so under the Wayland backend the app is in neither the compositor's
///   group nor its session — measured: after a SIGTERM to sway's group, the exec'd tree was
///   still running, reparented to init.
///
/// A pid is only signalled while `/proc/<pid>` still exists. That check races pid reuse the same
/// way every `kill`-based reaper on Linux does; the alternative is leaking the app's children on
/// every teardown.
///
/// Returns the pids still running [`KILL_CONFIRM`] after the ladder ran — empty on the ordinary
/// path. A caller that reports the teardown must report these too, or it claims a stop that did
/// not happen (glass#380).
#[must_use = "a launch that outlived its reap is what the caller has to report"]
pub fn reap_launch(child: &mut Child, tree: &[u32], grace: Duration) -> Vec<u32> {
    let leader = Pid::from_raw(child.id() as i32);
    let signal_all = |signal| {
        if let Some(leader) = leader {
            let _ = kill_process_group(leader, signal);
        }
        for &pid in tree {
            if std::path::Path::new(&format!("/proc/{pid}")).exists()
                && let Some(pid) = Pid::from_raw(pid as i32)
            {
                let _ = kill_process(pid, signal);
            }
        }
    };
    signal_all(Signal::TERM);
    let gone = await_condition(grace, || {
        matches!(child.try_wait(), Ok(Some(_))) && !any_alive(tree)
    });
    if !gone {
        signal_all(Signal::KILL);
        let _ = child.kill();
    }
    let _ = child.wait();
    // Asked after the wait, because until the child is reaped its own zombie is still in /proc.
    await_condition(KILL_CONFIRM, || !any_alive(tree));
    still_alive(tree)
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
                Ok(None) => std::thread::sleep(POLL_INTERVAL),
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
    use super::{
        Asked, CLOSE_GRACE, Closed, REAP_GRACE, reap_graceful, reap_group, teardown_notice,
    };
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
        // Echo "ready" after the trap is installed so we don't race the signal. `exec` keeps the
        // TERM-ignoring process the direct child — SIG_IGN survives execve. Drop it and `sleep` is
        // a grandchild `reap_graceful` never signals, outliving the test on the inherited stderr
        // pipe, which nextest reports as LEAK.
        let mut c = Command::new("sh")
            .args(["-c", "trap '' TERM; echo ready; exec sleep 30"])
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

    /// An `Asked` that stands for "one window accepted the close request", for the waits below.
    fn one_window_asked() -> Asked {
        Asked::counted(1, 1, |_| unreachable!("nothing was left unasked"))
    }

    #[test]
    fn await_child_exit_reports_a_child_that_leaves_on_its_own() {
        let mut c = Command::new("true").spawn().unwrap();
        assert!(
            one_window_asked().await_child_exit(&mut c, Duration::from_secs(5)),
            "a child that exits by itself must be reported as exited"
        );
        let _ = c.wait();
    }

    #[test]
    fn await_child_exit_does_not_signal_the_child_it_waits_for() {
        // The whole point of the wait: an app that was ASKED to close must be left alone to run
        // its own shutdown path. If the wait signalled, this would return true and the pid would
        // be gone.
        let mut c = Command::new("sleep").arg("30").spawn().unwrap();
        let grace = Duration::from_millis(200);
        assert!(
            !one_window_asked().await_child_exit(&mut c, grace),
            "a still-running child must be reported as not exited"
        );
        assert!(alive(c.id()), "the wait must not signal the child");
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
        let asked = Asked::counted(2, 0, |n| format!("{n} cannot be asked"));
        assert_eq!(
            asked.clone().outcome(false),
            Closed::SignalledUnasked {
                reason: Some("2 cannot be asked".into())
            }
        );
        // Even if the app happened to exit on its own, nothing asked it to — the caller has no
        // basis for reporting a graceful close.
        assert_eq!(
            asked.outcome(true),
            Closed::SignalledUnasked {
                reason: Some("2 cannot be asked".into())
            }
        );
    }

    #[test]
    fn an_asked_app_that_exits_is_reported_as_graceful() {
        let asked = Asked::counted(1, 1, |_| unreachable!("nothing was left unasked"));
        assert_eq!(asked.outcome(true), Closed::Gracefully);
    }

    #[test]
    fn an_asked_app_that_stays_is_reported_as_signalled_after_the_grace() {
        let asked = Asked::counted(1, 1, |_| unreachable!("nothing was left unasked"));
        assert_eq!(asked.outcome(false), Closed::SignalledAfterGrace);
    }

    #[test]
    fn an_app_with_no_window_to_ask_says_nothing() {
        // Teardown also runs on the failed-launch path, where there is no window and nothing was
        // missed. That must stay silent, while a window glass could not ask must not.
        assert_eq!(teardown_notice(&Asked::none().outcome(false)), None);
        assert!(
            teardown_notice(&Asked::counted(1, 0, |n| format!("{n} unaskable")).outcome(false))
                .is_some()
        );
    }

    #[test]
    fn an_enumeration_failure_is_reported_rather_than_read_as_nothing_to_ask() {
        // The failure mode this guards: glass's own machinery breaks (an unreachable display or
        // compositor), nothing is asked, the app is signalled — and the user is told nothing,
        // because it looks exactly like an app that had no window.
        let notice = teardown_notice(&Asked::blocked("the compositor went away").outcome(false))
            .expect("a blocked ask must be disclosed");
        assert!(
            notice.contains("the compositor went away"),
            "the reason must reach the user: {notice}"
        );
    }

    #[test]
    fn nothing_asked_means_the_close_grace_is_not_spent() {
        // An app that could not be asked has no shutdown to wait for; spending the grace on it
        // would add over a second to every such teardown.
        let t = Instant::now();
        assert!(!Asked::none().await_close(Duration::from_secs(30), || false));
        assert!(
            t.elapsed() < Duration::from_secs(1),
            "waited {:?} for a close nobody was asked to perform",
            t.elapsed()
        );
    }

    #[test]
    fn a_graceful_close_is_not_worth_reporting() {
        assert_eq!(teardown_notice(&Closed::Gracefully), None);
    }

    #[test]
    fn an_app_that_ignored_the_request_is_reported_with_the_grace_it_was_given() {
        let notice = teardown_notice(&Closed::SignalledAfterGrace).expect("must be disclosed");
        assert!(
            notice.contains(&format!("{CLOSE_GRACE:?}")),
            "the notice should say how long the app was given: {notice}"
        );
    }

    #[test]
    fn the_close_grace_gets_the_larger_share_of_the_ladder() {
        // The ask is the half that produces a clean shutdown, so it gets the longer wait; the
        // signal ladder only has to make sure the app is gone. The budget the pair has to fit
        // inside is `glass_core::TEARDOWN_BUDGET`, which this crate deliberately cannot see —
        // each backend binds them to it with a compile-time assertion.
        assert!(super::CLOSE_GRACE > super::APP_REAP_GRACE);
    }

    /// glass#380: the reaper computed whether the launch went away and discarded the answer, so
    /// its callers could only report the stop they asked for. A list rather than a bool because
    /// naming a survivor is what an operator can act on.
    #[test]
    fn still_alive_names_the_processes_that_are_there() {
        let mut child = Command::new("sleep").arg("5").spawn().expect("spawn sleep");
        let pid = child.id();
        assert_eq!(super::still_alive(&[pid]), vec![pid]);
        child.kill().expect("kill");
        child.wait().expect("wait");
        assert!(
            super::still_alive(&[pid]).is_empty(),
            "a child that was killed and reaped is not alive"
        );
    }

    #[test]
    fn reap_launch_reaps_a_child_that_left_the_process_group() {
        // sway does exactly this to every app it execs: `setsid` puts the app in its own group
        // and session, where a signal to the launcher's group never reaches it. Reaping the
        // snapshot is what covers that.
        let mut leader = Command::new("sh")
            .args(["-c", "setsid sleep 30 & echo started; wait"])
            .process_group(0)
            .stdout(Stdio::piped())
            .spawn()
            .unwrap();
        let mut line = String::new();
        BufReader::new(leader.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        assert_eq!(line.trim(), "started");
        std::thread::sleep(Duration::from_millis(100)); // let setsid re-exec into its own session
        let tree = super::proc_tree_pids(leader.id());
        let escaped: Vec<u32> = tree.iter().copied().filter(|&p| p != leader.id()).collect();
        assert!(
            !escaped.is_empty(),
            "the launch should have a child to reap"
        );
        let left = super::reap_launch(&mut leader, &tree, Duration::from_secs(5));
        assert!(
            left.is_empty(),
            "a launch that was reaped has no survivors to report: {left:?}"
        );
        std::thread::sleep(Duration::from_millis(100));
        assert!(
            !super::any_alive(&escaped),
            "reap_launch must reach every process the launch produced, group or no group: \
             {escaped:?} survived"
        );
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
