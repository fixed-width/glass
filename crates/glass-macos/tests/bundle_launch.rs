//! Mac-gated `.app`-bundle-launch integration test — the first real-bundle proof of
//! `MacosPlatform::start_app`'s `.app` branch (`backend.rs::start_bundle`): direct-spawn
//! the bundle's inner executable first, fall back to an `NSWorkspace` handoff only if that
//! stub exits before a window appears, and fail closed if the handoff would need
//! Seatbelt containment it can't provide (`bundle::handoff_gate`). Three checks, run in
//! sequence from one process:
//!
//! 1. **Foreground `.app`** (direct-spawn): a fresh `Demo.app` fixture (this file's own
//!    `build_demo_app`, compiling `fixture/quadrants.swift` into
//!    `Demo.app/Contents/MacOS/Demo`) direct-spawns successfully — `start_app(sandbox:
//!    Default)` returns `Ok`, and a click's `click: <x>,<y>` line comes back through
//!    `drain_logs`. That captured log line is the only externally-observable proof this
//!    launch went through `process::spawn`'s child path rather than the handoff path below:
//!    `self.child`/`self.adopted` are private to `glass-macos`, but only a direct-spawned
//!    child has its stdout/stderr piped into the log buffer at all (see check 2).
//! 2. **Handoff app** (`NSWorkspace` adopt): `/System/Applications/TextEdit.app` re-execs
//!    itself through LaunchServices, so its directly-spawned stub exits before a window
//!    appears and `start_bundle` hands off to `NSWorkspace` — `start_app(sandbox: Off)`
//!    returns `Ok`, `drain_logs` is empty (the adopted pid was never `process::spawn`ed, so
//!    nothing was ever piped), and a live `glass_a11y_macos::MacosA11y::snapshot` against the
//!    adopted window returns a non-empty AX tree. Also confirms `stop_app`'s terminate path:
//!    when this run's own launch was the fresh instance (no `TextEdit` already running
//!    beforehand), the adopted pid must actually be gone afterward, not orphaned.
//! 3. **Fail-closed**: the same handoff trigger as check 2, but with `sandbox: Default` —
//!    `bundle::handoff_gate` rejects any sandbox level except `Off` before the adoption ever
//!    happens, so `start_app` must return `Err(GlassError::AppNotStarted(_))`.
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "bundle_launch"` entry), for
//! the same reason as `tests/capture.rs`/`tests/a11y.rs`/`tests/input.rs`/`tests/windows.rs`:
//! `start_app`'s bundle branch reaches `ffi::app_kit_init()` (via `backend.rs`'s
//! `discover_window`/`discover_window_pid`), which requires the process's TRUE main thread
//! (`objc2::MainThreadMarker`) — libtest's per-`#[test]` worker threads can't provide that,
//! so this file defines its own `fn main()` instead, run directly rather than through
//! libtest. There is accordingly no `#[ignore]` attribute anywhere in this file — none of
//! the sibling mac-gated integration tests use one either; the gate is this file not being
//! built at all by a plain `cargo test -p glass-macos` (`scripts/test-macos.sh` restricts
//! that invocation to `--lib`), only on request (see that script's `GLASS_MACOS_ONBOX` gate
//! for the sibling tests).
//!
//! Needs the same two TCC grants as `tests/input.rs` (Screen Recording for window
//! discovery/capture, Accessibility for the AX snapshot in check 2), held by the signed,
//! granted `GlassProbe.app` bundle on this project's dev Mac (`mini`) — same granted-run
//! procedure as `capture.rs`/`input.rs`: copy this built test binary into the bundle,
//! re-sign, run via a `gui/501` LaunchAgent so it inherits the bundle's grants. See
//! `.superpowers/sdd/objc2-spike-report.md` and `.superpowers/sdd/task-6-brief.md` for the
//! exact procedure. Check 2/3 additionally require `/System/Applications/TextEdit.app` to
//! exist on the test machine (true on every stock macOS install) and, like `tests/input.rs`,
//! an unlocked screen session (TextEdit's window must actually receive focus for the AX
//! snapshot in check 2 to see real content).

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("skipped (not macOS)");
}

#[cfg(target_os = "macos")]
fn main() {
    macos_main::run();
}

#[cfg(target_os = "macos")]
mod macos_main {
    use std::path::{Path, PathBuf};
    use std::process::Command;
    use std::time::Duration;

    use glass_a11y_macos::MacosA11y;
    use glass_core::platform::{MouseButton, PointerEvent};
    use glass_core::{
        Accessibility, AppSpec, AxContext, GlassError, Platform, SandboxLevel, Stream,
    };
    use glass_macos::MacosPlatform;

    /// A stock-macOS handoff app: re-execs itself through LaunchServices, so directly
    /// spawning `Contents/MacOS/TextEdit` exits before a window appears and
    /// `start_bundle` falls into its `NSWorkspace` branch — exactly the fixture checks 2
    /// and 3 need, without shipping a second custom `.app`. Present on every stock macOS
    /// install (unlike a third-party app), so no availability probe is needed.
    const TEXT_EDIT: &str = "/System/Applications/TextEdit.app";

    /// `CFBundleIdentifier` [`build_demo_app`] writes into the fixture's `Info.plist` —
    /// used only to identify the fixture in logs/Info.plist; check 1 never looks an app up
    /// by bundle id (that's the handoff path's job, checks 2/3), so this need not resolve to
    /// anything real.
    const DEMO_BUNDLE_ID: &str = "tech.fixedwidth.quadrants-demo";

    /// Window-relative pixel [`run_foreground_check`] clicks — the center of
    /// `quadrants.swift`'s top-left quadrant in its 400x400 window, mirroring
    /// `tests/input.rs`'s identical `CLICK_TARGET`.
    const CLICK_TARGET: (i32, i32) = (100, 100);

    /// Settle after `start_app`/`send_pointer` before reading logs/snapshotting — generous
    /// relative to each backend call's own internal settling, mirroring the sibling
    /// integration tests' identically-reasoned constants.
    const STARTUP_SETTLE: Duration = Duration::from_millis(500);
    const ACTION_SETTLE: Duration = Duration::from_millis(400);
    /// How long to wait after `stop_app` before checking a freshly-adopted pid has actually
    /// exited (check 2's no-orphan assertion) — `ffi::terminate_app` posts a terminate
    /// request and returns; this gives the target process a moment to actually unwind.
    const TERMINATE_SETTLE: Duration = Duration::from_secs(1);

    /// Print a clear failure message and exit non-zero — the `harness = false` contract (no
    /// libtest to format a panic for us). Mirrors the sibling integration tests.
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// Like the sibling tests' identical helper: returns the error as a `String` instead of
    /// exiting, so a failure inside a check function still lets that check's own cleanup
    /// (`stop_app`, fixture-dir removal) run before the message propagates.
    fn try_expect<T, E: std::fmt::Display>(
        result: Result<T, E>,
        context: &str,
    ) -> Result<T, String> {
        result.map_err(|e| format!("{context}: {e}"))
    }

    fn swiftc_available() -> bool {
        Command::new("swiftc")
            .arg("--version")
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// True if a process named `name` is currently running (`pgrep -x`) — used by
    /// [`run_handoff_check`] to tell a genuinely fresh `NSWorkspace` launch (no prior
    /// instance) from re-adopting one that was already running, since only the fresh case
    /// is expected to leave nothing behind after `stop_app` (see that function's doc).
    fn process_running(name: &str) -> bool {
        Command::new("pgrep")
            .arg("-x")
            .arg(name)
            .output()
            .is_ok_and(|o| o.status.success())
    }

    /// Compile `fixture/quadrants.swift` into a minimal `.app` bundle at
    /// `<build_dir>/Demo.app/Contents/MacOS/Demo`, with an `Info.plist` carrying exactly the
    /// three keys `bundle::is_app_bundle`/`resolve_inner_exec`/`bundle_identifier` need
    /// (`CFBundleExecutable`, `CFBundleIdentifier`, `CFBundlePackageType`) — the smallest
    /// bundle `is_app_bundle`'s `Contents/Info.plist`-presence check and `resolve_inner_exec`'s
    /// `CFBundleExecutable` lookup both accept. Returns the bundle's path and the temp build
    /// dir it lives in (the caller removes the dir once done, mirroring `capture.rs`'s
    /// `build_fixture`).
    fn build_demo_app() -> Result<(PathBuf, PathBuf), String> {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir.join("fixture").join("quadrants.swift");
        if !source.is_file() {
            return Err(format!("fixture source not found at {}", source.display()));
        }

        let out_dir = std::env::temp_dir().join(format!(
            "glass-macos-bundle-launch-test-{}",
            std::process::id()
        ));
        let bundle = out_dir.join("Demo.app");
        let macos_dir = bundle.join("Contents/MacOS");
        std::fs::create_dir_all(&macos_dir)
            .map_err(|e| format!("creating {}: {e}", macos_dir.display()))?;

        let exe = macos_dir.join("Demo");
        let status = Command::new("swiftc")
            .arg("-O")
            .arg("-parse-as-library")
            .arg(&source)
            .arg("-o")
            .arg(&exe)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => {
                return Err(format!(
                    "swiftc exited with {s} building {}",
                    source.display()
                ))
            }
            Err(e) => return Err(format!("failed to run swiftc: {e}")),
        }

        let info_plist = format!(
            r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0"><dict>
<key>CFBundleExecutable</key><string>Demo</string>
<key>CFBundleIdentifier</key><string>{DEMO_BUNDLE_ID}</string>
<key>CFBundlePackageType</key><string>APPL</string>
</dict></plist>"#
        );
        let plist_path = bundle.join("Contents/Info.plist");
        std::fs::write(&plist_path, info_plist)
            .map_err(|e| format!("writing {}: {e}", plist_path.display()))?;

        Ok((bundle, out_dir))
    }

    /// Build the `AppSpec` every check here launches: `run[0]` is `run0` (a `.app` bundle
    /// path in every call site), sandboxed at `sandbox`, with the fixed no-build/no-a11y/
    /// 8s-timeout settings none of the three checks vary. Extracted because checks 2 and 3
    /// launch the identical `TEXT_EDIT` target and differ only in `sandbox`.
    fn bundle_spec(run0: impl Into<String>, sandbox: SandboxLevel) -> AppSpec {
        AppSpec {
            build: None,
            run: vec![run0.into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 8000,
            sandbox,
            a11y: false,
        }
    }

    /// Run `body`, then always call `platform.stop_app()` before returning — regardless of
    /// whether `body` succeeded — so a failing check never leaks whatever it started.
    /// Mirrors the sibling tests' `run()`/`run_checks` cleanup-always-runs discipline,
    /// collapsed into one helper since each check function here owns a short-lived
    /// `MacosPlatform` of its own rather than sharing one across the whole file. On success,
    /// a `stop_app` failure becomes the check's own failure; on an already-failing `body`,
    /// a `stop_app` failure is only logged (the original failure is more informative and
    /// must not be masked).
    fn with_stop_app<T>(
        platform: &mut MacosPlatform,
        body: impl FnOnce(&mut MacosPlatform) -> Result<T, String>,
    ) -> Result<T, String> {
        let result = body(platform);
        let stop_result = platform.stop_app();
        match result {
            Ok(v) => try_expect(stop_result, "stop_app").map(|()| v),
            Err(e) => {
                if let Err(stop_err) = stop_result {
                    eprintln!("(additionally) stop_app failed: {stop_err}");
                }
                Err(e)
            }
        }
    }

    /// The result of running one check. Kept distinct from a plain `Result` so a missing
    /// precondition for ONE check (`Skipped`) is reported as skipped-not-passed and, crucially,
    /// never causes the OTHER checks to be skipped — the pathological "the whole test silently
    /// passes because a single precondition was absent" shape. `Ran(Ok(()))` passed;
    /// `Ran(Err(_))` failed with a reason; `Skipped(_)` asserted nothing. Each check gates
    /// itself on ONLY the precondition it actually needs (swiftc for check 1; `TextEdit.app`
    /// for checks 2/3), so `run` no longer has any blanket gate that could skip everything.
    enum Outcome {
        Ran(Result<(), String>),
        Skipped(String),
    }

    /// Check 1 — foreground `.app`, direct-spawn: build `Demo.app`, `start_app` it under
    /// the default sandbox, click it, and confirm the click round-tripped through captured
    /// stdout. A captured `click: <x>,<y>` line is the only externally-observable proof this
    /// went through `process::spawn`'s child path (see this file's module doc) — an
    /// `NSWorkspace`-adopted process (check 2) has nothing piped at all, so its `drain_logs`
    /// is asserted empty instead.
    ///
    /// Precondition: `swiftc` (to build the fixture bundle). This check alone needs it —
    /// checks 2/3 use the stock `TextEdit.app` and no compiler — so its absence skips only
    /// this check, never the others.
    fn run_foreground_check() -> Outcome {
        if !swiftc_available() {
            return Outcome::Skipped(
                "swiftc unavailable (needed to compile the Demo.app fixture)".to_string(),
            );
        }
        Outcome::Ran(foreground_check_body())
    }

    /// Check 1's body, split out so [`run_foreground_check`] can gate on `swiftc` and wrap
    /// this in [`Outcome`] while the body itself keeps using `?` over `Result`.
    fn foreground_check_body() -> Result<(), String> {
        let (bundle, build_dir) = build_demo_app()?;
        println!("built fixture bundle at {}", bundle.display());

        let mut platform = match MacosPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&build_dir);
                return Err(format!("MacosPlatform::new(): {e}"));
            }
        };

        let result = with_stop_app(&mut platform, |platform| {
            let spec = bundle_spec(bundle.to_string_lossy().into_owned(), SandboxLevel::Default);
            let geometry = try_expect(
                platform.start_app(&spec),
                "start_app(Demo.app, sandbox: Default)",
            )?;
            println!("started Demo.app foreground window: {geometry:?}");
            std::thread::sleep(STARTUP_SETTLE);

            let click_event = PointerEvent::Click {
                x: CLICK_TARGET.0,
                y: CLICK_TARGET.1,
                button: MouseButton::Left,
                count: 1,
                modifiers: vec![],
            };
            try_expect(platform.send_pointer(&click_event), "send_pointer(Click)")?;
            std::thread::sleep(ACTION_SETTLE);

            let logs = platform.drain_logs();
            let saw_click = logs
                .iter()
                .any(|(stream, line)| *stream == Stream::Stdout && line.starts_with("click: "));
            if !saw_click {
                return Err(format!(
                    "expected a captured 'click: ' stdout line proving the direct-spawn/log \
                     path; captured logs: {logs:?}"
                ));
            }
            println!(
                "foreground .app direct-spawn OK: click landed and was reported via captured \
                 stdout (proves this launch used process::spawn's child path, not a handoff)"
            );
            Ok(())
        });

        let _ = std::fs::remove_dir_all(&build_dir);
        result
    }

    /// Check 2 — handoff app, `NSWorkspace` adopt: `start_app(TextEdit.app, sandbox: Off)`
    /// must succeed via the handoff path (no captured logs, since nothing was
    /// `process::spawn`ed for the adopted pid), and a live AX snapshot against it must come
    /// back non-empty. Also verifies the terminate path: if this run's own launch was the
    /// fresh instance (`process_running("TextEdit")` was false beforehand — no prior
    /// instance to merely re-adopt), the adopted pid must be gone after `stop_app`, not
    /// orphaned. If `TextEdit` was already running before this test, `stop_app` is expected
    /// to leave it running (glass only raised an existing instance, not one it started), so
    /// the orphan check is skipped in that case rather than asserting the wrong thing.
    ///
    /// Precondition: `/System/Applications/TextEdit.app` (the handoff target). This check
    /// needs no `swiftc`, so a missing swiftc must never skip it — its own missing-TextEdit
    /// gate is the only thing that skips it.
    fn run_handoff_check() -> Outcome {
        if !Path::new(TEXT_EDIT).is_dir() {
            return Outcome::Skipped(format!(
                "{TEXT_EDIT} not found (the handoff check needs a stock TextEdit.app)"
            ));
        }
        Outcome::Ran(handoff_check_body())
    }

    /// Check 2's body, split out so [`run_handoff_check`] can gate on `TextEdit.app` and
    /// wrap this in [`Outcome`].
    fn handoff_check_body() -> Result<(), String> {
        let text_edit_was_running = process_running("TextEdit");

        let mut platform =
            MacosPlatform::new().map_err(|e| format!("MacosPlatform::new(): {e}"))?;

        let result = with_stop_app(&mut platform, |platform| {
            let spec = bundle_spec(TEXT_EDIT, SandboxLevel::Off);
            let geometry = try_expect(
                platform.start_app(&spec),
                "start_app(TextEdit.app, sandbox: Off)",
            )?;
            println!("adopted TextEdit window: {geometry:?}");
            std::thread::sleep(STARTUP_SETTLE);

            let logs = platform.drain_logs();
            if !logs.is_empty() {
                return Err(format!(
                    "expected no captured logs for an NSWorkspace-adopted app (nothing was \
                     process::spawn'd for it), got: {logs:?}"
                ));
            }

            let ctx = AxContext {
                pids: platform.app_pids(),
                window: geometry,
                window_handle: None,
                a11y_bus_addr: None,
            };
            let mut a11y = MacosA11y::new();
            let mut tree = try_expect(a11y.snapshot(&ctx), "a11y snapshot(TextEdit)")?;
            tree.assign_ids();
            if tree.count == 0 {
                return Err("expected a non-empty AX tree for TextEdit, got 0 nodes".into());
            }
            println!(
                "handoff app OK: TextEdit adopted with no captured logs, AX snapshot has {} \
                 nodes",
                tree.count
            );
            Ok(())
        });

        // Orphan check: only meaningful once the main check above actually adopted
        // TextEdit, and only asserted when this test's own call was the fresh launch —
        // re-run the same `process_running` probe used above (now post-`stop_app`, via
        // `with_stop_app`) rather than tracking the adopted pid directly, since
        // `MacosPlatform`'s `adopted`/`app_pid` fields are private to `glass-macos`. A
        // `match` on both facts (rather than if/else-if) keeps all three outcomes explicit,
        // including the "main check already failed" case, which has nothing to verify here.
        match (&result, text_edit_was_running) {
            (Ok(()), false) => {
                std::thread::sleep(TERMINATE_SETTLE);
                if process_running("TextEdit") {
                    return Err(
                        "TextEdit is still running after stop_app on a launch this test \
                         itself started fresh (no prior instance existed) -- stop_app's \
                         fresh-adoption terminate path did not actually terminate it"
                            .to_string(),
                    );
                }
                println!("no-orphan check OK: fresh TextEdit adoption was terminated by stop_app");
            }
            (Ok(()), true) => println!(
                "TextEdit was already running before this check -- skipping the no-orphan \
                 assertion (stop_app correctly leaves a pre-existing instance running, which \
                 is not an orphan)"
            ),
            (Err(_), _) => {
                // The main check already failed; the orphan probe only makes sense after a
                // successful adoption, so there's nothing more to verify here.
            }
        }

        result
    }

    /// Check 3 — fail-closed: the same handoff trigger as check 2, but `sandbox: Default`.
    /// `bundle::handoff_gate` must reject the adoption before it happens, so `start_app`
    /// returns `Err(GlassError::AppNotStarted(_))` rather than ever adopting TextEdit.
    ///
    /// Precondition: `/System/Applications/TextEdit.app` (same target as check 2, no
    /// `swiftc`). Gates on that alone.
    fn run_fail_closed_check() -> Outcome {
        if !Path::new(TEXT_EDIT).is_dir() {
            return Outcome::Skipped(format!(
                "{TEXT_EDIT} not found (the fail-closed check needs a stock TextEdit.app)"
            ));
        }
        Outcome::Ran(fail_closed_check_body())
    }

    /// Check 3's body, split out so [`run_fail_closed_check`] can gate on `TextEdit.app` and
    /// wrap this in [`Outcome`].
    fn fail_closed_check_body() -> Result<(), String> {
        let mut platform =
            MacosPlatform::new().map_err(|e| format!("MacosPlatform::new(): {e}"))?;

        with_stop_app(&mut platform, |platform| {
            let spec = bundle_spec(TEXT_EDIT, SandboxLevel::Default);
            match platform.start_app(&spec) {
                Err(GlassError::AppNotStarted(msg)) => {
                    println!(
                        "fail-closed OK: start_app(TextEdit.app, sandbox: Default) -> \
                         AppNotStarted({msg:?})"
                    );
                    Ok(())
                }
                Ok(geometry) => Err(format!(
                    "expected Err(AppNotStarted(_)) for a sandboxed handoff attempt, but \
                     start_app unexpectedly succeeded with geometry {geometry:?}"
                )),
                Err(other) => Err(format!(
                    "expected Err(AppNotStarted(_)) for a sandboxed handoff attempt, got \
                     {other:?} instead"
                )),
            }
        })
    }

    pub(super) fn run() {
        // No blanket precondition gate here — each check declines itself (returning
        // `Outcome::Skipped`) when the ONE precondition it needs is absent, so a missing
        // `swiftc` can never skip the handoff/fail-closed checks and a missing `TextEdit.app`
        // can never skip the foreground check. Failures fail the run; skips are reported
        // distinctly (SKIPPED, never counted as a pass) and do not block the PASS banner for
        // the checks that did run and pass. On the mini both preconditions are present, so
        // all three run.
        let mut failures = Vec::new();
        let mut skips = Vec::new();

        // Takes the check as a `fn` (not an already-evaluated `Outcome`) so the header prints
        // BEFORE the check runs — otherwise the check's own progress output would land above
        // its header.
        let mut record = |label: &str, header: &str, check: fn() -> Outcome| {
            println!("--- {header} ---");
            match check() {
                Outcome::Ran(Ok(())) => {}
                Outcome::Ran(Err(e)) => failures.push(format!("{label}: {e}")),
                Outcome::Skipped(reason) => {
                    println!("SKIPPED: {label}: {reason}");
                    skips.push(label.to_string());
                }
            }
        };

        record(
            "foreground check",
            "check 1: foreground .app (direct-spawn)",
            run_foreground_check,
        );
        record(
            "handoff check",
            "check 2: handoff app (NSWorkspace adopt)",
            run_handoff_check,
        );
        record(
            "fail-closed check",
            "check 3: fail-closed (sandboxed handoff)",
            run_fail_closed_check,
        );

        if !failures.is_empty() {
            fail(failures.join("\n"));
        }
        if skips.is_empty() {
            println!("BUNDLE_LAUNCH_INTEGRATION_PASS");
        } else {
            println!(
                "BUNDLE_LAUNCH_INTEGRATION_PASS ({} check(s) skipped: {})",
                skips.len(),
                skips.join(", ")
            );
        }
        std::process::exit(0);
    }
}
