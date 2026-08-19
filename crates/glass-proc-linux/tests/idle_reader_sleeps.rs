//! glass#471: the reader sleeps between reads.
//!
//! Nothing in the unit tests reaches this. A reader that turned every empty read straight into
//! another empty read would pass all of them — and `Xvfb` holds one for the whole life of a
//! server that is normally silent, so it would pin a core for the session.
//!
//! Its own test binary: the measurement is this process's CPU time, which a sibling test running
//! in parallel would add to.

#![cfg(target_os = "linux")]

use std::process::{Command, Stdio};
use std::time::Duration;

/// How long the reader is watched. Long enough that a spinning one is unmistakable, short enough
/// that the survivor below outlives it many times over.
const WATCHED_FOR: Duration = Duration::from_millis(300);

/// This process's CPU time in clock ticks: `utime` + `stime`, read after the last `)` because the
/// comm field before it can itself contain spaces and parens.
fn cpu_ticks() -> u64 {
    let stat = std::fs::read_to_string("/proc/self/stat").expect("/proc/self/stat");
    let after_comm = &stat[stat.rfind(')').expect("comm is parenthesised") + 1..];
    let fields: Vec<&str> = after_comm.split_whitespace().collect();
    // `state` is the first field left, which puts utime and stime at 11 and 12.
    fields[11].parse::<u64>().expect("utime") + fields[12].parse::<u64>().expect("stime")
}

#[test]
fn an_idle_reader_costs_no_cpu() {
    // The survivor keeps the pipe open with nothing to say, which is the state a healthy server
    // spends its life in. It outlives the watch and then goes on its own.
    let mut child = Command::new("sh")
        .arg("-c")
        .arg("printf 'the fatal line\\n' >&2; sleep 5 &")
        .stdout(Stdio::null())
        .stderr(Stdio::piped())
        .spawn()
        .expect("sh is runnable");
    let tail = glass_proc_linux::StderrTail::drain(child.stderr.take().expect("piped stderr"))
        .expect("a reader");
    child
        .wait()
        .expect("the child exits, the survivor does not");

    let before = cpu_ticks();
    std::thread::sleep(WATCHED_FOR);
    let spent = cpu_ticks() - before;

    // Clock ticks are 100/s on Linux, so a reader spinning through the watch bills ~30 and one
    // asleep in `poll` bills none. Ten is far above the noise and far below a spin.
    assert!(
        spent < 10,
        "an idle reader must sleep, not spin: {spent} ticks of CPU in {WATCHED_FOR:?}"
    );
    assert_eq!(
        tail.finish(Duration::from_millis(200)).trim(),
        "the fatal line"
    );
}
