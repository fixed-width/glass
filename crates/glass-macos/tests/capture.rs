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
//! this project's dev Mac. A plain `cargo test --test capture` build (this file
//! compiles and can run) will still fail at the grant check unless run in that granted
//! context — the actual granted run copies this test binary into the granted
//! `GlassProbe.app` bundle, re-signs it, and launches it via a `gui/501` LaunchAgent so it
//! inherits the bundle's grants. See `scripts/test-macos.sh`'s `GLASS_MACOS_ONBOX` gate
//! for how this fits the test scripts.

mod common;

#[cfg(not(target_os = "macos"))]
fn main() {
    println!("skipped (not macOS): test");
}

#[cfg(target_os = "macos")]
fn main() {
    macos_main::run();
}

#[cfg(target_os = "macos")]
mod macos_main {
    use std::time::Duration;

    use glass_core::{AppSpec, Platform, Region, SandboxLevel};
    use glass_macos::MacosPlatform;

    use crate::common::{
        PIXEL_TOLERANCE, assert_pixel, close, pixel_at, run_fixture_test, try_expect,
    };

    const RED: [u8; 4] = [255, 0, 0, 255];
    const GREEN: [u8; 4] = [0, 255, 0, 255];
    const BLUE: [u8; 4] = [0, 0, 255, 255];
    const WHITE: [u8; 4] = [255, 255, 255, 255];

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
                         (tolerance {PIXEL_TOLERANCE}) within a {w}x{h} region"
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

        // start_app only waits for the window to *exist*, not for its first paint to land.
        // This fixture draws once, synchronously, on launch, so a fixed sleep suffices — do
        // NOT reuse it as a wait-for-first-paint pattern for apps with slower or async paints.
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
        run_fixture_test("quadrants", "CAPTURE_INTEGRATION_PASS", run_checks);
    }
}
