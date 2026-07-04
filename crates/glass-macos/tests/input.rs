//! Mac-gated input integration test — the end-to-end proof that `MacosPlatform::send_key`/
//! `send_pointer` land at the right place: a real `NSView` reports back the characters it
//! received and the pixel it was clicked at, closing the loop on the whole
//! pixel -> point -> global -> CGEvent -> AppKit -> app round trip. It also records the
//! scroll-wheel sign `input.rs`'s `MacScrollSink::wheel` produces: a positive `dy` is sent
//! and the fixture's raw, verbatim `NSEvent.scrollingDeltaY` is printed. A granted run has
//! confirmed this leg passes end-to-end, but the observed sign is intentionally left
//! unasserted here — see the scroll leg's comment in `run_checks` below — because it
//! depends on the target Mac's **natural-scrolling** system setting
//! (`com.apple.swipescrolldirection`), not just `input.rs`'s own `wheel1 = -dy` mapping, so
//! hard-asserting one fixed sign here would be machine-specific rather than a real
//! regression check.
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "input"` entry) for the exact
//! same reason as `tests/capture.rs`: `send_key`/`send_pointer` reach AppKit
//! (`NSRunningApplication::activateWithOptions`, `ffi::app_kit_init()` via `start_app`'s
//! window discovery), which requires the process's TRUE main thread
//! (`objc2::MainThreadMarker`) — libtest's per-`#[test]` worker threads can't provide that,
//! so this file defines its own `fn main()` instead.
//!
//! Needs the Accessibility TCC grant (CGEvent posting) in addition to Screen Recording
//! (window discovery via ScreenCaptureKit), both held by the signed, granted `GlassProbe.app`
//! bundle on this project's dev Mac (`mini`) — same granted-run procedure as
//! `tests/capture.rs`: copy this built test binary into the bundle, re-sign, run via a
//! `gui/501` LaunchAgent so it inherits the bundle's grants. See
//! `.superpowers/sdd/objc2-spike-report.md`, `.superpowers/sdd/task-6-fix-report.md`, and
//! `.superpowers/sdd/task-6-brief.md` for the exact procedure, and `scripts/test-macos.sh`'s
//! `GLASS_MACOS_ONBOX` gate for how this fits the test scripts.
//!
//! **Additional runtime precondition beyond the two TCC grants: `mini`'s screen session
//! must not be locked.** Task 6 debugging (see `.superpowers/sdd/task-6-report.md`)
//! empirically proved that while the console session is locked
//! (`CGSSessionScreenIsLocked=1`), macOS's secure-input protection pins
//! `NSWorkspace.frontmostApplication` to `loginwindow` and silently drops every synthetic
//! CGEvent aimed at a background app — `NSRunningApplication.activate`/
//! `AXUIElementSetAttributeValue(kAXFrontmostAttribute)` both report success with zero
//! effect, cursor *position* HID state updates but clicks never reach the target window,
//! and keyDown events never arrive at all. This is a session-state precondition of the
//! granted run, not a `glass-macos` bug — `capture.rs`'s read-only ScreenCaptureKit path is
//! unaffected by it (compositor output is readable regardless of lock state), which is why
//! this gap wasn't visible until this file's first real input-posting run.

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
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    use glass_core::platform::{KeyEvent, MouseButton, PointerEvent};
    use glass_core::{AppSpec, Platform, SandboxLevel, Stream};
    use glass_macos::MacosPlatform;

    /// Settle after each injected action before draining logs — generous relative to
    /// `input.rs`'s own internal `FOCUS_SETTLE` (300ms, already paid once per `send_key`/
    /// `send_pointer` call via `focus()`), so the fixture's `fflush`ed stdout line has
    /// definitely been read by `process.rs`'s background reader thread before we drain.
    const ACTION_SETTLE: Duration = Duration::from_millis(400);

    /// Window-relative pixel the click is sent to — the center of the fixture's top-left
    /// (red) quadrant in its 400x400 window, comfortably away from any quadrant boundary.
    const CLICK_TARGET: (i32, i32) = (100, 100);
    /// `assert_click_near`'s tolerance: how far the fixture-reported click coordinate may
    /// drift from `CLICK_TARGET` and still count as a correct round trip (rounding in the
    /// pixel<->point conversion, not a loose "close enough").
    const CLICK_TOLERANCE: i32 = 5;

    /// Print a clear failure message and exit non-zero — the `harness = false` contract
    /// (no libtest to format a panic for us).
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// Unwrap a `Result`, failing the whole test process with `context` prefixed to the
    /// error on `Err`. Only safe to use before a fixture process has been spawned (or once
    /// all spawn-time cleanup is already done) — see `capture.rs`'s identical helper for why
    /// anything after that must go through `try_expect`/`run_checks` instead.
    fn expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(v) => v,
            Err(e) => fail(format!("{context}: {e}")),
        }
    }

    /// Like `expect`, but returns the error as a `String` instead of exiting the process —
    /// see `capture.rs`'s identical helper's doc for why: a failure raised inside
    /// `run_checks` (fixture already spawned) must flow back to `run()` so it can
    /// `stop_app()` before the process exits, which a direct `std::process::exit` would skip
    /// (Rust destructors don't run across `exit`).
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

    /// Build `fixture/quadrants.swift` to a fresh temp path — identical to `capture.rs`'s
    /// `build_fixture`, just a distinct temp-dir name so a parallel run of both tests never
    /// collides. Returns the built binary's path and the temp build dir it lives in (the
    /// caller removes the dir once done — see `run()`).
    fn build_fixture() -> (PathBuf, PathBuf) {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir.join("fixture").join("quadrants.swift");
        if !source.is_file() {
            fail(format!("fixture source not found at {}", source.display()));
        }

        let out_dir =
            std::env::temp_dir().join(format!("glass-macos-input-test-{}", std::process::id()));
        expect(
            std::fs::create_dir_all(&out_dir),
            "creating fixture build dir",
        );
        let out_bin = out_dir.join("quadrants");

        let status = Command::new("swiftc")
            .arg("-O")
            .arg("-parse-as-library")
            .arg(&source)
            .arg("-o")
            .arg(&out_bin)
            .status();
        match status {
            Ok(s) if s.success() => {}
            Ok(s) => fail(format!(
                "swiftc exited with {s} building {}",
                source.display()
            )),
            Err(e) => fail(format!("failed to run swiftc: {e}")),
        }
        (out_bin, out_dir)
    }

    /// Every stdout line from `lines` that starts with `prefix`, with the prefix stripped —
    /// e.g. `find_reported(lines, "key: ")` yields `["h", "e", ...]` from `quadrants.swift`'s
    /// `key: <characters>` reporting. Stderr lines are ignored (the fixture never writes
    /// there).
    fn find_reported<'a>(lines: &'a [(Stream, String)], prefix: &str) -> Vec<&'a str> {
        lines
            .iter()
            .filter(|(stream, _)| *stream == Stream::Stdout)
            .filter_map(|(_, line)| line.strip_prefix(prefix))
            .collect()
    }

    /// Parse a fixture-reported `"A,B"` pair (as printed by `quadrants.swift`'s `click:`/
    /// `scroll:` lines) into `(A, B)`. Generic over `T` so callers can parse either the
    /// integer `click:` pair or the floating-point `scroll:` pair (`NSEvent.scrollingDeltaX`/
    /// `Y` are `CGFloat`).
    fn parse_pair<T: std::str::FromStr>(raw: &str) -> Option<(T, T)> {
        let (a, b) = raw.split_once(',')?;
        Some((a.trim().parse().ok()?, b.trim().parse().ok()?))
    }

    /// Assert the fixture's `key:` lines, concatenated in report order, spell out `expected`
    /// — each keyDown reports one character, so typing `"hello"` should produce exactly the
    /// 5 lines `key: h`/`key: e`/`key: l`/`key: l`/`key: o`.
    fn assert_typed(lines: &[(Stream, String)], expected: &str) -> Result<(), String> {
        let reported = find_reported(lines, "key: ");
        let got: String = reported.iter().copied().collect();
        if got != expected {
            return Err(format!(
                "typed text mismatch: fixture reported {reported:?} (joined: {got:?}), expected {expected:?}"
            ));
        }
        Ok(())
    }

    /// Assert the fixture's last `click:` line is within `tolerance` px of `expected` —
    /// validates the whole pixel -> point -> global -> CGEvent -> back-to-window-pixel round
    /// trip (`coords::pixel_to_global_point` on the way out, `quadrants.swift`'s
    /// `mouseDown` view-space conversion on the way back).
    fn assert_click_near(
        lines: &[(Stream, String)],
        expected: (i32, i32),
        tolerance: i32,
    ) -> Result<(), String> {
        let reported = find_reported(lines, "click: ");
        let Some(raw) = reported.last() else {
            return Err(format!(
                "no click: line in fixture output (stdout lines: {lines:?})"
            ));
        };
        let Some((x, y)) = parse_pair::<i32>(raw) else {
            return Err(format!("could not parse click coordinates from {raw:?}"));
        };
        if (x - expected.0).abs() > tolerance || (y - expected.1).abs() > tolerance {
            return Err(format!(
                "click landed at ({x},{y}), expected ~{expected:?} (tolerance {tolerance}px)"
            ));
        }
        println!(
            "click landed at ({x},{y}), expected ~{expected:?} — within {tolerance}px tolerance"
        );
        Ok(())
    }

    /// Read the fixture's last `scroll:` line, asserting it exists and reports a non-zero
    /// vertical delta, and return `(reported_dx, reported_dy)` verbatim (`NSEvent`'s own
    /// `scrollingDeltaX`/`Y`, unmodified) for the caller to interpret against glass's own
    /// `dy`-sent convention.
    fn read_scroll(lines: &[(Stream, String)]) -> Result<(f64, f64), String> {
        let reported = find_reported(lines, "scroll: ");
        let Some(raw) = reported.last() else {
            return Err(format!(
                "no scroll: line in fixture output (stdout lines: {lines:?})"
            ));
        };
        let Some((dx, dy)) = parse_pair::<f64>(raw) else {
            return Err(format!("could not parse scroll delta from {raw:?}"));
        };
        if dy == 0.0 {
            return Err(format!(
                "scroll dy reported as zero (raw scroll line: {raw:?})"
            ));
        }
        Ok((dx, dy))
    }

    /// The whole `start_app` -> input-injection -> assertion flow, from launching the
    /// fixture through the last assertion. Returns `Err` instead of exiting the process on
    /// any failure, so `run()` can always reach `platform.stop_app()` first — see
    /// `capture.rs`'s identical-shaped `run_checks` for why a bare `std::process::exit` from
    /// in here would leak the spawned fixture process.
    fn run_checks(
        platform: &mut MacosPlatform,
        fixture_bin: &std::path::Path,
    ) -> Result<(), String> {
        let spec = AppSpec {
            build: None,
            run: vec![fixture_bin.to_string_lossy().into_owned()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 8000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };

        let geometry = try_expect(platform.start_app(&spec), "start_app")?;
        println!("started fixture window: {geometry:?}");

        // Give the fixture's window a moment to finish appearing/painting before the first
        // injected event — mirrors capture.rs's identical settle (this fixture draws once,
        // synchronously, immediately on launch; see that file's comment for why a fixed
        // sleep is fine here specifically).
        std::thread::sleep(Duration::from_millis(500));

        // --- send_key: type "hello", assert the fixture reported exactly those chars. ---
        try_expect(
            platform.send_key(&KeyEvent::Text("hello".into())),
            "send_key(Text(\"hello\"))",
        )?;
        std::thread::sleep(ACTION_SETTLE);
        let key_logs = platform.drain_logs();
        assert_typed(&key_logs, "hello")?;
        println!("send_key OK: fixture reported the typed characters in order");

        // --- send_pointer(Click): click a known pixel, assert the fixture reported that
        // pixel back (within tolerance) — the pixel -> point -> global -> CGEvent -> back
        // round trip. ---
        let click_event = PointerEvent::Click {
            x: CLICK_TARGET.0,
            y: CLICK_TARGET.1,
            button: MouseButton::Left,
            count: 1,
            modifiers: vec![],
        };
        try_expect(platform.send_pointer(&click_event), "send_pointer(Click)")?;
        std::thread::sleep(ACTION_SETTLE);
        let click_logs = platform.drain_logs();
        assert_click_near(&click_logs, CLICK_TARGET, CLICK_TOLERANCE)?;
        println!("send_pointer(Click) OK");

        // --- send_pointer(Scroll): record the scroll-wheel sign. dy=5 is glass's "scroll
        // the content DOWN" convention (see input.rs's module doc / glass-x11's
        // `scroll_button(5=down,4=up, dy)`). `MacScrollSink::wheel` posts `wheel1 = -dy`, but
        // what `NSEvent.scrollingDeltaY` the window server ultimately delivers to the fixture
        // also depends on `mini`'s **natural-scrolling** system setting
        // (`com.apple.swipescrolldirection`), which inverts the effective on-screen direction
        // independent of anything `input.rs` controls. So only `read_scroll`'s non-zero check
        // is hard-asserted here — the SIGN itself is recorded via `println!` rather than
        // asserted against one fixed expectation, since a fixed expectation would be
        // machine-specific rather than a portable regression check. Follow-up: read
        // `com.apple.swipescrolldirection` (e.g. via `defaults read -g
        // com.apple.swipescrolldirection`) and fold it into the expectation so this can
        // become a real, machine-independent assertion. ---
        let scroll_event = PointerEvent::Scroll {
            x: 200,
            y: 200,
            dx: 0,
            dy: 5,
            modifiers: vec![],
        };
        try_expect(platform.send_pointer(&scroll_event), "send_pointer(Scroll)")?;
        std::thread::sleep(ACTION_SETTLE);
        let scroll_logs = platform.drain_logs();
        let (reported_dx, reported_dy) = read_scroll(&scroll_logs)?;
        println!(
            "send_pointer(Scroll) OK: sent dx=0,dy=5 (glass 'scroll down') -> fixture reported \
             scrollingDeltaX={reported_dx},scrollingDeltaY={reported_dy} (sign depends on \
             mini's natural-scrolling setting — see this file's module doc)"
        );

        Ok(())
    }

    pub(super) fn run() {
        if !swiftc_available() {
            println!("skipped (no swiftc)");
            return;
        }

        let (fixture_bin, fixture_dir) = build_fixture();
        println!("built fixture at {}", fixture_bin.display());

        let mut platform = match MacosPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                let _ = std::fs::remove_dir_all(&fixture_dir);
                fail(format!(
                    "MacosPlatform::new() (Screen Recording / Accessibility grant missing?): {e}"
                ));
            }
        };

        let result = run_checks(&mut platform, &fixture_bin);

        // Reached regardless of outcome, and BEFORE any process::exit below — see
        // capture.rs's identical cleanup ordering for why this must run before any exit.
        let stop_result = platform.stop_app();
        let _ = std::fs::remove_dir_all(&fixture_dir);

        match result {
            Ok(()) => {
                expect(stop_result, "stop_app");
                println!("INPUT_INTEGRATION_PASS");
                std::process::exit(0);
            }
            Err(msg) => {
                if let Err(e) = stop_result {
                    eprintln!("(additionally) stop_app failed: {e}");
                }
                fail(msg);
            }
        }
    }
}
