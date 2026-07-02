//! App spawn, background log-piping, and terminate for the macOS backend.
//!
//! Mirrors `glass-x11`/`glass-wayland`'s `spawn`+`spawn_reader`+`LogSink` shape: a plain
//! `std::process::Command` built from [`AppSpec`], stdout/stderr piped into a shared,
//! `Arc<Mutex<_>>`-guarded log buffer via one reader thread per stream, so
//! `MacosPlatform::drain_logs` can read it without blocking the readers. `terminate`
//! mirrors `glass-proc-linux::reap`'s SIGTERM -> brief wait -> SIGKILL -> reap sequence,
//! reimplemented here (rather than depending on that crate) because it is `/proc`-based
//! and therefore Linux-only.
//!
//! Window discovery is a separate concern ([`crate::scwindow::find_window_for_pids`]);
//! this module only owns the process lifecycle.
//!
//! Clip-shim injection: a contained launch whose target is not hardened-runtime signed
//! ([`target_is_injectable`]) gets `glass-clip-shim-macos`'s built dylib
//! ([`shim_dylib_path`]) loaded via `DYLD_INSERT_LIBRARIES`, plus a per-spawn private
//! pasteboard name in `GLASS_CLIP_PASTEBOARD` — both set on the `Command` before
//! `cmd.spawn()` in [`spawn`]. `Command`'s envp is applied at the `exec` that follows
//! `pre_exec`'s `sandbox_init` call (not at fork/`pre_exec` time), so both vars are present
//! in the launched app's environment, having survived the sandbox. [`ClipLaunch`] carries
//! those facts back to `start_app`, which holds them until the launched window is
//! confirmed and the clipboard route can be decided (a later step; not this module's
//! concern).

use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::process::{kill_process, Pid, Signal};

use glass_core::platform::{AppSpec, SandboxLevel};
use glass_core::{GlassError, Result, Stream};
use glass_sandbox_macos::{build_profile, ProfileOpts};

/// Per-spawn counter seeding [`crate::clipboard_route::session_pasteboard_name`] — starts
/// at 1 (not 0) purely so a default-initialized `u64` elsewhere in the codebase can never
/// be mistaken for a real token; the exact starting value has no other significance.
static CLIP_TOKEN: AtomicU64 = AtomicU64::new(1);

/// Clip-shim facts for one contained, injectable launch: the private pasteboard name the
/// shim redirects `NSPasteboard.generalPasteboard` to, and whether injection was attempted
/// (`true` whenever [`spawn`] returns `Some` — see its doc). `start_app` holds this until
/// the launched window is confirmed, then uses it to decide clipboard routing (a later,
/// separate step).
///
/// Neither field is read yet (only constructed and moved through `MacosPlatform::clip`) —
/// that later step is what reads them; `expect` rather than a blanket `allow` so this
/// attribute itself starts failing the build once that step lands and actually reads them,
/// as a forcing function to remove it then.
#[expect(dead_code, reason = "read by clipboard routing, which lands in a later, separate step")]
pub(crate) struct ClipLaunch {
    pub name: String,
    pub injectable: bool,
}

/// True iff `stderr` — `codesign --display --verbose=2`'s report — shows no hardened
/// runtime, i.e. `DYLD_INSERT_LIBRARIES` injection can take on this target. Factored out of
/// [`target_is_injectable`] as a pure string check so the decision itself is unit-testable
/// without shelling out to `codesign`.
fn injectable_from_codesign_report(stderr: &str) -> bool {
    !stderr.contains("runtime")
}

/// True iff `program` is not hardened-runtime signed, so injecting the clip shim via
/// `DYLD_INSERT_LIBRARIES` can take. Shells out to `codesign --display --verbose=2`
/// (codesign writes its report to stderr, not stdout) rather than linking a
/// Security-framework binding — simplest option, no new framework dependency.
///
/// Conservative and fail-closed: any uncertainty (`codesign` missing or unspawnable, its
/// output not valid UTF-8) reports `false` (non-injectable), never `true` — an unsigned or
/// adhoc-signed binary reports `false` from codesign's own exit status, but its stderr
/// still won't mention `runtime`, so [`injectable_from_codesign_report`] correctly reports
/// `true` for it regardless of that exit status.
fn target_is_injectable(program: &Path) -> bool {
    let Ok(output) = Command::new("codesign")
        .arg("--display")
        .arg("--verbose=2")
        .arg(program)
        .output()
    else {
        return false;
    };
    let Ok(stderr) = String::from_utf8(output.stderr) else {
        return false;
    };
    injectable_from_codesign_report(&stderr)
}

/// Env var overriding [`shim_dylib_path`]'s resolution — tests and non-standard layouts.
const SHIM_DYLIB_ENV: &str = "GLASS_CLIP_SHIM_DYLIB";

/// File name of the shim's build artifact: `glass-clip-shim-macos`'s `crate-type =
/// ["cdylib"]` compiles to `lib<crate name, underscored>.dylib` on macOS.
const SHIM_DYLIB_NAME: &str = "libglass_clip_shim_macos.dylib";

/// Resolve the injected clip shim's dylib: [`SHIM_DYLIB_ENV`] → next to the running
/// executable → the cargo target dir one level up from it (`current_exe` is
/// `target/<profile>/<bin>` for a normal build, or `target/<profile>/deps/<bin>-<hash>`
/// under `cargo test`, one directory deeper than the shim's own build output — hence the
/// second candidate). Every tier, including the env override, is existence-checked
/// (`.is_file()`) before being returned — a bad override (stale/typo'd path) falls through
/// to the remaining tiers rather than being trusted blind, same fail-closed discipline as
/// the rest of this resolution. `None` if none of these exist: callers treat that as "not
/// injectable" (fail-closed — no resolvable shim, no injection).
fn shim_dylib_path() -> Option<PathBuf> {
    if let Ok(path) = std::env::var(SHIM_DYLIB_ENV) {
        let path = PathBuf::from(path);
        if path.is_file() {
            return Some(path);
        }
    }
    let exe_dir = std::env::current_exe().ok()?.parent()?.to_path_buf();
    let next_to_exe = exe_dir.join(SHIM_DYLIB_NAME);
    if next_to_exe.is_file() {
        return Some(next_to_exe);
    }
    let target_dir = exe_dir.parent()?.join(SHIM_DYLIB_NAME);
    target_dir.is_file().then_some(target_dir)
}

/// Log lines captured by the per-stream reader threads spawned in [`spawn`], drained by
/// `MacosPlatform::drain_logs`. `Arc<Mutex<_>>` (not a bare `Vec`) because the reader
/// threads outlive `spawn`'s call and push into it concurrently with `drain_logs` reading
/// it from the main thread — the same shape `glass-x11`/`glass-wayland` use.
pub(crate) type LogSink = Arc<Mutex<Vec<(Stream, String)>>>;

/// How long [`terminate`] waits after SIGTERM before escalating to SIGKILL. Short: a
/// terminate call is already the "shut it down" path (`stop_app` or a failed launch's
/// cleanup), not a place to make the caller wait for a slow shutdown handler.
const TERMINATE_GRACE: Duration = Duration::from_millis(500);

/// Spawn `spec.run` (with `spec.cwd`/`spec.env` applied) with stdout/stderr piped into
/// `logs` via one reader thread per stream. Returns [`GlassError::AppNotStarted`] if the
/// program can't be launched (e.g. not found, not executable).
///
/// macOS process containment: [`SandboxLevel::Default`]/[`SandboxLevel::Strict`] apply a
/// generated Seatbelt (`sandbox_init`) profile to the launched app via a fork-safe
/// `pre_exec` (see below). [`SandboxLevel::Off`] spawns unchanged.
///
/// The second return value is the clip-shim launch facts ([`ClipLaunch`]), `Some` only for
/// a contained launch whose target is injectable (see [`target_is_injectable`]) and whose
/// shim dylib resolved (see [`shim_dylib_path`]); `None` for `SandboxLevel::Off` or a
/// non-injectable/unresolved target. The caller (`MacosPlatform::start_app`) holds it for a
/// later clipboard-routing decision — this function only sets up the injection.
pub(crate) fn spawn(spec: &AppSpec, logs: LogSink) -> Result<(Child, Option<ClipLaunch>)> {
    let mut cmd = Command::new(&spec.run[0]);

    // Containment: for Default/Strict, apply a generated Seatbelt profile to the launched app
    // in a fork-safe pre_exec (build the CString here, before fork; the closure only makes the
    // sandbox_init syscall). Off spawns unchanged. Build (run_build) is never contained.
    let mut clip: Option<ClipLaunch> = None;
    if spec.sandbox != SandboxLevel::Off {
        // Resolve to absolute paths: a relative `(subpath ".")` never matches the child's real
        // cwd, and `build_profile`'s guard needs absolute paths to reason about home exposure.
        let cwd = spec
            .cwd
            .clone()
            .or_else(|| std::env::current_dir().ok())
            .unwrap_or_else(|| PathBuf::from("/"));
        let cwd = std::fs::canonicalize(&cwd).unwrap_or(cwd);
        let program =
            std::fs::canonicalize(&spec.run[0]).unwrap_or_else(|_| PathBuf::from(&spec.run[0]));

        // Decide injection before building the profile: `allow_pasteboard` depends on it.
        // `dylib_path` is resolved once and reused below (rather than a second
        // `shim_dylib_path()` call) so a transient filesystem hiccup between the two checks
        // can't make `injectable` and the later `.expect` disagree.
        let dylib_path = shim_dylib_path();
        let injectable = target_is_injectable(&program) && dylib_path.is_some();
        let allow_pasteboard = injectable;
        // The shim dylib's parent dir, re-allowed for read in the profile below when
        // injecting (`None` — no re-allow — otherwise, matching unchanged pre-injection
        // behavior).
        let mut shim_dir: Option<PathBuf> = None;
        if injectable {
            let token = CLIP_TOKEN.fetch_add(1, Ordering::Relaxed);
            let name = crate::clipboard_route::session_pasteboard_name(token);
            let dylib = dylib_path.expect("checked Some above");
            // The shim dylib lives in glass's own `target/<profile>/` tree, typically under
            // $HOME — which the profile below denies by default (see `build_profile`'s
            // `/Users` deny). dyld loads the shim AFTER `sandbox_init` applies the profile
            // (in `pre_exec`, below), so its directory must be re-allowed for read here or
            // dyld can't open it, injection silently fails, and the sandboxed app never sees
            // the shim.
            shim_dir = dylib.parent().map(Path::to_path_buf);
            // Set BEFORE `cmd.spawn()`: `Command`'s envp is applied at the `exec` that
            // follows `pre_exec`'s `sandbox_init` call (not at fork/`pre_exec` time), so both
            // vars are present in the launched app's environment, having survived the
            // sandbox — same timing guarantee the profile CString relies on.
            cmd.env("DYLD_INSERT_LIBRARIES", &dylib);
            cmd.env("GLASS_CLIP_PASTEBOARD", &name);
            clip = Some(ClipLaunch {
                name,
                injectable: true,
            });
        }

        // Pasteboard is allowed only for an injectable target (the shim's redirect is the
        // actual isolation there); a hardened/non-injectable target keeps it denied, same as
        // before this task.
        let opts = ProfileOpts {
            cwd,
            program,
            ro_binds: shim_dir.into_iter().collect(),
            rw_binds: vec![],
            allow_pasteboard,
        };
        let profile = build_profile(spec.sandbox, &opts);
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
    for (k, v) in &spec.env {
        cmd.env(k, v);
    }
    cmd.stdout(Stdio::piped());
    cmd.stderr(Stdio::piped());

    let mut child = cmd.spawn().map_err(|e| {
        // A PermissionDenied under containment could be `sandbox_init` rejecting the profile in
        // pre_exec, OR a plain EACCES on a non-executable binary — the two are indistinguishable
        // from this `io::Error` alone. Surface the actionable SandboxUnavailable either way
        // (the failure is real regardless of cause: fail-closed, never unconfined), but don't
        // overclaim which one it was.
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
    spawn_reader(stdout, Stream::Stdout, logs.clone());
    spawn_reader(stderr, Stream::Stderr, logs);

    Ok((child, clip))
}

/// Pipe a child stream's lines into the shared log sink on a background thread. Exits
/// quietly (no error surfaced — this is a best-effort log tap, not the app's lifecycle)
/// once the stream hits EOF (the child closed it, typically by exiting) or a read fails.
fn spawn_reader<R: std::io::Read + Send + 'static>(reader: R, stream: Stream, sink: LogSink) {
    std::thread::spawn(move || {
        for line in BufReader::new(reader).lines() {
            match line {
                Ok(text) => sink.lock().expect("log sink mutex").push((stream, text)),
                Err(_) => break,
            }
        }
    });
}

/// Idempotently terminate `child`: SIGTERM, wait up to [`TERMINATE_GRACE`] for exit, then
/// SIGKILL, then reap. Safe to call on an already-exited (or already-terminated) child —
/// `try_wait` is checked first so a second call never re-signals a pid the kernel may have
/// since recycled.
pub(crate) fn terminate(child: &mut Child) {
    if matches!(child.try_wait(), Ok(Some(_))) {
        return;
    }

    let pid = Pid::from_child(child);
    let _ = kill_process(pid, Signal::TERM);

    let deadline = Instant::now() + TERMINATE_GRACE;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => return,
            Ok(None) if Instant::now() >= deadline => break,
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            // `try_wait` failing is unexpected (the pid is ours) but not a reason to spin
            // forever — fall through to the SIGKILL/reap below.
            Err(_) => break,
        }
    }

    let _ = child.kill(); // SIGKILL, tolerates an already-exited child.
    let _ = child.wait(); // Reap so the child doesn't linger as a zombie.
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
        // `cat` failure can only mean the sandbox denied it, never that the path was absent.
        let home = std::env::var("HOME").expect("HOME must be set");
        let proj = std::path::Path::new(&home).join(format!("glass-sbx-cwd-{}", std::process::id()));
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
        let _cleanup = Cleanup { secret: secret_path.clone(), proj: proj.clone() };
        let proj_str = proj.to_str().expect("project path is valid UTF-8");
        let secret = secret_path.to_str().expect("secret path is valid UTF-8");
        let shell_cmd = format!(
            "cat /usr/lib/dyld >/dev/null 2>&1 && echo SYS_OK; \
             cat \"{proj_str}/probe\" >/dev/null 2>&1 && echo CWD_OK; \
             cat \"{secret}\" >/dev/null 2>&1 && echo HOME_READABLE || echo HOME_DENIED",
        );

        let mut denied = spec(&["/bin/sh", "-c", shell_cmd.as_str()]);
        denied.sandbox = SandboxLevel::Default;
        denied.cwd = Some(proj.clone());
        let logs = empty_sink();
        let (mut child, _clip) =
            spawn(&denied, logs.clone()).unwrap_or_else(|e| panic!("sandboxed spawn should succeed: {e}"));
        child.wait().expect("wait");
        std::thread::sleep(Duration::from_millis(100));
        let out: Vec<String> = logs
            .lock()
            .expect("sink")
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        assert!(out.iter().any(|l| l == "SYS_OK"), "whole-FS read should be allowed: {out:?}");
        assert!(out.iter().any(|l| l == "CWD_OK"), "cwd under home should be reallowed: {out:?}");
        assert!(out.iter().any(|l| l == "HOME_DENIED"), "home read must be denied: {out:?}");
        assert!(!out.iter().any(|l| l == "HOME_READABLE"), "home leaked: {out:?}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn spawn_pipes_stdout_and_stderr_lines() {
        let logs = empty_sink();
        let (mut child, _clip) = spawn(&spec(&["/bin/sh", "-c", "echo out; echo err 1>&2"]), logs.clone())
            .expect("spawn /bin/sh");
        child.wait().expect("wait for /bin/sh to exit");
        // The reader threads finish shortly after the child's fds close on exit; give them
        // a moment rather than racing the drain against them.
        std::thread::sleep(Duration::from_millis(100));

        let lines = logs.lock().expect("log sink mutex").clone();
        assert!(lines.contains(&(Stream::Stdout, "out".to_string())), "{lines:?}");
        assert!(lines.contains(&(Stream::Stderr, "err".to_string())), "{lines:?}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn spawn_missing_program_returns_app_not_started() {
        let err = spawn(&spec(&["/no/such/glass-test-binary"]), empty_sink())
            .expect_err("missing program must fail to spawn");
        assert!(matches!(err, GlassError::AppNotStarted(_)), "expected AppNotStarted, got {err:?}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn terminate_kills_a_long_running_child() {
        let (mut child, _clip) = spawn(&spec(&["/bin/sleep", "100"]), empty_sink()).expect("spawn /bin/sleep");
        terminate(&mut child);
        let status = child.try_wait().expect("try_wait after terminate");
        assert!(status.is_some(), "child should have exited after terminate");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn terminate_is_idempotent_on_an_already_exited_child() {
        let (mut child, _clip) = spawn(&spec(&["/bin/echo", "hi"]), empty_sink()).expect("spawn /bin/echo");
        child.wait().expect("wait for /bin/echo to exit");
        // Already reaped; terminate must not panic or hang.
        terminate(&mut child);
        terminate(&mut child);
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_true_for_unsigned_or_adhoc() {
        // codesign's report for an unsigned binary never mentions "runtime" at all.
        assert!(injectable_from_codesign_report(
            "TestApp: code object is not signed at all\n"
        ));
        // Nor does an adhoc/linker-signed binary's flags line.
        assert!(injectable_from_codesign_report(
            "CodeDirectory v=20400 size=91 flags=0x2(adhoc) hashes=3+3 location=embedded\n"
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn injectable_from_codesign_report_false_when_hardened_runtime_flag_present() {
        assert!(!injectable_from_codesign_report(
            "CodeDirectory v=20500 size=634 flags=0x10000(runtime) hashes=13+3 location=embedded\n"
        ));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shim_dylib_path_uses_an_explicit_env_override_that_exists() {
        // The override tier is existence-checked like the exe-dir/target-dir fallback tiers
        // below it (fail-closed, uniformly) — a real file at the override path is returned.
        let dir = std::env::temp_dir();
        let dylib = dir.join(format!("glass-clip-shim-test-{}.dylib", std::process::id()));
        std::fs::write(&dylib, b"stand-in for the real shim dylib; only existence matters here")
            .expect("write stand-in dylib file");
        struct Cleanup(PathBuf);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = std::fs::remove_file(&self.0);
            }
        }
        let _cleanup = Cleanup(dylib.clone());

        let previous = std::env::var(SHIM_DYLIB_ENV).ok();
        std::env::set_var(SHIM_DYLIB_ENV, &dylib);
        let resolved = shim_dylib_path();
        match previous {
            Some(v) => std::env::set_var(SHIM_DYLIB_ENV, v),
            None => std::env::remove_var(SHIM_DYLIB_ENV),
        }
        assert_eq!(resolved, Some(dylib));
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn shim_dylib_path_falls_through_a_nonexistent_env_override() {
        // Unlike the old (fail-open) behavior, a bad override — stale env, typo'd path — must
        // not be trusted blind: it falls through to the remaining tiers (or `None`) rather
        // than handing dyld an unopenable path, same fail-closed discipline as those tiers.
        let bogus = PathBuf::from("/nonexistent/glass-clip-shim-test.dylib");
        let previous = std::env::var(SHIM_DYLIB_ENV).ok();
        std::env::set_var(SHIM_DYLIB_ENV, &bogus);
        let resolved = shim_dylib_path();
        match previous {
            Some(v) => std::env::set_var(SHIM_DYLIB_ENV, v),
            None => std::env::remove_var(SHIM_DYLIB_ENV),
        }
        assert_ne!(resolved, Some(bogus), "a nonexistent override must not be returned as-is");
    }
}
