//! Mac-gated window-management integration test — the first real-window proof of
//! `list_windows`/`select_window`/`window(op)` end-to-end, and in particular of the private
//! `CGWindowID` <-> `AXUIElement` correlation (`axwindow::ax_window_for_cgwindowid`'s private
//! `_AXUIElementGetWindow` call, contained + geometry-fallback — see `axwindow.rs`'s module
//! doc). Every earlier Plan 4 task exercised this only against a *single* on-screen window,
//! where "the app's only window" and "the correlated window" happen to be the same thing
//! even if the correlation itself were silently broken. This test launches the extended
//! `fixture/quadrants.swift` with TWO windows and drives ops against the *non-default* one
//! (`select_window` to a specific `CGWindowID`, then `Move`/`Resize`/`Geometry`/
//! `capture_frame` against it) — a wrong correlation would move/resize/capture the WRONG
//! window, which the assertions below would catch (the moved/resized window's own geometry
//! reads back wrong, or the captured pixels show the other window's palette).
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "windows"` entry) for the exact
//! same reason as `tests/capture.rs`/`tests/input.rs`: `start_app`/`list_windows`/`window`/
//! `capture_frame` all reach `ffi::app_kit_init()`, which requires the process's TRUE main
//! thread (`objc2::MainThreadMarker`) — libtest's per-`#[test]` worker threads can't provide
//! that, so this file defines its own `fn main()` instead.
//!
//! Needs the same two TCC grants as `tests/input.rs` (Screen Recording for window discovery/
//! capture, Accessibility for the AX window ops), held by the signed, granted `GlassProbe.app`
//! bundle on this project's dev Mac (`mini`) — same granted-run procedure as `capture.rs`/
//! `input.rs`: copy this built test binary into the bundle, re-sign, run via a `gui/501`
//! LaunchAgent so it inherits the bundle's grants. See `scripts/test-macos.sh`'s
//! `GLASS_MACOS_ONBOX` gate for how this fits the test scripts.
//!
//! **Additional runtime precondition beyond the two TCC grants: `mini`'s screen session must
//! not be locked** — same secure-input restriction `tests/input.rs`'s module doc documents in
//! detail (a locked screen silently drops synthetic input and pins frontmost-app queries to
//! `loginwindow`). `select_window`/`window(op)` both activate/raise the target window, so this
//! test needs an unlocked screen exactly like `input.rs` does.
//!
//! If a window op below fails, the failure message names which op and what geometry it read
//! back; additionally, watch this run's stderr for `axwindow.rs`'s own diagnostic
//! (`"_AXUIElementGetWindow errored on every AX window for pid ...; falling back to geometry
//! match..."`) — its PRESENCE means the private symbol failed and the geometry fallback
//! engaged for this run; its ABSENCE means the private correlation itself succeeded.

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
    use std::collections::HashSet;
    use std::path::PathBuf;
    use std::process::Command;
    use std::time::Duration;

    use glass_core::platform::{WindowGeometry, WindowInfo, WindowOp};
    use glass_core::{AppSpec, Platform, SandboxLevel};
    use glass_macos::MacosPlatform;

    /// Settle after `start_app` before the first `list_windows` — both fixture windows need
    /// a moment to finish appearing/painting (mirrors `capture.rs`/`input.rs`'s identical
    /// fixed sleep for the single-window case).
    const STARTUP_SETTLE: Duration = Duration::from_millis(500);

    /// Settle after `window(Resize)` before `capture_frame(None)`. Discovered empirically:
    /// `window(Resize)`'s own read-back goes through `AXUIElement` (synchronous, and already
    /// reflects the new size immediately), but `capture_frame`'s `SCShareableContent`/
    /// ScreenCaptureKit path lags slightly behind the on-screen compositor catching up to a
    /// just-resized window — capturing with zero delay observed a captured `Frame` still
    /// reporting the PRE-resize dimensions, with pixels beyond the window's actual new bounds
    /// reading back as transparent black. Generous relative to `input.rs`'s 400ms
    /// `ACTION_SETTLE` for the same class of "let the window system catch up" reason.
    const RESIZE_SETTLE: Duration = Duration::from_millis(500);

    /// `WindowOp::Move` target, in global screen PIXELS — comfortably away from (0,0) and
    /// from the fixture's own default window positions (primary at `.zero`, secondary offset
    /// by (120,120) points — see `quadrants.swift`'s module doc), so a successful move is an
    /// unambiguous, large positional change rather than a coincidental near-match.
    const MOVE_TARGET: (i32, i32) = (300, 200);
    /// Tolerance (px) for comparing a read-back position against [`MOVE_TARGET`]. Wider than
    /// `backend.rs`'s own internal `WINDOW_OP_TOLERANCE_PX` (8px, already enforced inside
    /// `window(Move)` itself — a `window(Move)` call that returns `Ok` already passed that
    /// check) to also absorb the independent `SCShareableContent`-vs-`AXUIElement` geometry
    /// discrepancy `axwindow.rs`'s own `FALLBACK_TOLERANCE_PX` (8px) documents, since
    /// `select_window`'s pre-move geometry check below compares across exactly that boundary.
    const MOVE_TOLERANCE_PX: i32 = 10;
    /// Tolerance (px) for [`geometry_close`]'s comparison of `select_window`'s returned
    /// geometry (from a fresh `AXUIElement` read) against `list_windows`'s own geometry (from
    /// `SCShareableContent`) for the same window — the same two-source discrepancy
    /// `MOVE_TOLERANCE_PX` absorbs, at rest (no move/resize involved).
    const SELECT_TOLERANCE_PX: i32 = 10;
    /// `WindowOp::Resize` target, in PIXELS — both dimensions genuinely different from the
    /// fixture's 400x400 default (width grows, height shrinks), so a successful resize is an
    /// unambiguous change in both axes.
    const RESIZE_TARGET: (u32, u32) = (550, 300);
    /// Minimum per-axis change (px) for [`size_moved_toward`] to count a `Resize` as having
    /// done anything at all — well above `backend.rs`'s own 8px no-op tolerance, so this only
    /// rejects a resize that visibly did nothing, not rounding noise.
    const RESIZE_CHANGE_THRESHOLD_PX: i64 = 20;

    /// Per-channel RGBA tolerance for a sampled pixel vs. its expected known color — same
    /// value and rationale as `capture.rs`'s identical constant.
    const PIXEL_TOLERANCE: i32 = 40;
    /// `quadrants.swift`'s `secondaryPalette` ("glass-fixture-2"'s quadrant colors) — see that
    /// file's module doc. Used to confirm `capture_frame(None)` captured the SELECTED
    /// (secondary) window, not the primary one, by pixel color alone.
    const SECONDARY_TOP_LEFT: [u8; 4] = [0, 255, 255, 255]; // cyan
    const SECONDARY_TOP_RIGHT: [u8; 4] = [255, 0, 255, 255]; // magenta
    const SECONDARY_BOTTOM_LEFT: [u8; 4] = [255, 255, 0, 255]; // yellow
    const SECONDARY_BOTTOM_RIGHT: [u8; 4] = [0, 0, 0, 255]; // black

    /// Print a clear failure message and exit non-zero — the `harness = false` contract (no
    /// libtest to format a panic for us).
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// Unwrap a `Result`, failing the whole test process with `context` prefixed to the error
    /// on `Err`. Only safe to use before a fixture process has been spawned — see
    /// `capture.rs`'s identical helper for why anything after that must go through
    /// `try_expect`/`run_checks` instead.
    fn expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(v) => v,
            Err(e) => fail(format!("{context}: {e}")),
        }
    }

    /// Like `expect`, but returns the error as a `String` instead of exiting the process — so
    /// a failure raised inside `run_checks` (fixture already spawned) still flows back to
    /// `run()`, which reaches `stop_app()` before the process exits.
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

    /// Build `fixture/quadrants.swift` to a fresh temp path — identical to `capture.rs`'s/
    /// `input.rs`'s `build_fixture`, just a distinct temp-dir name so a parallel run of all
    /// three tests never collides.
    fn build_fixture() -> (PathBuf, PathBuf) {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir.join("fixture").join("quadrants.swift");
        if !source.is_file() {
            fail(format!("fixture source not found at {}", source.display()));
        }

        let out_dir =
            std::env::temp_dir().join(format!("glass-macos-windows-test-{}", std::process::id()));
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

    /// Number of `windows` entries with `active == true`.
    fn count_active(windows: &[WindowInfo]) -> usize {
        windows.iter().filter(|w| w.active).count()
    }

    /// True if every field of `a`/`b` is within `tolerance` px of the other — the same shape
    /// as `axwindow.rs`'s internal `within_tolerance`, reimplemented here (rather than
    /// depending on that `pub(crate)` helper from a separate test binary) for comparing a
    /// `list_windows`-reported geometry against a `select_window`/`window(op)`-reported one.
    fn geometry_close(a: &WindowGeometry, b: &WindowGeometry, tolerance: i32) -> bool {
        (a.x - b.x).abs() <= tolerance
            && (a.y - b.y).abs() <= tolerance
            && (a.width as i32 - b.width as i32).abs() <= tolerance
            && (a.height as i32 - b.height as i32).abs() <= tolerance
    }

    /// True if `pos` is within `tolerance` px of `target` on both axes.
    fn position_close(pos: (i32, i32), target: (i32, i32), tolerance: i32) -> bool {
        (pos.0 - target.0).abs() <= tolerance && (pos.1 - target.1).abs() <= tolerance
    }

    /// True if a `Resize` visibly changed the window's size ([`RESIZE_CHANGE_THRESHOLD_PX`]
    /// on at least one axis) AND the change moved the size closer to (or exactly to)
    /// `target`, rather than further away — tolerant of macOS clamping to an intermediate
    /// size (see `backend.rs`'s `resize_was_refused` doc for the same "clamped, not exact"
    /// contract), while still catching a resize that did nothing or moved the wrong way.
    fn size_moved_toward(
        before: &WindowGeometry,
        after: &WindowGeometry,
        target: (u32, u32),
    ) -> bool {
        let changed = (after.width as i64 - before.width as i64).abs() > RESIZE_CHANGE_THRESHOLD_PX
            || (after.height as i64 - before.height as i64).abs() > RESIZE_CHANGE_THRESHOLD_PX;
        if !changed {
            return false;
        }
        let dist_before = (before.width as i64 - target.0 as i64).abs()
            + (before.height as i64 - target.1 as i64).abs();
        let dist_after = (after.width as i64 - target.0 as i64).abs()
            + (after.height as i64 - target.1 as i64).abs();
        dist_after <= dist_before
    }

    /// Fetch pixel `(x, y)` from a tightly-packed RGBA8 `Frame` buffer — identical to
    /// `capture.rs`'s helper of the same name.
    fn pixel_at(pixels: &[u8], frame_width: u32, x: u32, y: u32) -> [u8; 4] {
        let idx = (y as usize * frame_width as usize + x as usize) * 4;
        [
            pixels[idx],
            pixels[idx + 1],
            pixels[idx + 2],
            pixels[idx + 3],
        ]
    }

    fn close(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter()
            .zip(b.iter())
            .all(|(x, y)| (*x as i32 - *y as i32).abs() <= PIXEL_TOLERANCE)
    }

    /// Assert the pixel at `(x, y)` in a tightly-packed RGBA8 buffer is within tolerance of
    /// `expected` — identical shape to `capture.rs`'s helper of the same name.
    fn assert_pixel(
        pixels: &[u8],
        frame_width: u32,
        x: u32,
        y: u32,
        expected: [u8; 4],
        label: &str,
    ) -> Result<(), String> {
        let got = pixel_at(pixels, frame_width, x, y);
        if !close(got, expected) {
            return Err(format!(
                "{label} pixel at ({x},{y}) = {got:?}, expected ~{expected:?} (tolerance {PIXEL_TOLERANCE})"
            ));
        }
        Ok(())
    }

    /// The whole `start_app` -> list/select/move/resize/capture -> assertion flow. Returns
    /// `Err` instead of exiting the process on any failure, so `run()` can always reach
    /// `platform.stop_app()` first — see `capture.rs`'s identically-shaped `run_checks` for
    /// why a bare `std::process::exit` from in here would leak the spawned fixture process.
    fn run_checks(
        platform: &mut MacosPlatform,
        fixture_bin: &std::path::Path,
    ) -> Result<(), String> {
        let spec = AppSpec {
            build: None,
            run: vec![
                fixture_bin.to_string_lossy().into_owned(),
                "--windows".into(),
                "2".into(),
            ],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 8000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };

        let geometry = try_expect(platform.start_app(&spec), "start_app")?;
        println!("started 2-window fixture; initial active-window geometry: {geometry:?}");
        std::thread::sleep(STARTUP_SETTLE);

        // --- list_windows: both windows present, distinct ids, exactly one active. ---
        let windows = try_expect(platform.list_windows(), "list_windows (initial)")?;
        println!("list_windows (initial): {windows:?}");
        if windows.len() < 2 {
            return Err(format!(
                "expected >=2 windows, got {}: {windows:?}",
                windows.len()
            ));
        }
        let ids: HashSet<_> = windows.iter().map(|w| w.id).collect();
        if ids.len() != windows.len() {
            return Err(format!("window ids are not all distinct: {windows:?}"));
        }
        if count_active(&windows) != 1 {
            return Err(format!(
                "expected exactly one active window, got {}: {windows:?}",
                count_active(&windows)
            ));
        }
        let primary = windows
            .iter()
            .find(|w| w.title.as_deref() == Some("glass-fixture"))
            .ok_or_else(|| format!("no window titled 'glass-fixture' in {windows:?}"))?;
        let secondary = windows
            .iter()
            .find(|w| w.title.as_deref() == Some("glass-fixture-2"))
            .ok_or_else(|| format!("no window titled 'glass-fixture-2' in {windows:?}"))?;
        println!("found both fixture windows: primary={primary:?} secondary={secondary:?}");
        if secondary.active {
            return Err(format!(
                "expected 'glass-fixture-2' to NOT be the initially active window (test needs to \
                 select a non-active window to exercise retargeting): {secondary:?}"
            ));
        }
        if !primary.active {
            return Err(format!(
                "expected 'glass-fixture' to be the initially active window (start_app's \
                 first-window-discovered contract): {primary:?}"
            ));
        }
        let secondary_id = secondary.id;
        let secondary_geometry = secondary.geometry.clone();

        // --- select_window(secondary.id): becomes active, geometry matches the listed
        // window's own geometry (via a completely different lookup path — SCShareableContent
        // for the list, AXUIElement for the select — so agreement here already exercises the
        // CGWindowID<->AXUIElement correlation once). ---
        let selected_geom = try_expect(
            platform.select_window(secondary_id),
            "select_window(glass-fixture-2)",
        )?;
        println!("select_window(glass-fixture-2) -> {selected_geom:?}");
        if !geometry_close(&selected_geom, &secondary_geometry, SELECT_TOLERANCE_PX) {
            return Err(format!(
                "select_window returned geometry {selected_geom:?}, expected within \
                 {SELECT_TOLERANCE_PX}px of the listed window's own geometry {secondary_geometry:?} -- \
                 check stderr above for _AXUIElementGetWindow/geometry-fallback diagnostics \
                 (a wrong correlation would resolve to the WRONG AXUIElement here)"
            ));
        }

        let windows_after_select =
            try_expect(platform.list_windows(), "list_windows (after select)")?;
        println!("list_windows (after select): {windows_after_select:?}");
        let now_selected = windows_after_select
            .iter()
            .find(|w| w.id == secondary_id)
            .ok_or_else(|| {
                format!("selected window {secondary_id:?} missing from list_windows: {windows_after_select:?}")
            })?;
        if !now_selected.active {
            return Err(format!(
                "expected glass-fixture-2 to be active after select_window: {windows_after_select:?}"
            ));
        }
        if count_active(&windows_after_select) != 1 {
            return Err(format!(
                "expected exactly one active window after select, got {}: {windows_after_select:?}",
                count_active(&windows_after_select)
            ));
        }
        println!("select_window OK: glass-fixture-2 is now the sole active window");

        // --- window(Move) on the selected (non-first) window. ---
        let move_op = WindowOp::Move {
            x: MOVE_TARGET.0,
            y: MOVE_TARGET.1,
        };
        let moved = try_expect(platform.window(&move_op), "window(Move)")?;
        println!(
            "window(Move{{{},{}}}) -> {moved:?}",
            MOVE_TARGET.0, MOVE_TARGET.1
        );
        if !position_close((moved.x, moved.y), MOVE_TARGET, MOVE_TOLERANCE_PX) {
            return Err(format!(
                "window did not move to ~{MOVE_TARGET:?} (within {MOVE_TOLERANCE_PX}px); backend \
                 reports ({},{}) -- check stderr above for _AXUIElementGetWindow/geometry-fallback \
                 diagnostics",
                moved.x, moved.y
            ));
        }
        let geom_after_move = try_expect(
            platform.window(&WindowOp::Geometry),
            "window(Geometry) after Move",
        )?;
        println!("window(Geometry) after Move -> {geom_after_move:?}");
        if !position_close(
            (geom_after_move.x, geom_after_move.y),
            MOVE_TARGET,
            MOVE_TOLERANCE_PX,
        ) {
            return Err(format!(
                "window(Geometry) after Move disagrees with the Move op's own return: \
                 {geom_after_move:?} vs target {MOVE_TARGET:?}"
            ));
        }
        println!(
            "window(Move) OK: window is at ({},{}), target was {MOVE_TARGET:?}",
            geom_after_move.x, geom_after_move.y
        );

        // --- window(Resize) on the selected window: tolerant of macOS clamping — assert it
        // changed toward the target, not exactly reached it. ---
        let before_resize = geom_after_move;
        let resize_op = WindowOp::Resize {
            width: RESIZE_TARGET.0,
            height: RESIZE_TARGET.1,
        };
        let resized = try_expect(platform.window(&resize_op), "window(Resize)")?;
        println!(
            "window(Resize{{{},{}}}) -> {resized:?}",
            RESIZE_TARGET.0, RESIZE_TARGET.1
        );
        if !size_moved_toward(&before_resize, &resized, RESIZE_TARGET) {
            return Err(format!(
                "resize did not move toward {RESIZE_TARGET:?}: before={before_resize:?}, \
                 after={resized:?} -- check stderr above for _AXUIElementGetWindow/geometry-fallback \
                 diagnostics"
            ));
        }
        println!(
            "window(Resize) OK: {}x{} -> {}x{} (target {}x{})",
            before_resize.width,
            before_resize.height,
            resized.width,
            resized.height,
            RESIZE_TARGET.0,
            RESIZE_TARGET.1
        );
        std::thread::sleep(RESIZE_SETTLE);

        // --- capture_frame(None): must capture the SELECTED (secondary) window — confirmed
        // by its distinct quadrant palette, not the primary window's. ---
        let frame = try_expect(platform.capture_frame(None), "capture_frame(None)")?;
        println!(
            "captured {}x{} frame of the selected window",
            frame.width, frame.height
        );
        if frame.width < 2 || frame.height < 2 {
            return Err(format!(
                "captured frame too small to sample quadrants: {}x{}",
                frame.width, frame.height
            ));
        }
        let (fw, fh) = (frame.width, frame.height);
        let (qx0, qx1) = (fw / 4, fw * 3 / 4);
        let (qy0, qy1) = (fh / 4, fh * 3 / 4);
        assert_pixel(
            &frame.pixels,
            fw,
            qx0,
            qy0,
            SECONDARY_TOP_LEFT,
            "selected-window top-left",
        )?;
        assert_pixel(
            &frame.pixels,
            fw,
            qx1,
            qy0,
            SECONDARY_TOP_RIGHT,
            "selected-window top-right",
        )?;
        assert_pixel(
            &frame.pixels,
            fw,
            qx0,
            qy1,
            SECONDARY_BOTTOM_LEFT,
            "selected-window bottom-left",
        )?;
        assert_pixel(
            &frame.pixels,
            fw,
            qx1,
            qy1,
            SECONDARY_BOTTOM_RIGHT,
            "selected-window bottom-right",
        )?;
        println!(
            "capture_frame OK: captured frame matches glass-fixture-2's distinct palette, not \
             glass-fixture's -- confirms capture followed the select_window retarget"
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
        // capture.rs's/input.rs's identical cleanup ordering for why this must run before
        // any exit (stop_app is idempotent/infallible-today, and this is what guarantees the
        // fixture's `quadrants` process — now potentially TWO windows' worth of it, still one
        // process — never survives a failed run).
        let stop_result = platform.stop_app();
        let _ = std::fs::remove_dir_all(&fixture_dir);

        match result {
            Ok(()) => {
                expect(stop_result, "stop_app");
                println!("WINDOW_INTEGRATION_PASS");
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
