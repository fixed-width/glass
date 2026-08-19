//! Fixtures shared by the tap and line-splitter suites: a child that leaves a process holding its
//! pipe, and a guard that kills it.

use std::io::{BufRead, BufReader};
use std::process::{Child, Command, Stdio};

use rustix::process::{Pid, Signal, kill_process};

/// A process holding a child's pipe after the child is gone, killed when the test ends, panic
/// included. It outlives the suites' deadlines several times over, and self-limits if the guard
/// cannot run.
///
/// Load-bearing beyond cleanup: while it holds the write end the pipe's inode cannot be freed, so
/// no sibling test thread can be handed the same `pipe:[n]` and read as a false failure.
pub(crate) struct Survivor(pub(crate) Option<u32>);

impl Drop for Survivor {
    fn drop(&mut self) {
        if let Some(pid) = self.0.and_then(|p| Pid::from_raw(p as i32)) {
            let _ = kill_process(pid, Signal::KILL);
        }
    }
}

/// Run `script` — which must background something and print its pid on stdout — and hand back the
/// exited child plus a guard for the survivor.
pub(crate) fn child_with_survivor(script: &str) -> (Child, Survivor) {
    let mut c = Command::new("sh")
        .arg("-c")
        .arg(script)
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sh is runnable");
    let mut pid = String::new();
    let read = BufReader::new(c.stdout.take().expect("piped stdout")).read_line(&mut pid);
    // Guarded before anything that can panic: the sleeper exists from the moment `sh` runs.
    let survivor = Survivor(pid.trim().parse().ok());
    read.expect("sh reports the pid it backgrounded");
    assert!(survivor.0.is_some(), "sh reported {pid:?}, not a pid");
    c.wait().expect("the child exits, the survivor does not");
    (c, survivor)
}

/// A child that says `said` on stderr, leaves a sleeper holding that pipe, and exits.
pub(crate) fn said_then_survived(said: &str) -> (Child, Survivor) {
    child_with_survivor(&format!("printf '{said}' >&2; sleep 30 & echo $!"))
}
