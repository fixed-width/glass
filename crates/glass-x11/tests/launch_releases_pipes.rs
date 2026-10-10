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
use std::path::Path;

use glass_core::{AppSpec, Deadline, Platform, TEARDOWN_BUDGET};
use rustix::process::{Pid, Signal, kill_process};

/// The process the launch leaves behind, killed when the test ends, panic included.
///
/// Load-bearing beyond cleanup: while it holds the write ends the pipes' inodes cannot be freed,
/// so no other pipe here can be handed the same `pipe:[n]` and read as a false pass — which is
/// why it is killed only after the assertions.
struct Survivor(Option<u32>);

impl Drop for Survivor {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|p| Pid::from_raw(p as i32)) {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}

/// Every pipe this process holds, as `fd -> pipe:[inode]`. The inode is half the identity: an fd
/// number released and handed straight back for a different pipe must not read as one still held.
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
/// It prints the pid it left, so the test can kill what glass deliberately could not. The sleep
/// self-limits if that kill never happens, and is long enough to outlive discovery plus the whole
/// teardown budget on a loaded runner — a survivor that died first would take the pipes to EOF
/// and turn this gate into a vacuous pass.
fn spec_leaving_a_survivor() -> AppSpec {
    AppSpec {
        build: None,
        run: [
            "sh",
            "-c",
            "printf 'complaint\\n' >&2; ( setsid sleep 60 & echo \"survivor $!\" ); sleep 30",
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

    // Two vacuity guards, because either alone can go green for the wrong reason. A launch
    // whose output nobody read would leak nothing *because* it tapped nothing...
    assert!(
        survivor.is_some(),
        "the tap must deliver what the app printed, but the log holds {said:?}"
    );
    // ...and a fixture that stopped leaving a *live* survivor would let the pipes reach EOF on
    // their own, which the EOF-only reader this replaces would also have released. Then the
    // assertion below would pass without covering glass#477 at all, and nothing would say so.
    assert!(
        survivor.is_some_and(|pid| Path::new(&format!("/proc/{pid}")).exists()),
        "the fixture must leave a live survivor holding the write ends, or this gate proves \
         nothing an EOF-only reader would not also pass"
    );
    let leaked: Vec<_> = open_pipes().difference(&before).cloned().collect();
    assert!(
        leaked.is_empty(),
        "teardown must release the launch's pipes, but these are still held: {leaked:?}"
    );
}
