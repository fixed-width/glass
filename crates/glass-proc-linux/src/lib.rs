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
use std::process::Child;
use std::time::{Duration, Instant};

use rustix::process::{kill_process, kill_process_group, Pid, Signal};

/// Grace period a process gets to exit after SIGTERM before SIGKILL.
pub const REAP_GRACE: Duration = Duration::from_secs(2);

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
        let Ok(pid) = pid_str.parse::<u32>() else { continue };
        let status_path = format!("/proc/{pid}/status");
        let Ok(content) = std::fs::read_to_string(&status_path) else { continue };
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

#[cfg(test)]
mod reap_tests {
    use super::{reap_graceful, reap_group, REAP_GRACE};
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
        BufReader::new(c.stdout.take().unwrap()).read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        let grace = Duration::from_millis(300);
        let t = Instant::now();
        reap_graceful(&mut c, grace);
        let el = t.elapsed();
        assert!(el >= grace, "should wait the full grace before SIGKILL (waited {el:?})");
        assert!(el < grace + Duration::from_secs(2), "but must not hang (waited {el:?})");
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
        BufReader::new(leader.stdout.take().unwrap()).read_line(&mut line).unwrap();
        let grandchild: u32 = line.trim().parse().expect("grandchild pid");
        assert!(alive(grandchild), "grandchild should be alive before reaping");
        reap_group(&mut leader, Duration::from_secs(5));
        std::thread::sleep(Duration::from_millis(100));
        assert!(!alive(grandchild), "reap_group must reap the forked grandchild, not orphan it");
    }

    #[test]
    fn grace_constant_is_two_seconds() {
        assert_eq!(REAP_GRACE, Duration::from_secs(2));
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
        assert!(result.contains(&100), "root must be present even with a cycle");
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
        assert!(pids.contains(&std::process::id()), "must include the root pid");
        assert!(
            pids.contains(&child_pid),
            "must include the spawned descendant pid {child_pid}; got {pids:?}"
        );
    }
}
