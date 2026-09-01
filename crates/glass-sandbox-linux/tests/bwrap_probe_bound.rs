//! glass#398, the live path: the user-namespace probe waited for `bwrap` however long it took.
//!
//! The unit tests reach `userns_probe` directly with their own short budget, so `probe()` could
//! pass an hour — or go back to an unbounded `Command::output()` — with every one of them still
//! green. This drives the real constant through `checks()`, which shares its probe with
//! `availability()`, the launch path `glass_start` takes.
//!
//! Kept out of the crate's `mod tests`, and kept to ONE test: it sets `GLASS_BWRAP`, which those
//! tests read (`wrap_argv` asserts on `argv[0]`). A second test here would race that mutation and
//! invalidate the `SAFETY` notes below.

#![cfg(target_os = "linux")]

use std::ffi::OsString;
use std::path::{Path, PathBuf};
use std::time::{Duration, Instant};

/// How long the fixture lives if nothing kills it, in whole seconds. Far past the probe's budget,
/// so a lost bound fails this test rather than wedging the suite.
const FIXTURE_SECS: u64 = 30;

/// Restores rather than unsets, so this stays correct if the file ever grows a second test.
struct EnvGuard(Option<OsString>);

#[allow(unsafe_code)]
impl Drop for EnvGuard {
    fn drop(&mut self) {
        // SAFETY: one test in this binary, so every environment read is on this thread, between
        // this mutation and the last. The only other threads are `run_bounded`'s pipe drains,
        // which never touch the environment.
        match self.0.take() {
            Some(v) => unsafe { std::env::set_var("GLASS_BWRAP", v) },
            None => unsafe { std::env::remove_var("GLASS_BWRAP") },
        }
    }
}

#[allow(unsafe_code)]
fn point_glass_bwrap_at(path: &Path) -> EnvGuard {
    let guard = EnvGuard(std::env::var_os("GLASS_BWRAP"));
    // SAFETY: as above — one test in this binary.
    unsafe { std::env::set_var("GLASS_BWRAP", path) };
    guard
}

/// A `bwrap` that takes the probe's question and never answers it.
///
/// `exec`, so the sleeping process is the one glass spawned — a shell that forked its sleep would
/// leave the sleeper behind a kill that reached only the shell.
fn hung_bwrap(dir: &Path) -> PathBuf {
    use std::os::unix::fs::PermissionsExt as _;
    let bin = dir.join("bwrap");
    std::fs::write(
        &bin,
        format!(
            "#!/bin/sh\n[ $# -eq 11 ] || exit 3\n\
             [ \"$*\" = '--unshare-user --unshare-pid --ro-bind / / --proc /proc --json-status-fd 1 -- true' ] || exit 3\n\
             exec sleep {FIXTURE_SECS}\n"
        ),
    )
    .expect("write the fake bwrap");
    std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(0o755)).expect("chmod");
    bin
}

#[test]
fn a_bwrap_that_never_answers_is_reported_rather_than_waited_out() {
    let dir = tempfile::tempdir().expect("tempdir");
    let _guard = point_glass_bwrap_at(&hung_bwrap(dir.path()));

    let started = Instant::now();
    let checks = glass_sandbox_linux::checks();
    let elapsed = started.elapsed();
    assert!(
        elapsed < Duration::from_secs(FIXTURE_SECS) / 2,
        "the probe waited the binary out rather than bounding it: {elapsed:?}"
    );
    // The other end of the bracket, and the one no fixture can fail: every fake bwrap answers in
    // milliseconds, so a budget cut to nothing would leave the whole suite green while a real
    // `--ro-bind /` timed out and no sandboxed launch worked at all.
    assert!(
        elapsed >= Duration::from_secs(5),
        "the probe's budget is no longer generous enough for real kernel work: {elapsed:?}"
    );

    let sandbox = checks
        .iter()
        .find(|c| c.name == "sandbox (bubblewrap)")
        .expect("doctor reports on the sandbox");
    assert_eq!(
        sandbox.status,
        glass_core::CheckStatus::Fail,
        "a bubblewrap that answered nothing has proven no namespace: {sandbox:?}"
    );
    assert!(sandbox.detail.contains("no answer within"), "{sandbox:?}");
    // The remedy for this cause specifically: every remedy ends in `sandbox:"off"`, so asserting
    // that alone would pass for the install advice this binary is here to keep out.
    assert!(
        sandbox
            .remedy
            .as_deref()
            .is_some_and(|r| r.contains("mount") && !r.contains("install")),
        "{sandbox:?}"
    );
}
