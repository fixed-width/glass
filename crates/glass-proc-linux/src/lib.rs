//! Linux process-tree introspection via `/proc`.
//!
//! Tracks Linux process trees for display backends.
//!
//! It correlates descendants to launched apps, reaps them, and tails bounded stderr.

#![cfg(target_os = "linux")]

mod stderr;

pub use stderr::{STDERR_KEPT, StderrTail};

use std::collections::{HashMap, HashSet, VecDeque};
use std::process::Child;
use std::time::{Duration, Instant};

use rustix::process::{Pid, Signal, kill_process, kill_process_group};

/// Grace period a process gets to exit after SIGTERM before SIGKILL.
pub const REAP_GRACE: Duration = Duration::from_secs(2);

/// Grace period after a close request lets an app flush state before signaling within the teardown budget.
pub const CLOSE_GRACE: Duration = Duration::from_millis(1500);

/// SIGTERM-to-SIGKILL grace after [`CLOSE_GRACE`] expires.
pub const APP_REAP_GRACE: Duration = Duration::from_millis(1000);

/// How a launched app exited.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Closed {
    /// Asked, and the app left on its own before the grace ran out.
    Gracefully,
    /// Signaled without a close request and `reason` describes unaskable windows or failed enumeration.
    SignalledUnasked { reason: Option<String> },
    /// Signaled after [`CLOSE_GRACE`] expired.
    SignalledAfterGrace,
}

/// The number of windows asked to close and why others could not be asked.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct Asked {
    asked: usize,
    reason: Option<String>,
}

impl Asked {
    /// No windows were available to close.
    pub const fn none() -> Asked {
        Asked {
            asked: 0,
            reason: None,
        }
    }

    /// Records `asked` close requests out of `total` and `why_not` describes the rest.
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

/// Whether `pid` is a live process.
///
/// A zombie is not: it keeps a `/proc` entry until its parent reaps it, and holds nothing. Its
/// state is field 3 of `/proc/<pid>/stat`, read after the last `)` because the comm field before
/// it can itself contain spaces and parens.
fn alive(pid: u32) -> bool {
    if !std::path::Path::new(&format!("/proc/{pid}")).exists() {
        return false;
    }
    let Ok(stat) = std::fs::read_to_string(format!("/proc/{pid}/stat")) else {
        // Reported as alive: a caller looking twice costs less than a survivor missed.
        return true;
    };
    !stat
        .rsplit_once(')')
        .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z'))
}

/// Which of `pids` are live processes.
pub fn live_pids(pids: &[u32]) -> Vec<u32> {
    pids.iter().copied().filter(|pid| alive(*pid)).collect()
}

/// Whether any of `pids` is still a live process.
pub fn any_alive(pids: &[u32]) -> bool {
    pids.iter().copied().any(alive)
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
/// Returns the pids of `tree` still alive once the ladder has run — empty on the ordinary path.
/// Two things it is not: an answer about the process GROUP, which is signalled but not
/// enumerated, and a confirmation, because a signal lands only when its target is next scheduled.
/// A caller that reports what survived polls this until it settles (glass#380).
#[must_use = "the survivors are the caller's to report, or to ignore on purpose"]
pub fn reap_launch(child: &mut Child, tree: &[u32], grace: Duration) -> Vec<u32> {
    let leader = Pid::from_raw(child.id() as i32);
    let signal_all = |signal| {
        if let Some(leader) = leader {
            let _ = kill_process_group(leader, signal);
        }
        for &pid in tree {
            if alive(pid)
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
    live_pids(tree)
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

/// Host and namespace-visible identities for one live process tree.
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ProcessIdentitySet {
    host_pids: Vec<u32>,
    matching_pids: Vec<u32>,
}

/// Parse the PID visible in the innermost namespace from a Linux status file.
pub fn namespace_pid_from_status(status: &str) -> Option<u32> {
    status.lines().find_map(|line| {
        line.strip_prefix("NSpid:")?
            .split_whitespace()
            .last()?
            .parse()
            .ok()
    })
}

impl ProcessIdentitySet {
    pub fn from_pairs(pairs: impl IntoIterator<Item = (u32, Option<u32>)>) -> Self {
        let mut host_pids = Vec::new();
        let mut matching_pids = Vec::new();
        for (host, namespace) in pairs {
            host_pids.push(host);
            matching_pids.push(host);
            matching_pids.extend(namespace);
        }
        host_pids.sort_unstable();
        host_pids.dedup();
        matching_pids.sort_unstable();
        matching_pids.dedup();
        Self {
            host_pids,
            matching_pids,
        }
    }

    pub fn from_host_root(root: u32) -> Self {
        Self::from_pairs(proc_tree_pids(root).into_iter().map(|host| {
            let namespace = std::fs::read_to_string(format!("/proc/{host}/status"))
                .ok()
                .as_deref()
                .and_then(namespace_pid_from_status);
            (host, namespace)
        }))
    }

    pub fn host_pids(&self) -> &[u32] {
        &self.host_pids
    }

    pub fn matching_pids(&self) -> &[u32] {
        &self.matching_pids
    }
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

#[cfg(test)]
mod identity_tests {
    use super::{ProcessIdentitySet, namespace_pid_from_status};

    #[test]
    fn nspid_uses_the_innermost_namespace_pid() {
        let status = "Name:\tapp\nPid:\t4242\nNSpid:\t4242\t2\n";
        assert_eq!(namespace_pid_from_status(status), Some(2));
    }

    #[test]
    fn malformed_or_missing_nspid_is_ignored() {
        for status in ["Name:\tapp\n", "NSpid:\t4242\tbad\n", "NSpid:\t\n"] {
            assert_eq!(namespace_pid_from_status(status), None);
        }
    }

    #[test]
    fn identity_set_sorts_and_deduplicates_host_and_matching_pids() {
        let set = ProcessIdentitySet::from_pairs([
            (4243, Some(3)),
            (4242, Some(2)),
            (7, Some(7)),
            (4242, Some(2)),
        ]);
        assert_eq!(set.host_pids(), &[7, 4242, 4243]);
        assert_eq!(set.matching_pids(), &[2, 3, 7, 4242, 4243]);
    }

    #[test]
    fn identity_set_from_a_live_root_contains_that_process() {
        let root = std::process::id();

        let set = ProcessIdentitySet::from_host_root(root);

        assert!(set.host_pids().contains(&root));
        assert!(set.matching_pids().contains(&root));
    }
}

#[cfg(test)]
mod reap_tests {
    use super::{
        Asked, CLOSE_GRACE, Closed, REAP_GRACE, await_condition, reap_graceful, reap_group,
        teardown_notice,
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
    /// its callers could only report the stop they asked for.
    #[test]
    fn live_pids_names_the_processes_that_are_there() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        assert_eq!(super::live_pids(&[pid]), vec![pid]);
        child.kill().expect("kill");
        child.wait().expect("wait");
        assert!(
            super::live_pids(&[pid]).is_empty(),
            "a child that was killed and reaped is not alive"
        );
    }

    /// An existence test calls a zombie a survivor.
    #[test]
    fn a_zombie_is_not_a_live_process() {
        let mut child = Command::new("sleep")
            .arg("5")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .expect("spawn sleep");
        let pid = child.id();
        child.kill().expect("kill");
        // Deliberately not waited: that is what makes it a zombie rather than gone.
        assert!(
            await_condition(Duration::from_secs(5), || std::fs::read_to_string(format!(
                "/proc/{pid}/stat"
            ))
            .is_ok_and(|s| s
                .rsplit_once(')')
                .is_some_and(|(_, rest)| rest.trim_start().starts_with('Z')))),
            "the fixture has to reach the state under test"
        );
        assert!(std::path::Path::new(&format!("/proc/{pid}")).exists());
        assert!(
            super::live_pids(&[pid]).is_empty(),
            "a zombie holds nothing and takes no signal, so it is not a survivor"
        );
        child.wait().expect("reap the fixture");
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
