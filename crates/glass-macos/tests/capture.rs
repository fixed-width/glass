//! Mac-gated capture integration test — the first real-pixels proof through the whole
//! `MacosPlatform` capture path (`MacosPlatform::new` -> `start_app` -> `capture_frame`
//! -> ScreenCaptureKit -> `Frame`).
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "capture"` entry): `Platform`
//! calls that touch AppKit (`capture_frame`, `start_app`'s window discovery) reach
//! `ffi::app_kit_init()` -> `NSApplication::sharedApplication(mtm)`, which requires the
//! process's TRUE main thread (`objc2::MainThreadMarker`). libtest runs every `#[test]` on
//! a spawned worker thread, so a normal harness test would panic on
//! `MainThreadMarker::new().expect(...)`. This file defines its own `fn main()` instead,
//! which — when this binary is executed directly rather than through libtest — runs on the
//! real main thread.
//!
//! Needs the Screen Recording TCC grant, which only a signed, granted app bundle holds on
//! this project's dev Mac (`mini`). A plain `cargo test --test capture` build (this file
//! compiles and can run) will still fail at the grant check unless run in that granted
//! context — the actual granted run copies this test binary into the granted
//! `GlassProbe.app` bundle, re-signs it, and launches it via a `gui/501` LaunchAgent so it
//! inherits the bundle's grants. See `.superpowers/sdd/objc2-spike-report.md` and
//! `.superpowers/sdd/task-6-brief.md` for the exact procedure, and
//! `scripts/test-macos.sh`'s `GLASS_MACOS_ONBOX` gate for how this fits the test scripts.

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

    use glass_core::{AppSpec, Platform, Region, SandboxLevel};
    use glass_macos::MacosPlatform;

    /// Per-channel RGBA tolerance for a sampled pixel vs. its expected known color.
    /// Generous relative to `deviceRGB` fills (which should land very close to exact),
    /// but wide enough to absorb any residual compositor/backing-scale blending at a
    /// sampled point (sample points are quadrant *centers*, away from color boundaries,
    /// so this is a safety margin, not a load-bearing tolerance).
    const TOLERANCE: i32 = 40;

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

    /// Print a clear failure message and exit non-zero — the `harness = false` contract
    /// (no libtest to format a panic for us).
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// Unwrap a `Result`, failing the whole test process with `context` prefixed to the
    /// error on `Err`. Only safe to use before a fixture process has been spawned (or
    /// once all spawn-time cleanup is already done) — it exits immediately, skipping
    /// destructors, so anything that still needs `MacosPlatform::stop_app()` run against
    /// it must go through `try_expect`/`run_checks` below instead.
    fn expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(v) => v,
            Err(e) => fail(format!("{context}: {e}")),
        }
    }

    /// Like `expect`, but returns the error as a `String` instead of exiting the
    /// process. Used inside `run_checks`, where a failure must still flow back to
    /// `run()` so it can `stop_app()` the spawned fixture (and clean up the fixture
    /// build dir) before the process exits — `std::process::exit` skips Rust
    /// destructors, so `MacosPlatform::Drop` would never reap the child if we exited
    /// straight from in here.
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

    /// Build `fixture/quadrants.swift` to a fresh temp path. Returns the built binary's
    /// path and the temp build dir it lives in (the caller is responsible for removing
    /// the dir once done with it — see `run()`).
    fn build_fixture() -> (PathBuf, PathBuf) {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir.join("fixture").join("quadrants.swift");
        if !source.is_file() {
            fail(format!("fixture source not found at {}", source.display()));
        }

        let out_dir =
            std::env::temp_dir().join(format!("glass-macos-capture-test-{}", std::process::id()));
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

    /// Fetch pixel `(x, y)` from a tightly-packed RGBA8 `Frame` buffer.
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
            .all(|(x, y)| (*x as i32 - *y as i32).abs() <= TOLERANCE)
    }

    /// Assert the pixel at `(x, y)` in `frame` is within tolerance of `expected`,
    /// returning a detailed `Err` message (including the actual bytes) otherwise. Returns
    /// `Result` rather than failing the process directly so a mismatch, raised from
    /// inside `run_checks` with a fixture process already spawned, still flows back to
    /// `run()`'s cleanup (`stop_app` + temp dir removal) before the process exits.
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
                "{label} pixel at ({x},{y}) = {got:?}, expected ~{expected:?} (tolerance {TOLERANCE})"
            ));
        }
        Ok(())
    }

    /// Assert every pixel in a tightly-packed RGBA8 buffer of `w`x`h` is within tolerance
    /// of `expected`. Returns `Result` for the same reason as `assert_pixel`.
    fn assert_uniform(
        pixels: &[u8],
        w: u32,
        h: u32,
        expected: [u8; 4],
        label: &str,
    ) -> Result<(), String> {
        for y in 0..h {
            for x in 0..w {
                let got = pixel_at(pixels, w, x, y);
                if !close(got, expected) {
                    return Err(format!(
                        "{label}: non-uniform pixel at ({x},{y}) = {got:?}, expected ~{expected:?} \
                         (tolerance {TOLERANCE}) within a {w}x{h} region"
                    ));
                }
            }
        }
        Ok(())
    }

    /// The whole capture-and-assert flow, from launching the fixture through the last
    /// pixel assertion. Returns `Err` instead of exiting the process on any failure, so
    /// `run()` can always reach `platform.stop_app()` first — a bare `std::process::exit`
    /// from in here would skip `MacosPlatform::Drop` (Rust destructors don't run across
    /// `exit`) and leak the spawned `quadrants` fixture process (reparented to launchd,
    /// accumulating stray windows across repeated failed runs).
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

        // start_app only waits for the window to *exist* (SCShareableContent
        // enumeration), not for its first paint to land — give the initial draw() a
        // moment to complete before capturing. This fixture draws once, synchronously,
        // immediately on launch, so a fixed sleep is sufficient here; it is NOT a
        // wait-for-first-paint pattern to reuse for apps with slower or async first
        // paints.
        std::thread::sleep(Duration::from_millis(500));

        let frame = try_expect(platform.capture_frame(None), "capture_frame(None)")?;
        println!("captured {}x{} frame", frame.width, frame.height);

        if frame.width < 2 || frame.height < 2 {
            return Err(format!(
                "captured frame too small to sample quadrants: {}x{}",
                frame.width, frame.height
            ));
        }

        // Quadrant centers, in the captured Frame's own coordinate system (row-major,
        // top-left origin per glass_core::frame::Frame's contract). The fixture draws its
        // four *visual* quadrants (as seen on screen) directly at these same corners —
        // see quadrants.swift's header — so top-left/top-right/bottom-left/bottom-right
        // below name the same corners on both sides.
        let (fw, fh) = (frame.width, frame.height);
        let (qx0, qx1) = (fw / 4, fw * 3 / 4);
        let (qy0, qy1) = (fh / 4, fh * 3 / 4);
        assert_pixel(&frame.pixels, fw, qx0, qy0, RED, "top-left")?;
        assert_pixel(&frame.pixels, fw, qx1, qy0, GREEN, "top-right")?;
        assert_pixel(&frame.pixels, fw, qx0, qy1, BLUE, "bottom-left")?;
        assert_pixel(&frame.pixels, fw, qx1, qy1, WHITE, "bottom-right")?;
        println!("full-frame quadrant colors OK");

        // Crop to the top-left quadrant (frame-relative Region) and assert it's uniformly
        // red and exactly half-sized.
        let half_w = fw / 2;
        let half_h = fh / 2;
        let region = Region {
            x: 0,
            y: 0,
            width: half_w,
            height: half_h,
        };
        let cropped = try_expect(
            platform.capture_frame(Some(&region)),
            "capture_frame(Some(top-left region))",
        )?;
        if cropped.width != half_w || cropped.height != half_h {
            return Err(format!(
                "cropped frame is {}x{}, expected {half_w}x{half_h}",
                cropped.width, cropped.height
            ));
        }
        assert_uniform(
            &cropped.pixels,
            cropped.width,
            cropped.height,
            RED,
            "cropped top-left region",
        )?;
        println!(
            "region-crop OK: {}x{} uniformly red",
            cropped.width, cropped.height
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

        // Reached regardless of outcome, and BEFORE any process::exit below: stop_app is
        // documented idempotent (a no-op if start_app never got far enough to spawn a
        // child), so this is always safe and is what guarantees the fixture's `quadrants`
        // process never survives a failed run. Best-effort temp dir cleanup rides along
        // here too, on both the success and failure paths.
        let stop_result = platform.stop_app();
        let _ = std::fs::remove_dir_all(&fixture_dir);

        match result {
            Ok(()) => {
                expect(stop_result, "stop_app");
                println!("CAPTURE_INTEGRATION_PASS");
                std::process::exit(0);
            }
            Err(msg) => {
                // stop_app is infallible today (always Ok), but surface a future failure
                // here too rather than silently dropping it — this is the last point
                // before the process exits, so it's the only chance to report it.
                if let Err(e) = stop_result {
                    eprintln!("(additionally) stop_app failed: {e}");
                }
                fail(msg);
            }
        }
    }
}
