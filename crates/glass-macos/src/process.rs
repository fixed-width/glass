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

use std::ffi::CString;
use std::io::{BufRead, BufReader};
use std::os::unix::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use rustix::process::{kill_process, Pid, Signal};

use glass_core::platform::{AppSpec, SandboxLevel};
use glass_core::{GlassError, Result, Stream};
use glass_sandbox_macos::{build_profile, ProfileOpts};

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
pub(crate) fn spawn(spec: &AppSpec, logs: LogSink) -> Result<Child> {
    let mut cmd = Command::new(&spec.run[0]);

    // Containment: for Default/Strict, apply a generated Seatbelt profile to the launched app
    // in a fork-safe pre_exec (build the CString here, before fork; the closure only makes the
    // sandbox_init syscall). Off spawns unchanged. Build (run_build) is never contained.
    if spec.sandbox != SandboxLevel::Off {
        let opts = ProfileOpts {
            cwd: spec.cwd.clone().map(PathBuf::from).unwrap_or_else(|| PathBuf::from(".")),
            program: PathBuf::from(&spec.run[0]),
            ro_binds: vec![],
            rw_binds: vec![],
        };
        let profile = build_profile(spec.sandbox, &opts);
        let profile_c = CString::new(profile).map_err(|e| {
            GlassError::SandboxUnavailable(format!("sandbox profile contains NUL: {e}"))
        })?;
        // SAFETY: the closure is async-signal-safe — it makes a single `sandbox_init` syscall
        // via a pre-built CString and allocates nothing (see `apply_cstr`). It runs in the
        // forked child before exec.
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

    let mut child = cmd
        .spawn()
        .map_err(|e| GlassError::AppNotStarted(format!("spawn {:?}: {e}", spec.run)))?;

    // `Stdio::piped()` guarantees these are `Some` immediately after a successful spawn.
    let stdout = child.stdout.take().expect("stdout was piped");
    let stderr = child.stderr.take().expect("stderr was piped");
    spawn_reader(stdout, Stream::Stdout, logs.clone());
    spawn_reader(stderr, Stream::Stderr, logs);

    Ok(child)
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
        // A denied path (outside system/cwd — the user's home) must be unreadable under the
        // sandbox, while an allowed system read succeeds. Proves sandbox_init applied AND
        // that deny-default filesystem containment bites through the real spawn path.
        //
        // The secret file is written under $HOME (not a fixed name like `.ssh/known_hosts`,
        // which may not exist on a fresh CI runner) so the "denied" assertion is grounded in
        // a file that provably exists — a `cat` failure can only mean the sandbox denied it,
        // never that the path was simply absent.
        let home = std::env::var("HOME").expect("HOME must be set");
        let secret_path = std::path::Path::new(&home).join("glass-sbx-test-secret");
        std::fs::write(&secret_path, "top-secret").expect("write test secret under $HOME");
        let secret = secret_path.to_str().expect("secret path is valid UTF-8");
        let shell_cmd = format!(
            "cat /usr/lib/dyld >/dev/null 2>&1 && echo SYS_OK; \
             cat \"{secret}\" >/dev/null 2>&1 && echo HOME_READABLE || echo HOME_DENIED",
        );

        let mut denied = spec(&["/bin/sh", "-c", shell_cmd.as_str()]);
        denied.sandbox = SandboxLevel::Default;
        let logs = empty_sink();
        let spawn_result = spawn(&denied, logs.clone());
        let cleanup = || {
            let _ = std::fs::remove_file(&secret_path);
        };
        let mut child = match spawn_result {
            Ok(child) => child,
            Err(e) => {
                cleanup();
                panic!("sandboxed spawn should succeed: {e}");
            }
        };
        child.wait().expect("wait");
        cleanup();
        std::thread::sleep(Duration::from_millis(100));
        let out: Vec<String> = logs
            .lock()
            .expect("sink")
            .iter()
            .map(|(_, l)| l.clone())
            .collect();
        assert!(out.iter().any(|l| l == "SYS_OK"), "system read should be allowed: {out:?}");
        assert!(out.iter().any(|l| l == "HOME_DENIED"), "home read must be denied: {out:?}");
        assert!(!out.iter().any(|l| l == "HOME_READABLE"), "home leaked: {out:?}");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn spawn_pipes_stdout_and_stderr_lines() {
        let logs = empty_sink();
        let mut child = spawn(&spec(&["/bin/sh", "-c", "echo out; echo err 1>&2"]), logs.clone())
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
        let mut child = spawn(&spec(&["/bin/sleep", "100"]), empty_sink()).expect("spawn /bin/sleep");
        terminate(&mut child);
        let status = child.try_wait().expect("try_wait after terminate");
        assert!(status.is_some(), "child should have exited after terminate");
    }

    #[test]
    #[cfg(target_os = "macos")]
    fn terminate_is_idempotent_on_an_already_exited_child() {
        let mut child = spawn(&spec(&["/bin/echo", "hi"]), empty_sink()).expect("spawn /bin/echo");
        child.wait().expect("wait for /bin/echo to exit");
        // Already reaped; terminate must not panic or hang.
        terminate(&mut child);
        terminate(&mut child);
    }
}
