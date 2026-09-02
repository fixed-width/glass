//! App spawn, background log-piping, and terminate for the macOS backend.
//!
//! Mirrors `glass-x11`/`glass-wayland`'s `spawn`+`LineTap`+`LogSink` shape: a plain
//! `std::process::Command` built from [`AppSpec`], stdout/stderr piped into a shared,
//! `Arc<Mutex<_>>`-guarded log buffer via one reader thread per stream, so
//! `MacosPlatform::drain_logs` can read it without blocking the readers. The taps go back to the
//! caller with the child: ending them is part of teardown (glass#477). `terminate` asks
//! the app to quit first — AppKit runs no shutdown path for a signal — and only then falls
//! back to `glass-proc-linux::reap`'s SIGTERM -> brief wait -> SIGKILL -> reap sequence,
//! reimplemented here (rather than depending on that crate) because it is `/proc`-based
//! and therefore Linux-only.
//!
//! Window discovery is a separate concern ([`crate::scwindow::find_window_for_pids`]).
//!
//! Clip-shim injection: a contained launch whose target is not hardened-runtime signed
//! ([`target_is_injectable`]) gets `glass-clip-shim-macos`'s built dylib
//! ([`shim_dylib_path`], the macOS wrapper around [`crate::shim_path::resolve_shim`]'s pure
//! path-tier logic) loaded via `DYLD_INSERT_LIBRARIES`, plus a per-spawn private
//! pasteboard name in `GLASS_CLIP_PASTEBOARD` — both set on the `Command` before
//! `cmd.spawn()` in [`spawn`]. `Command`'s envp is applied at the `exec` that follows
//! `pre_exec`'s `sandbox_init` call (not at fork/`pre_exec` time), so both vars are present
//! in the launched app's environment, having survived the sandbox. [`ClipLaunch`] carries
//! those facts back to `start_app`, which holds them until the launched window is confirmed
//! and the clipboard route can be decided.

use std::ffi::CString;
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_pipe_unix::LineTap;
use std::os::fd::AsFd;

use rustix::process::{Pid, Signal, kill_process};

use glass_core::platform::{AppSpec, ProtectedHostPath, SandboxLevel};
use glass_core::{GlassError, Result, Stream, TEARDOWN_BUDGET};
use glass_sandbox_macos::{ProfileOpts, build_profile, launch_reallows};

/// Per-spawn counter feeding one component of
/// [`crate::clipboard_route::session_pasteboard_name`] (combined there with this process's pid
/// and a wall-clock nonce). It only disambiguates multiple launches WITHIN a single `glass-mcp`
/// run; the pid+nonce components are what prevent name reuse ACROSS runs (a bare counter resets
/// to its start value on every restart).
static CLIP_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Clip-shim facts for one contained, injectable launch: the private pasteboard name the
/// shim redirects `NSPasteboard.generalPasteboard` to. Produced only when [`spawn`] actually
/// set up injection, so injectability is encoded by the `Option` itself. `start_app` holds this
/// until the launched window is confirmed, then combines `name` with a live shim-confirmation
/// check to decide the session's `ClipboardRoute` (`crate::clipboard_route::decide_route`).
#[derive(Debug)]
pub(crate) struct ClipLaunch {
    pub name: String,
}

/// Whether `stderr` — a `codesign --display --verbose=2` report, with `status_success`
/// codesign's exit status — shows a RECOGNIZED non-hardened signature, i.e.
/// `DYLD_INSERT_LIBRARIES` injection can take on this target. Pure, so the decision is
/// unit-testable without shelling out to `codesign`.
///
/// Fail-closed: `true` only on a recognized non-hardened signal, `false` otherwise (including
/// any unrecognized/garbled output). Injectable iff either:
/// - the report contains `code object is not signed at all` (unsigned — codesign exits
///   non-zero for this, so the status isn't gated for this specific marker), OR
/// - codesign SUCCEEDED and there is a `flags=` line that does NOT mention `runtime` (an
///   adhoc/linker-signed or otherwise non-hardened binary; a hardened-runtime binary's flags
///   line contains `runtime`).
///
/// Everything else — an I/O/parse failure, a non-zero exit without the "not signed" marker, or
/// output with no `flags=` line — is treated as non-injectable, so "couldn't determine" never
/// grants injection.
fn injectable_from_codesign_report(stderr: &str, status_success: bool) -> bool {
    // A genuinely unsigned binary's report is ONLY the "not signed" diagnostic — it can never
    // also carry a `flags=` line (there is no signature to describe one). Requiring that
    // absence stops the marker being smuggled in via attacker-controlled report text — codesign
    // echoes `Executable=<path>`, and `<path>` is the caller-chosen `spec.run[0]` — to force a
    // genuinely signed, possibly hardened-runtime target to classify as injectable.
    if stderr.contains("code object is not signed at all") && !stderr.contains("flags=") {
        return true;
    }
    status_success && stderr.contains("flags=") && !stderr.contains("runtime")
}

/// True iff `program` is not hardened-runtime signed, so injecting the clip shim via
/// `DYLD_INSERT_LIBRARIES` can take. Shells out to `codesign --display --verbose=2`, which
/// writes its report to stderr, not stdout.
///
/// Fail-closed: any uncertainty (`codesign` missing or unspawnable, its output not valid UTF-8,
/// or an unrecognized report) reports `false` (non-injectable). See
/// [`injectable_from_codesign_report`] for the exact recognized-signal logic.
fn target_is_injectable(program: &Path) -> bool {
    let Ok(output) = Command::new("codesign")
        .arg("--display")
        .arg("--verbose=2")
        .arg(program)
        .output()
    else {
        return false;
    };
    let status_success = output.status.success();
    let Ok(stderr) = String::from_utf8(output.stderr) else {
        return false;
    };
    injectable_from_codesign_report(&stderr, status_success)
}

/// Whether `bundle` is Apple platform code — OS code rather than a developer's app. Shells out
/// to the same `codesign --display --verbose=2` that [`target_is_injectable`] reads (the report
/// goes to stderr, not stdout); the decision itself is
/// [`crate::bundle::platform_code_from_codesign_report`], which carries the reasoning.
///
/// Fail-open: if codesign can't be run or its output isn't UTF-8, this reports `false` and the
/// caller keeps the launch path it would have taken anyway.
pub(crate) fn is_apple_platform_code(bundle: &Path) -> bool {
    let Ok(output) = Command::new("codesign")
        .arg("--display")
        .arg("--verbose=2")
        .arg(bundle)
        .output()
    else {
        return false;
    };
    let Ok(stderr) = String::from_utf8(output.stderr) else {
        return false;
    };
    crate::bundle::platform_code_from_codesign_report(&stderr)
}

/// Env var overriding [`shim_dylib_path`]'s resolution — tests and non-standard layouts.
const SHIM_DYLIB_ENV: &str = "GLASS_CLIP_SHIM_DYLIB";

/// [`crate::shim_path::resolve_shim`] against the real environment: the running executable's
/// directory and [`SHIM_DYLIB_ENV`]. This wrapper supplies only those two OS-touching inputs;
/// the tier logic itself is pure and lives in [`crate::shim_path`].
fn shim_dylib_path() -> Option<PathBuf> {
    shim_dylib_path_with(std::env::var(SHIM_DYLIB_ENV).ok().map(PathBuf::from))
}

/// [`shim_dylib_path`] against an already-read override — the testable seam (no global env), so
/// a test can exercise the override tier without `set_var` racing a concurrent env reader.
fn shim_dylib_path_with(env_override: Option<PathBuf>) -> Option<PathBuf> {
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    crate::shim_path::resolve_shim(&exe_dir, env_override)
}

/// Log lines captured by the per-stream [`LineTap`]s started in [`spawn`], drained by
/// `MacosPlatform::drain_logs`. `Arc<Mutex<_>>` (not a bare `Vec`) because the reader
/// threads outlive `spawn`'s call and push into it concurrently with `drain_logs` reading
/// it from the main thread — the same shape `glass-x11`/`glass-wayland` use.
pub(crate) type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// How long [`terminate`] waits after SIGTERM before escalating to SIGKILL. Short: by the time
/// a signal is being sent the app has already declined (or could not be sent) the quit request
/// that precedes it, so this is the escape hatch, not a place to wait out a shutdown handler.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// How long [`terminate`] waits for the app to quit through its own shutdown path after being
/// asked ([`crate::ffi::terminate_app`], which carries the measurement), before falling back to
/// signals. An app that cannot be asked skips this entirely rather than spending it; only one
/// that was asked and will not go pays it in full — several times over what a cooperating app
/// was measured to need. Matches the Windows backend's close grace.
///
/// The ladder that follows this — `SIGTERM`, [`TERMINATE_GRACE`], `SIGKILL` — has to fit inside
/// [`TEARDOWN_BUDGET`] too, and it is the part that actually guarantees the app is gone. When the
/// *process* is going away, `glass-mcp` abandons teardown once that budget is spent, and a
/// `spawn_blocking` teardown thread cannot be cancelled: a grace long enough to swallow the budget
/// would mean the signals were never sent at all and the app outlived glass, orphaned to launchd.
const QUIT_GRACE: Duration = Duration::from_millis(1500);

// The whole ladder must finish inside the budget glass-mcp allows teardown, or it is abandoned
// partway and the child is orphaned. This is the only thing keeping the numbers — in different
// crates — honest about each other.
const _: () = assert!(
    QUIT_GRACE.as_millis() + TERMINATE_GRACE.as_millis() < TEARDOWN_BUDGET.as_millis(),
    "the ask + signal ladder must complete inside glass_core::TEARDOWN_BUDGET"
);

/// How often the exit waits re-check the child.
const EXIT_POLL: Duration = Duration::from_millis(20);

/// Spawn `spec.run` (with `spec.cwd`/`spec.env` applied) with stdout/stderr piped into
/// `logs` via one reader thread per stream. Returns [`GlassError::AppNotStarted`] if the
/// program can't be launched (e.g. not found, not executable).
///
/// macOS process containment: [`SandboxLevel::Default`]/[`SandboxLevel::Strict`] apply a
/// generated Seatbelt (`sandbox_init`) profile to the launched app via a fork-safe
/// `pre_exec` (see below). [`SandboxLevel::Off`] spawns unchanged.
///
/// The [`Launch`]'s `clip` is the clip-shim launch facts ([`ClipLaunch`]), `Some` only for
/// a contained launch whose target is injectable (see [`target_is_injectable`]) and whose
/// shim dylib resolved (see [`shim_dylib_path`]); `None` for `SandboxLevel::Off` or a
/// non-injectable/unresolved target. The caller (`MacosPlatform::start_app`) holds it for a
/// later clipboard-routing decision — this function only sets up the injection.
pub(crate) fn spawn(
    spec: &AppSpec,
    logs: LogSink,
    protected_paths: &[ProtectedHostPath],
) -> Result<Launch> {
    glass_sandbox_macos::profile::validate_protected_paths(protected_paths)?;
    let mut cmd = Command::new(&spec.run[0]);

    // Apply the caller's env FIRST, so glass's own containment/injection vars are written LAST
    // and win. Otherwise a caller could blank `DYLD_INSERT_LIBRARIES`/`GLASS_CLIP_PASTEBOARD`
    // via `spec.env` and silently defeat injection while the profile had already granted the
    // pasteboard — fail-OPEN.
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }

    // Containment: for Default/Strict, apply a generated Seatbelt profile to the launched app
    // in a fork-safe pre_exec (build the CString here, before fork; the closure only makes the
    // sandbox_init syscall). Off spawns unchanged. Build (run_build) is never contained.
    let mut clip: Option<ClipLaunch> = None;
    if spec.sandbox != SandboxLevel::Off {
        // Effective working dir for the profile + reachability: the caller's cwd, else glass's own
        // current dir. A failure to read current_dir() is surfaced (not silently substituted with
        // "/", which would misroot relative launch tokens) and leaves the cwd unset.
        let effective_cwd: Option<PathBuf> = spec.cwd.clone().or_else(|| {
            std::env::current_dir()
                .map_err(|e| {
                    logs.lock().expect("log sink mutex").push((
                        Stream::Stderr,
                        format!(
                            "glass: could not determine current directory for sandbox cwd: {e}"
                        ),
                    ));
                })
                .ok()
        });
        // Canonicalize for the profile's cwd field (same as the pre-diff code did), non-fatally.
        let cwd_canon: Option<PathBuf> = effective_cwd
            .as_ref()
            .map(|c| std::fs::canonicalize(c).unwrap_or_else(|_| c.clone()));
        // Resolve to an absolute path: `build_profile`'s guard needs one to reason about home
        // exposure.
        let program =
            std::fs::canonicalize(&spec.run[0]).unwrap_or_else(|_| PathBuf::from(&spec.run[0]));

        // Decide injection before building the profile: `allow_pasteboard` depends on it.
        // `dylib_path` is resolved once and reused below (rather than a second
        // `shim_dylib_path()` call) so a transient filesystem hiccup between the two checks
        // can't make `injectable` and the later `.expect` disagree.
        let dylib_path = shim_dylib_path();
        let injectable = target_is_injectable(&program) && dylib_path.is_some();
        let allow_pasteboard = injectable;
        // The shim dylib FILE, re-allowed for read in the profile below when injecting (`None`
        // — no re-allow — otherwise, matching unchanged pre-injection behavior).
        let mut shim_file: Option<PathBuf> = None;
        if injectable {
            let pid = std::process::id();
            let nonce = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos() as u64)
                .unwrap_or(0);
            let token = CLIP_TOKEN.fetch_add(1, Ordering::Relaxed);
            // Unique per launch (pid + wall-clock nonce + counter) so the name can never be
            // reused across `glass-mcp` restarts — a reused name lets a stale, system-wide
            // named pasteboard mask a failed injection with an old sentinel (fail-OPEN).
            let name = crate::clipboard_route::session_pasteboard_name(pid, nonce, token);
            let dylib = dylib_path.expect("checked Some above");
            // Surface, rather than silently swallow, a caller trying to set the injection vars
            // themselves: glass overrides them below (last-write-wins, above), so their value
            // is intentionally ignored — say so instead of leaving the caller to wonder.
            for (k, _) in &spec.env {
                if matches!(
                    k.as_str(),
                    "DYLD_INSERT_LIBRARIES" | "GLASS_CLIP_PASTEBOARD"
                ) {
                    logs.lock().expect("log sink mutex").push((
                        Stream::Stderr,
                        format!(
                            "glass: caller-supplied {k} is overridden by clipboard-shim injection"
                        ),
                    ));
                }
            }
            // The shim dylib lives in glass's own `target/<profile>/` tree, typically under
            // $HOME — which the profile below denies by default (see `build_profile`'s
            // `/Users` deny). dyld loads the shim AFTER `sandbox_init` applies the profile
            // (in `pre_exec`, below), so the FILE itself must be re-allowed for read here or
            // dyld can't open it, injection silently fails, and the sandboxed app never sees
            // the shim. Re-allow only the file (not its whole directory) — least privilege.
            shim_file = Some(dylib.clone());
            // Set LAST (after `spec.env`) and BEFORE `cmd.spawn()`: `Command`'s envp is applied
            // at the `exec` that follows `pre_exec`'s `sandbox_init`, not at fork time, so both
            // vars survive the sandbox into the launched app's environment.
            cmd.env("DYLD_INSERT_LIBRARIES", &dylib);
            cmd.env("GLASS_CLIP_PASTEBOARD", &name);
            clip = Some(ClipLaunch { name });
        }

        // Re-allow the launch target itself (program + path args) so it stays reachable under
        // the profile's `/Users` read-deny — see `launch_reallows`'s own docs. Pass the
        // un-canonicalized `effective_cwd` (not `cwd_canon`) so relative tokens resolve against
        // where glass was actually invoked.
        let reallows = launch_reallows(&spec.run, effective_cwd.as_deref());
        // Union the arg-literal re-allows with the shim file. The `.contains()` dedup below is
        // defensive — the shim lives in glass's own target dir and shouldn't collide.
        let mut ro_files = reallows.ro_files;
        if let Some(f) = shim_file
            && !ro_files.contains(&f)
        {
            ro_files.push(f);
        }
        // Pasteboard is allowed only for an injectable target (the shim's redirect is the
        // actual isolation there); a hardened/non-injectable target keeps it denied, same as
        // before this task.
        let opts = ProfileOpts {
            // `/` here is inert (`is_safe_reallow("/")` is false) — it only matters when
            // `effective_cwd` was unset (a `current_dir()` failure, logged above).
            cwd: cwd_canon.unwrap_or_else(|| PathBuf::from("/")),
            program,
            ro_binds: reallows.ro_binds,
            ro_files,
            rw_binds: vec![],
            protected_paths: protected_paths.to_vec(),
            allow_pasteboard,
        };
        let profile = build_profile(spec.sandbox, &opts)?;
        let profile_c = CString::new(profile).map_err(|e| {
            GlassError::SandboxUnavailable(format!("sandbox profile contains NUL: {e}"))
        })?;
        // SAFETY: the closure runs in the forked child in the narrow window before `exec`; it
        // makes a single `sandbox_init` syscall over a pre-built `CString` (see `apply_cstr`)
        // and performs no allocation of its own.
        unsafe {
            cmd.pre_exec(move || glass_sandbox_macos::apply_cstr(&profile_c));
        }
    }

    cmd.args(&spec.run[1..]);
    if let Some(cwd) = &spec.cwd {
        cmd.current_dir(cwd);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        // A PermissionDenied under containment could be `sandbox_init` rejecting the profile,
        // or a plain EACCES on a non-executable binary — indistinguishable from this
        // `io::Error` alone. Surface SandboxUnavailable either way without overclaiming which.
        if spec.sandbox != SandboxLevel::Off && e.kind() == std::io::ErrorKind::PermissionDenied {
            GlassError::SandboxUnavailable(format!(
                "launch failed under containment (sandbox != off): sandbox_init rejected the profile, or the program could not be exec'd: {e}"
            ))
        } else {
            GlassError::AppNotStarted(format!("spawn {:?}: {e}", spec.run))
        }
    })?;

    // `Stdio::piped()` guarantees these are `Some` immediately after a successful spawn.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    let taps = match (
        tap_or_reap(
            stdout,
            Stream::Stdout,
            "glass-app-stdout",
            &logs,
            &mut child,
            spec,
        ),
        tap_or_reap(
            stderr,
            Stream::Stderr,
            "glass-app-stderr",
            &logs,
            &mut child,
            spec,
        ),
    ) {
        (Ok(out), Ok(err)) => vec![out, err],
        (out, err) => {
            // The first path that ends a `ClipLaunch`, and `release_clip`'s doc requires every one
            // of them to release: the boards persist system-wide, and a later session can read a
            // stale `.ready` sentinel.
            release_named_boards(clip.as_ref());
            return Err(out.err().or(err.err()).expect("one of the two failed"));
        }
    };

    Ok(Launch { child, clip, taps })
}

/// Release the named pasteboards a shim launch created, for a launch abandoned before the caller
/// ever sees its [`ClipLaunch`]. `backend::release_clip` is the same call where one exists.
fn release_named_boards(clip: Option<&ClipLaunch>) {
    if let Some(c) = clip {
        crate::clipboard::release_named(&c.name);
        crate::clipboard::release_named(&format!("{}.ready", c.name));
    }
}

/// What a direct spawn produced.
#[derive(Debug)]
pub(crate) struct Launch {
    pub(crate) child: Child,
    /// The clip-shim launch facts — see [`spawn`], which decides them.
    pub(crate) clip: Option<ClipLaunch>,
    /// The app's stdout/stderr readers, held for the app's lifetime. The app's write ends are
    /// inherited by everything it spawns, so an EOF-only reader parks on a survivor's pipe, holding
    /// a thread and an fd for the life of the process (glass#477).
    ///
    /// Bind these, do not `..` them, in anything that later reads the sink: dropping a tap stops
    /// its reader, and `..` does that at the destructuring — before the app has written a line.
    pub(crate) taps: Vec<LineTap>,
}

/// Start a reader over one of the launch's streams, or terminate the launch and say why it could
/// not.
///
/// The failed start consumed the pipe, so the app's next write to that stream takes `SIGPIPE` — a
/// death mid-run with nothing in the logs to say why.
fn tap_or_reap<R: std::io::Read + AsFd + Send + 'static>(
    stream: R,
    tag: Stream,
    name: &str,
    logs: &LogSink,
    child: &mut Child,
    spec: &AppSpec,
) -> Result<LineTap> {
    LineTap::start(stream, tag, name, logs.clone()).map_err(|e| {
        terminate(child);
        GlassError::AppNotStarted(format!(
            "started {:?} but could not read its output ({e}); the app was stopped rather than \
             left to write into a pipe nobody drains — free up threads and file descriptors on \
             the host",
            spec.run
        ))
    })
}

/// Idempotently terminate `child`: ask it to quit, wait up to [`QUIT_GRACE`], then SIGTERM,
/// wait up to [`TERMINATE_GRACE`], then SIGKILL, then reap. A child that *cannot* be asked —
/// anything not registered as a running application, which is every console-shaped process —
/// skips the quit grace entirely rather than spending it, so its teardown is as fast as it was
/// before. Safe to call on an already-exited (or already-terminated) child — `try_wait` is
/// checked first so a second call never re-signals a pid the kernel may have since recycled.
///
/// The quit request comes first because signals give a Cocoa app no shutdown path at all:
/// AppKit installs no `SIGTERM` handler, so the default disposition ends the process with
/// `applicationWillTerminate:` never called and nothing flushed — measured, not assumed (see
/// [`crate::ffi::terminate_app`]). An app that ignores or vetoes the request still gets the
/// full signal ladder afterwards.
///
/// This is the child-backed path. A LaunchServices-adopted app (`backend.rs`'s `Adopted`) that
/// glass started is asked the same way but never escalated — glass will not force-kill an app it
/// only adopted — and one that was merely re-found running is not asked at all; see
/// `Adopted::reap`.
pub(crate) fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }

    // A pid too large for `i32` cannot be a macOS pid, so a conversion failure lands in the
    // same place as an app that isn't GUI-registered: couldn't ask, fall through to signals.
    let asked = i32::try_from(child.id()).is_ok_and(crate::ffi::terminate_app);
    if asked && wait_for_exit(child, QUIT_GRACE) {
        // `wait_for_exit` only reports true via `try_wait() == Ok(Some(_))`, which has already
        // reaped; this just collects the status so the call reads the same as the path below.
        let _ = child.wait();
        return;
    }

    let pid = Pid::from_child(child);
    let _ = kill_process(pid, Signal::TERM);
    // Whether SIGTERM was enough or not, SIGKILL follows — it is idempotent on an exited child.
    let _ = wait_for_exit(child, TERMINATE_GRACE);

    let _ = child.kill(); // SIGKILL, tolerates an already-exited child.
    let _ = child.wait(); // Reap so the child doesn't linger as a zombie.
}

/// Poll `child` for exit for up to `grace`, reporting whether it exited within it. A
/// `try_wait` error ends the wait reporting "still running": the pid is ours, so a failure is
/// unexpected, and the caller escalating is the safe reading of an unreadable state.
fn wait_for_exit(child: &mut Child, grace: Duration) -> bool {
    let deadline = Instant::now() + grace;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return true,
            Ok(None) if Instant::now() >= deadline => return false,
            Ok(None) => std::thread::sleep(EXIT_POLL),
            Err(_) => return false,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn spec(run: &[&str]) -> AppSpec {
        AppSpec {
            build: None,
            run: run.iter().map(|s| s.to_string()).collect(),
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        }
    }

    fn empty_sink() -> LogSink {
        Arc::new(Mutex::new(Vec::new()))
    }

    /// Path to the `glass-macos-sandbox-probe` fixture (see `src/bin/sandbox_probe.rs`) that
    /// the `SandboxLevel::Default` tests below spawn in place of `/bin/sh`. Located the same
    /// way `shim_dylib_path_with` locates the clip shim: `cargo test`'s own binary lives at
    /// `target/<profile>/deps/<hash>`, one directory below where `cargo build --all-targets`
    /// places a plain `[[bin]]` target's unhashed copy. `scripts/test-macos.sh` builds this
    /// fixture first, so it's never missing under the sanctioned entry point. Panics (never
    /// skips) if it somehow still is — these tests must not silently pass without exercising
    /// the containment they assert.
    fn sandbox_probe_path() -> PathBuf {
        const PROBE_BIN_NAME: &str = "glass-macos-sandbox-probe";
        let exe_dir = std::env::current_exe()
            .expect("read current_exe")
            .parent()
            .expect("current_exe has a parent dir")
            .to_path_buf();
        let probe = exe_dir
            .parent()
            .expect("exe_dir has a parent dir")
            .join(PROBE_BIN_NAME);
        assert!(
            probe.is_file(),
            "{probe:?} not built; run `cargo build -p glass-macos --bin {PROBE_BIN_NAME}` \
             (matching this run's profile, e.g. add --release) first — \
             scripts/test-macos.sh does this automatically"
        );
        probe
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn default_sandbox_runs_but_contains_filesystem() {
        // Exercises the full read-all-except-home model through the real spawn path: a system
        // read (outside cwd, outside home) must succeed (whole-FS read-allow), a read of a
        // probe file under a project dir living *inside* $HOME must also succeed (the cwd
        // reallow that undoes the `/Users` deny for a real project dir), and a read of a secret
        // living directly under $HOME (outside the project dir) must be denied.
        //
        // Both the probe and the secret are files that provably exist (rather than relying on a
        // fixed name like `.ssh/known_hosts`, which may not exist on a fresh CI runner) so a
        // failed read can only mean the sandbox denied it, never that the path was absent.
        let home = std::env::var("HOME").expect("HOME must be set");
        let proj =
            std::path::Path::new(&home).join(format!("glass-sbx-cwd-{}", std::process::id()));
        std::fs::create_dir_all(&proj).expect("create project dir under $HOME");
        let probe_path = proj.join("probe");
        std::fs::write(&probe_path, "probe").expect("write probe file under the project dir");
        let secret_path = std::path::Path::new(&home).join("glass-sbx-secret");
        std::fs::write(&secret_path, "top-secret").expect("write test secret under $HOME");
        // Drop guard (rather than a manual cleanup closure called on each exit path) so both
        // the secret file and the project dir are removed even if `child.wait()` or an
        // assertion below panics.
        struct Cleanup {
            secret: std::path::PathBuf,
            proj: std::path::PathBuf,
        }
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.secret);
                let _ = std::fs::remove_dir_all(&self.proj);
            }
        }
        let _cleanup = Cleanup {
            secret: secret_path.clone(),
            proj: proj.clone(),
        };
        let probe_str = probe_path.to_str().expect("probe path is valid UTF-8");
        let secret = secret_path.to_str().expect("secret path is valid UTF-8");
        let probe_bin = sandbox_probe_path();
        let probe_bin_str = probe_bin
            .to_str()
            .expect("sandbox-probe path is valid UTF-8");
        // The paths ride in `env`, not argv: `launch_reallows` re-allows any `run[1..]` token
        // that is itself an absolute, existing path (a launched script's own path argument), and
        // `SECRET_PATH` is exactly the path this test proves is denied — naming it in argv would
        // re-allow it and the assertion below would pass for the wrong reason.
        let run = [
            probe_bin_str,
            "--read-env",
            "SYS_PATH",
            "SYS_OK",
            "SYS_DENIED",
            "--read-env",
            "CWD_PATH",
            "CWD_OK",
            "CWD_DENIED",
            "--read-env",
            "SECRET_PATH",
            "HOME_READABLE",
            "HOME_DENIED",
        ];

        let mut denied = spec(&run);
        denied.sandbox = SandboxLevel::Default;
        denied.cwd = Some(proj.clone());
        denied.env = vec![
            ("SYS_PATH".to_string(), "/usr/lib/dyld".to_string()),
            ("CWD_PATH".to_string(), probe_str.to_string()),
            ("SECRET_PATH".to_string(), secret.to_string()),
        ];
        let logs = empty_sink();
        let Launch {
            mut child,
            clip,
            taps: _taps,
        } = spawn(&denied, logs.clone(), &[])
            .unwrap_or_else(|e| panic!("sandboxed spawn should succeed: {e}"));
        // The probe is a plain, unsigned arm64 binary — always injectable — so a `None` here
        // means the shim never resolved (most likely `glass-clip-shim-macos` wasn't built; see
        // scripts/test-macos.sh), not that containment itself is untested. Assert it explicitly
        // rather than letting the containment checks below pass without exercising injection.
        assert!(
            clip.is_some(),
            "clip shim was not injected into the sandboxed probe; is glass-clip-shim-macos built?"
        );
        child.wait().expect("wait");
        std::thread::sleep(Duration::from_millis(100));
        let out: Vec<String> = logs
            .lock()
            .expect("sink")
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        assert!(
            out.iter().any(|l| l == "SYS_OK"),
            "whole-FS read should be allowed: {out:?}"
        );
        assert!(
            out.iter().any(|l| l == "CWD_OK"),
            "cwd under home should be reallowed: {out:?}"
        );
        assert!(
            out.iter().any(|l| l == "HOME_DENIED"),
            "home read must be denied: {out:?}"
        );
        assert!(
            !out.iter().any(|l| l == "HOME_READABLE"),
            "home leaked: {out:?}"
        );
    }

    /// `launch_reallows` re-allows the launch target's OWN directory independent of `cwd`. With
    /// only `cwd` reallowed under the `/Users` read-deny, a launch target living under `$HOME`
    /// but launched with a `cwd` OUTSIDE `$HOME` could not even be read to be exec'd. The macOS
    /// analog of the Linux `sandbox_default_reaches_launch_target_via_argument_path` integration
    /// test; runs on real macOS hardware only.
    #[test]
    #[cfg(target_os = "macos")]
    fn default_sandbox_reaches_launch_target_under_home_when_cwd_is_elsewhere() {
        let home = std::env::var("HOME").expect("HOME must be set");
        let target_dir =
            std::path::Path::new(&home).join(format!("glass-sbx-target-{}", std::process::id()));
        std::fs::create_dir_all(&target_dir).expect("create target dir under $HOME");
        // A copy of the probe binary itself, not a `#!/bin/sh` script: the interpreter named by
        // a shebang is what actually gets exec'd and injected into, so a script would hit the
        // same arm64e mismatch `sandbox_probe_path`'s doc comment describes for `/bin/sh`.
        // `fs::copy` preserves the source's executable bit, so no `chmod` is needed here.
        let target_path = target_dir.join("glass-macos-sandbox-probe");
        std::fs::copy(sandbox_probe_path(), &target_path)
            .expect("copy sandbox-probe fixture under $HOME");
        let sentinel = format!("GLASS_SBX_SENTINEL_{}", std::process::id());

        // Drop guard so the dir under $HOME is removed even if an assertion below panics.
        struct Cleanup(std::path::PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_dir_all(&self.0);
            }
        }
        let _cleanup = Cleanup(target_dir.clone());

        let target_str = target_path.to_str().expect("target path is valid UTF-8");
        let mut launch = spec(&[target_str, "--print", sentinel.as_str()]);
        launch.sandbox = SandboxLevel::Default;
        // Deliberately OUTSIDE $HOME: proves the launch target is reachable via its OWN
        // re-allow, not merely because it happens to sit under a reallowed cwd.
        launch.cwd = Some(std::env::temp_dir());

        let logs = empty_sink();
        let Launch {
            mut child,
            clip,
            taps: _taps,
        } = spawn(&launch, logs.clone(), &[]).unwrap_or_else(|e| {
            panic!(
                "sandboxed spawn of a launch target under $HOME (cwd outside $HOME) should \
                 succeed: {e}"
            )
        });
        // See default_sandbox_runs_but_contains_filesystem's identical assertion: the copied
        // probe is always injectable, so `None` means the shim never resolved, not that the
        // reachability behaviour below is untested.
        assert!(
            clip.is_some(),
            "clip shim was not injected into the sandboxed probe; is glass-clip-shim-macos built?"
        );
        child.wait().expect("wait");
        std::thread::sleep(Duration::from_millis(100));

        let out: Vec<String> = logs
            .lock()
            .expect("sink")
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        assert!(
            out.iter().any(|l| l == &sentinel),
            "launch target under $HOME must be reachable (read + exec'd) even when cwd is \
             outside $HOME: {out:?}"
        );
    }

    struct ArtifactProbeFixture {
        process_dir: PathBuf,
        marker: PathBuf,
        lease: PathBuf,
        marker_text: String,
    }

    impl ArtifactProbeFixture {
        fn new() -> Self {
            let nonce = format!(
                "{}-{}",
                std::process::id(),
                CLIP_TOKEN.fetch_add(1, Ordering::Relaxed)
            );
            let root = std::env::temp_dir().join("glass-macos-artifact-probe");
            let process_dir = root.join(&nonce);
            let lease = root.join(format!("{nonce}.lease"));
            let marker = process_dir.join("marker");
            let marker_text = format!("artifact-marker-{nonce}");
            std::fs::create_dir_all(&process_dir).expect("create artifact process directory");
            std::fs::write(&marker, &marker_text).expect("write artifact marker");
            std::fs::write(&lease, "lease").expect("write artifact lease");
            Self {
                process_dir,
                marker,
                lease,
                marker_text,
            }
        }
    }

    impl Drop for ArtifactProbeFixture {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.process_dir);
            let _ = std::fs::remove_file(&self.lease);
        }
    }

    #[test]
    #[ignore = "requires macOS Seatbelt"]
    #[cfg(target_os = "macos")]
    fn protected_artifact_and_lease_are_denied_on_box() {
        let fixture = ArtifactProbeFixture::new();
        assert_eq!(
            std::fs::read_to_string(&fixture.marker).unwrap(),
            fixture.marker_text
        );
        assert_eq!(std::fs::read_to_string(&fixture.lease).unwrap(), "lease");
        let probe = sandbox_probe_path();
        let probe_arg = probe
            .to_str()
            .expect("sandbox probe path is valid UTF-8")
            .to_owned();
        for level in [SandboxLevel::Default, SandboxLevel::Strict] {
            let mut launch = spec(&[
                &probe_arg,
                "--protected-reads",
                "ARTIFACT_MARKER",
                "ARTIFACT_LEASE",
            ]);
            launch.sandbox = level;
            launch.env = vec![
                (
                    "ARTIFACT_MARKER".into(),
                    fixture
                        .marker
                        .to_str()
                        .expect("marker path is valid UTF-8")
                        .into(),
                ),
                (
                    "ARTIFACT_LEASE".into(),
                    fixture
                        .lease
                        .to_str()
                        .expect("lease path is valid UTF-8")
                        .into(),
                ),
            ];
            let protected = [
                ProtectedHostPath::directory(&fixture.process_dir),
                ProtectedHostPath::file(&fixture.lease),
            ];
            let Launch { mut child, .. } =
                spawn(&launch, empty_sink(), &protected).expect("spawn protected artifact probe");
            assert!(child.wait().expect("wait for artifact probe").success());
        }
        assert_eq!(
            std::fs::read_to_string(&fixture.marker).unwrap(),
            fixture.marker_text
        );
        assert_eq!(std::fs::read_to_string(&fixture.lease).unwrap(), "lease");
    }

    /// glass#477: the launch's readers are the caller's to end, and the last thing the app said
    /// survives that ending.
    ///
    /// **What this covers and what it does not.** Not the fd release: that assertion reads
    /// `/proc/self/fd` and is Linux-gated. Not the final drain either — `child.wait()` below
    /// returns long after the reader has had the line through its ordinary loop, so ablating the
    /// drain leaves this green, and `glass-pipe-unix` drives that function directly instead. What
    /// this pins is the wiring: `spawn` hands the taps back rather than detaching them, and
    /// stopping them joins rather than losing what the app already said.
    #[test]
    #[cfg(target_os = "macos")]
    fn a_launch_hands_back_taps_that_catch_the_apps_last_line() {
        let logs = empty_sink();
        // The sleeper inherits both pipes and outlives the child, so EOF is not a deadline the
        // readers can reach: `drop(taps)` has to be what ends them.
        let Launch {
            mut child, taps, ..
        } = spawn(
            &spec(&["/bin/sh", "-c", "echo 'the last line'; sleep 30 &"]),
            Arc::clone(&logs),
            &[],
        )
        .expect("spawn /bin/sh");

        assert_eq!(
            taps.len(),
            2,
            "both streams must be tapped, and handed back"
        );

        // Waited for, not slept past: the child exits once it has backgrounded the sleeper, which
        // is what makes "everything it wrote is already in the pipe" true rather than a race.
        // `terminate` is then the no-op it is on an app that left on its own, in teardown's order.
        child.wait().expect("the child exits, the sleeper does not");
        terminate(&mut child);
        // Reaped first, so each tap's final drain sees what the app wrote on the way out.
        drop(taps);

        let said: Vec<String> = logs
            .lock()
            .expect("log sink mutex")
            .iter()
            .map(|(_, line)| line.clone())
            .collect();
        assert!(
            said.iter().any(|line| line == "the last line"),
            "stopping the taps must join, not lose what the app already wrote: {said:?}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn spawn_pipes_stdout_and_stderr_lines() {
        let logs = empty_sink();
        let Launch {
            mut child,
            taps: _taps,
            ..
        } = spawn(
            &spec(&["/bin/sh", "-c", "echo out; echo err 1>&2"]),
            logs.clone(),
            &[],
        )
        .expect("spawn /bin/sh");
        child.wait().expect("wait for /bin/sh to exit");
        // The reader threads finish shortly after the child's fds close on exit; give them
        // a moment rather than racing the drain against them.
        std::thread::sleep(Duration::from_millis(100));

        let lines = logs.lock().expect("log sink mutex").clone();
        assert!(
            lines.contains(&(Stream::Stdout, "out".to_string())),
            "{lines:?}"
        );
        assert!(
            lines.contains(&(Stream::Stderr, "err".to_string())),
            "{lines:?}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn spawn_missing_program_returns_app_not_started() {
        let err = spawn(&spec(&["/no/such/glass-test-binary"]), empty_sink(), &[])
            .expect_err("missing program must fail to spawn");
        assert!(
            matches!(err, GlassError::AppNotStarted(_)),
            "expected AppNotStarted, got {err:?}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn ad_hoc_signed_code_is_not_apple_platform_code() {
        // The fail-open direction, and the one that matters for a glass user: their own app must
        // keep the direct-spawn path (piped logs, containment) and never be diverted to the
        // LaunchServices handoff. Ad-hoc signing a copy of a system binary strips the platform
        // signature, which is exactly the difference being detected.
        let dir = std::env::temp_dir().join(format!("glass-platform-code-{}", std::process::id()));
        std::fs::create_dir_all(&dir).expect("create temp dir");
        let copy = dir.join("echo");
        std::fs::copy("/bin/echo", &copy).expect("copy /bin/echo");
        let signed = Command::new("codesign")
            .args(["-f", "-s", "-"])
            .arg(&copy)
            .status()
            .expect("run codesign");
        assert!(signed.success(), "ad-hoc signing the copy failed");

        let is_platform = is_apple_platform_code(&copy);
        let _ = std::fs::remove_dir_all(&dir);
        assert!(
            !is_platform,
            "ad-hoc signed code must not be treated as Apple platform code"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "on-device only: reads the signatures of this machine's stock apps"]
    fn stock_apps_are_apple_platform_code() {
        // Both of these are platform code; only the first is killed when direct-spawned. That
        // asymmetry is the whole reason `start_bundle` acts on "is this OS code" rather than
        // trying to predict the kill — see its doc.
        for app in [
            "/System/Applications/TextEdit.app",
            "/System/Applications/Utilities/Terminal.app",
        ] {
            assert!(
                is_apple_platform_code(Path::new(app)),
                "{app} should be recognized as Apple platform code"
            );
        }
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn terminate_kills_a_long_running_child() {
        let Launch { mut child, .. } =
            spawn(&spec(&["/bin/sleep", "100"]), empty_sink(), &[]).expect("spawn /bin/sleep");
        terminate(&mut child);
        let status = child.try_wait().expect("try_wait after terminate");
        assert!(status.is_some(), "child should have exited after terminate");
    }

    /// How long the AppKit fixture gets to finish launching before it is torn down. Until it
    /// is a *running application* there is nothing to ask, so terminating early would exercise
    /// the signal fallback and quietly pass for the wrong reason.
    #[cfg(target_os = "macos")]
    const FIXTURE_LAUNCH_SETTLE: Duration = Duration::from_secs(3);

    /// How long the reader thread gets to drain the fixture's closing pipe. `terminate`
    /// returns once the child is reaped; the last line it printed may still be in flight.
    #[cfg(target_os = "macos")]
    const LOG_DRAIN_SETTLE: Duration = Duration::from_millis(500);

    /// Build `fixture/quadrants.swift` to a fresh temp path, returning (binary, build dir).
    #[cfg(target_os = "macos")]
    fn build_quit_fixture(tag: &str) -> (PathBuf, PathBuf) {
        let source = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("fixture")
            .join("quadrants.swift");
        let build_dir =
            std::env::temp_dir().join(format!("glass-quit-fixture-{tag}-{}", std::process::id()));
        std::fs::create_dir_all(&build_dir).expect("create fixture build dir");
        let fixture = build_dir.join("quit-fixture");
        let status = Command::new("swiftc")
            .arg("-O")
            // `-parse-as-library`: the fixture declares its entry point with `@main`, which
            // swiftc rejects in a file it would otherwise treat as a top-level script. Same
            // flags `tests/capture.rs` builds this fixture with.
            .arg("-parse-as-library")
            .arg(&source)
            .arg("-o")
            .arg(&fixture)
            .status()
            .expect("run swiftc");
        assert!(status.success(), "swiftc failed on {}", source.display());
        (fixture, build_dir)
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "on-device only: needs swiftc and a window-server session"]
    fn terminate_asks_a_gui_app_to_quit_before_signalling_it() {
        // The whole point of the quit request: AppKit installs no SIGTERM handler, so a
        // signalled Cocoa app never runs `applicationWillTerminate:`. The fixture announces
        // that method on stdout, so the captured line exists only if `terminate` asked first —
        // no signal-only teardown can produce it.
        //
        // `#[ignore]`d rather than self-skipping on an env var: it needs `swiftc` to build the
        // fixture and a real window-server session for that fixture to become a running
        // application, so on a CI runner it would report a pass having asserted nothing.
        // `scripts/test-macos.sh` runs it explicitly under `GLASS_MACOS_ONBOX=1`.

        let (fixture, build_dir) = build_quit_fixture("ask");
        let sink = empty_sink();
        let Launch {
            mut child,
            taps: _taps,
            ..
        } = spawn(
            &spec(&[fixture.to_string_lossy().as_ref()]),
            Arc::clone(&sink),
            &[],
        )
        .expect("spawn the AppKit fixture");
        std::thread::sleep(FIXTURE_LAUNCH_SETTLE);

        terminate(&mut child);
        std::thread::sleep(LOG_DRAIN_SETTLE);

        let logs = sink.lock().expect("log sink mutex").clone();
        let _ = std::fs::remove_dir_all(&build_dir);
        assert!(
            logs.iter()
                .any(|(stream, line)| *stream == Stream::Stdout && line.starts_with("quit: ")),
            "terminate must ask the app to quit before signalling it; captured logs: {logs:?}"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    #[ignore = "on-device only: needs swiftc and a window-server session"]
    fn terminate_finishes_off_an_app_that_refuses_to_quit() {
        // `terminate`'s doc promises that an app which vetoes the request still gets the full
        // signal ladder, so teardown always completes — the one claim on this path that a
        // cooperating fixture cannot exercise. `GLASS_FIXTURE_VETO_QUIT` makes the fixture
        // answer `NSTerminateCancel`, standing in for an unsaved-changes sheet.
        //
        // The elapsed bound is the load-bearing half: it pins the ladder inside the budget
        // `glass-mcp` allows teardown. Overrun it and `glass_stop` is abandoned mid-wait with
        // the app never signalled at all — orphaned to launchd, outliving glass itself.
        let (fixture, build_dir) = build_quit_fixture("veto");
        let mut spec = spec(&[fixture.to_string_lossy().as_ref()]);
        spec.env = vec![("GLASS_FIXTURE_VETO_QUIT".to_string(), "1".to_string())];

        let sink = empty_sink();
        let Launch {
            mut child,
            taps: _taps,
            ..
        } = spawn(&spec, Arc::clone(&sink), &[]).expect("spawn the AppKit fixture");
        std::thread::sleep(FIXTURE_LAUNCH_SETTLE);

        let started = Instant::now();
        terminate(&mut child);
        let elapsed = started.elapsed();
        std::thread::sleep(LOG_DRAIN_SETTLE);
        let logs = sink.lock().expect("log sink mutex").clone();
        let _ = std::fs::remove_dir_all(&build_dir);

        assert!(
            logs.iter().any(
                |(stream, line)| *stream == Stream::Stdout && line.starts_with("quit: refused")
            ),
            "the fixture must actually have refused, or this proves nothing; captured: {logs:?}"
        );
        assert!(
            child
                .try_wait()
                .expect("try_wait after terminate")
                .is_some(),
            "a vetoing app must still be gone after terminate"
        );
        assert!(
            elapsed < TEARDOWN_BUDGET,
            "terminate took {elapsed:?}, over the {TEARDOWN_BUDGET:?} glass-mcp allows teardown \
             — past that the app is left running and glass exits without it"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn terminate_signals_a_non_gui_child_without_waiting_out_the_quit_grace() {
        // `/bin/sleep` is not a running *application*, so there is nothing to ask and
        // `terminate_app` reports that. The quit grace must then be skipped entirely rather
        // than spent — otherwise asking first would add `QUIT_GRACE` to the teardown of every
        // app glass cannot ask, which is every console-shaped one.
        let Launch { mut child, .. } =
            spawn(&spec(&["/bin/sleep", "100"]), empty_sink(), &[]).expect("spawn /bin/sleep");
        let started = Instant::now();
        terminate(&mut child);
        let elapsed = started.elapsed();
        assert!(
            child
                .try_wait()
                .expect("try_wait after terminate")
                .is_some(),
            "child should have exited after terminate"
        );
        assert!(
            elapsed < QUIT_GRACE,
            "terminate took {elapsed:?} — it waited out the quit grace ({QUIT_GRACE:?}) for a \
             process it could not ask"
        );
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn terminate_is_idempotent_on_an_already_exited_child() {
        let Launch { mut child, .. } =
            spawn(&spec(&["/bin/echo", "hi"]), empty_sink(), &[]).expect("spawn /bin/echo");
        child.wait().expect("wait for /bin/echo to exit");
        // Already reaped; terminate must not panic or hang.
        terminate(&mut child);
        terminate(&mut child);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_true_for_unsigned() {
        // Unsigned: codesign exits non-zero, but the "not signed at all" marker is an
        // unambiguous injectable signal, so the exit status isn't gated for this case.
        assert!(injectable_from_codesign_report(
            "TestApp: code object is not signed at all\n",
            false,
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_true_for_adhoc_flags_line() {
        // Adhoc/linker-signed: codesign succeeds and the flags line lacks `runtime`. This is
        // the observed adhoc line shape.
        assert!(injectable_from_codesign_report(
            "CodeDirectory v=20400 size=91 flags=0x20002(adhoc,linker-signed) hashes=3+3 location=embedded\n",
            true,
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_false_when_hardened_runtime_flag_present() {
        assert!(!injectable_from_codesign_report(
            "CodeDirectory v=20500 size=634 flags=0x10000(runtime) hashes=13+3 location=embedded\n",
            true,
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_false_for_empty_or_garbage() {
        // Fail-closed: no recognized non-hardened signal (no marker, no `flags=` line) →
        // non-injectable, whether codesign succeeded or not.
        assert!(!injectable_from_codesign_report("", true));
        assert!(!injectable_from_codesign_report("", false));
        assert!(!injectable_from_codesign_report(
            "garbage output with no flags line",
            true
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_false_for_a_non_runtime_error_string() {
        // The exact regression this fix closes: an error whose text lacks BOTH the "not
        // signed" marker AND a successful `flags=` line must fail closed. The old
        // `!contains("runtime")` shape wrongly returned `true` here (fail-OPEN).
        assert!(!injectable_from_codesign_report(
            "test-binary: a codesign error that happens not to mention the h-word\n",
            false,
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_false_when_marker_only_in_path() {
        // A genuinely hardened, successfully-signed target whose PATH happens to contain the
        // unsigned marker (codesign echoes `Executable=<path>`, and the path is the
        // caller-chosen `spec.run[0]`) must still fail closed — the marker in path text must
        // not override a real `flags=(...runtime...)` line.
        let report = "Executable=/tmp/code object is not signed at all/App.app/Contents/MacOS/App\n\
                      CodeDirectory v=20400 flags=0x10000(runtime) hashes=3+3 location=embedded\n";
        assert!(!injectable_from_codesign_report(report, true));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_false_when_flags_line_present_but_status_failed() {
        // A non-runtime `flags=` line is trusted only when codesign actually SUCCEEDED; the
        // same line on a non-zero exit is unrecognized output → fail-closed.
        assert!(!injectable_from_codesign_report(
            "CodeDirectory v=20400 size=91 flags=0x2(adhoc) hashes=3+3 location=embedded\n",
            false,
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shim_dylib_path_uses_an_explicit_env_override_that_exists() {
        // The override tier is existence-checked like the exe-dir/target-dir fallback tiers
        // below it (fail-closed, uniformly) — a real file at the override path is returned.
        let dir = std::env::temp_dir();
        let dylib = dir.join(format!("glass-clip-shim-test-{}.dylib", std::process::id()));
        std::fs::write(
            &dylib,
            b"stand-in for the real shim dylib; only existence matters here",
        )
        .expect("write stand-in dylib file");
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(dylib.clone());

        assert_eq!(shim_dylib_path_with(Some(dylib.clone())), Some(dylib));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shim_dylib_path_falls_through_a_nonexistent_env_override() {
        // Unlike the old (fail-open) behavior, a bad override — stale env, typo'd path — must
        // not be trusted blind: it falls through to the remaining tiers (or `None`) rather
        // than handing dyld an unopenable path, same fail-closed discipline as those tiers.
        let bogus = PathBuf::from("/nonexistent/glass-clip-shim-test.dylib");
        assert_ne!(
            shim_dylib_path_with(Some(bogus.clone())),
            Some(bogus),
            "a nonexistent override must not be returned as-is"
        );
    }
}
