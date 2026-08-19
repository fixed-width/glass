//! glass#477: the launch's log readers end at teardown.
//!
//! Nothing in the unit tests reaches this. The app's stdout/stderr write ends are inherited by
//! everything it spawns, so a survivor holds them after teardown — and a reader that could only
//! stop at EOF would park there holding an fd for the life of the process, one pair per launch
//! under `glass-mcp serve --http`.
//!
//! Its own test binary: the measurement is this process's open fds, which a sibling test opening
//! or closing a pipe in parallel would land in.

#![cfg(target_os = "linux")]

use std::collections::HashSet;

use glass_core::{AppSpec, Deadline, Platform, TEARDOWN_BUDGET};
use rustix::process::{Pid, Signal, kill_process};

/// The process the launch leaves behind, killed when the test ends, panic included.
///
/// Load-bearing beyond cleanup: while it holds the write ends the pipes' inodes cannot be freed,
/// so no other pipe in this process can be handed the same `pipe:[n]` and read as a false pass.
/// Killed only after the assertions, for that reason.
struct Survivor(Option<u32>);

impl Drop for Survivor {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|p| Pid::from_raw(p as i32)) {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}

/// Every pipe this process holds, as `fd -> pipe:[inode]`. The inode is half the identity: an fd
/// number the kernel released and handed straight back for a different pipe must not read as the
/// same one still held.
fn open_pipes() -> HashSet<String> {
    std::fs::read_dir("/proc/self/fd")
        .expect("/proc/self/fd")
        .filter_map(|entry| {
            let path = entry.ok()?.path();
            let target = std::fs::read_link(&path).ok()?;
            let target = target.to_string_lossy().into_owned();
            let fd = path.file_name()?.to_string_lossy().into_owned();
            target
                .starts_with("pipe:")
                .then(|| format!("{fd}->{target}"))
        })
        .collect()
}

/// A launch that says something on both streams, leaves a process holding them, and then blocks.
///
/// The survivor is backgrounded from a subshell that exits at once, so it is reparented to init:
/// `reap_launch` signals the launch's process *group* and every pid in a `/proc` walk of its
/// tree, and a process still in either is reaped rather than left behind. What outlives a
/// teardown — what glass#470 measures and reports as `leaked` — is a process that escaped both,
/// which is what this builds. `setsid` covers the group, the subshell covers the tree.
///
/// It prints the pid it left, so the test can kill what glass deliberately could not; the sleep
/// self-limits well inside a suite run if that kill never happens.
fn spec_leaving_a_survivor() -> AppSpec {
    AppSpec {
        build: None,
        run: [
            "sh",
            "-c",
            "printf 'complaint\\n' >&2; ( setsid sleep 10 & echo \"survivor $!\" ); sleep 30",
        ]
        .map(String::from)
        .to_vec(),
        cwd: None,
        env: vec![],
        window_hint: None,
        // The stand-in maps no window, so discovery times out. The launch and its readers still
        // happened, and that failure path runs the same `kill_child` the success path does.
        timeout_ms: 250,
        sandbox: glass_core::SandboxLevel::Off,
        a11y: false,
    }
}

#[test]
#[ignore = "starts a real X server; needs Xvfb"]
fn tearing_down_a_launch_releases_the_pipes_a_survivor_holds_open() {
    let mut plat = glass_x11::X11Platform::from_env().expect("a display");
    // Snapshotted after the platform is up, so the private Xvfb's own stderr pipe is baseline.
    let before = open_pipes();

    plat.start_app(&spec_leaving_a_survivor())
        .expect_err("a command that maps no window cannot be launched");
    plat.stop_app_by(Deadline::from_millis(TEARDOWN_BUDGET.as_millis() as u64))
        .expect("stop");

    let said = plat.drain_logs();
    let survivor = said.iter().find_map(|(_, line)| {
        line.strip_prefix("survivor ")
            .and_then(|pid| pid.trim().parse().ok())
    });
    // Armed before the first assertion, so a failure still cleans up what glass could not.
    let _survivor = Survivor(survivor);

    // Half the assertion, and what keeps the other half from passing vacuously: a launch whose
    // output nobody read would leak nothing *because* it tapped nothing.
    assert!(
        survivor.is_some(),
        "the tap must deliver what the app printed, but the log holds {said:?}"
    );
    let leaked: Vec<_> = open_pipes().difference(&before).cloned().collect();
    assert!(
        leaked.is_empty(),
        "teardown must release the launch's pipes, but these are still held: {leaked:?}"
    );
}
