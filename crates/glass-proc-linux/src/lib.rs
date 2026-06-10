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
