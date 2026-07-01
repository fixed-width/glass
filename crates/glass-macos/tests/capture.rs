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
    /// error on `Err`.
    fn expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(v) => v,
            Err(e) => fail(format!("{context}: {e}")),
        }
    }

    fn swiftc_available() -> bool {
        Command::new("swiftc").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// Build `fixture/quadrants.swift` to a fresh temp path. Returns the built binary's
    /// path.
    fn build_fixture() -> PathBuf {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir.join("fixture").join("quadrants.swift");
        if !source.is_file() {
            fail(format!("fixture source not found at {}", source.display()));
        }

        let out_dir = std::env::temp_dir().join(format!("glass-macos-capture-test-{}", std::process::id()));
        expect(std::fs::create_dir_all(&out_dir), "creating fixture build dir");
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
            Ok(s) => fail(format!("swiftc exited with {s} building {}", source.display())),
            Err(e) => fail(format!("failed to run swiftc: {e}")),
        }
        out_bin
    }

    /// Fetch pixel `(x, y)` from a tightly-packed RGBA8 `Frame` buffer.
    fn pixel_at(pixels: &[u8], frame_width: u32, x: u32, y: u32) -> [u8; 4] {
        let idx = (y as usize * frame_width as usize + x as usize) * 4;
        [pixels[idx], pixels[idx + 1], pixels[idx + 2], pixels[idx + 3]]
    }

    fn close(a: [u8; 4], b: [u8; 4]) -> bool {
        a.iter().zip(b.iter()).all(|(x, y)| (*x as i32 - *y as i32).abs() <= TOLERANCE)
    }

    /// Assert the pixel at `(x, y)` in `frame` is within tolerance of `expected`, failing
    /// the process with a detailed message (including the actual bytes) otherwise.
    fn assert_pixel(
        pixels: &[u8],
        frame_width: u32,
        x: u32,
        y: u32,
        expected: [u8; 4],
        label: &str,
    ) {
        let got = pixel_at(pixels, frame_width, x, y);
        if !close(got, expected) {
            fail(format!(
                "{label} pixel at ({x},{y}) = {got:?}, expected ~{expected:?} (tolerance {TOLERANCE})"
            ));
        }
    }

    /// Assert every pixel in a tightly-packed RGBA8 buffer of `w`x`h` is within tolerance
    /// of `expected`.
    fn assert_uniform(pixels: &[u8], w: u32, h: u32, expected: [u8; 4], label: &str) {
        for y in 0..h {
            for x in 0..w {
                let got = pixel_at(pixels, w, x, y);
                if !close(got, expected) {
                    fail(format!(
                        "{label}: non-uniform pixel at ({x},{y}) = {got:?}, expected ~{expected:?} \
                         (tolerance {TOLERANCE}) within a {w}x{h} region"
                    ));
                }
            }
        }
    }

    pub(super) fn run() {
        if !swiftc_available() {
            println!("skipped (no swiftc)");
            return;
        }

        let fixture_bin = build_fixture();
        println!("built fixture at {}", fixture_bin.display());

        let mut platform = expect(
            MacosPlatform::new(),
            "MacosPlatform::new() (Screen Recording / Accessibility grant missing?)",
        );

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

        let geometry = expect(platform.start_app(&spec), "start_app");
        println!("started fixture window: {geometry:?}");

        // start_app only waits for the window to *exist* (SCShareableContent
        // enumeration), not for its first paint to land — give the initial draw() a
        // moment to complete before capturing.
        std::thread::sleep(Duration::from_millis(500));

        let frame = expect(platform.capture_frame(None), "capture_frame(None)");
        println!("captured {}x{} frame", frame.width, frame.height);

        if frame.width < 2 || frame.height < 2 {
            fail(format!("captured frame too small to sample quadrants: {}x{}", frame.width, frame.height));
        }

        // Quadrant centers, in the captured Frame's own coordinate system (row-major,
        // top-left origin per glass_core::frame::Frame's contract). The fixture draws its
        // four *visual* quadrants (as seen on screen) directly at these same corners —
        // see quadrants.swift's header — so top-left/top-right/bottom-left/bottom-right
        // below name the same corners on both sides.
        let (fw, fh) = (frame.width, frame.height);
        let (qx0, qx1) = (fw / 4, fw * 3 / 4);
        let (qy0, qy1) = (fh / 4, fh * 3 / 4);
        assert_pixel(&frame.pixels, fw, qx0, qy0, RED, "top-left");
        assert_pixel(&frame.pixels, fw, qx1, qy0, GREEN, "top-right");
        assert_pixel(&frame.pixels, fw, qx0, qy1, BLUE, "bottom-left");
        assert_pixel(&frame.pixels, fw, qx1, qy1, WHITE, "bottom-right");
        println!("full-frame quadrant colors OK");

        // Crop to the top-left quadrant (frame-relative Region) and assert it's uniformly
        // red and exactly half-sized.
        let half_w = fw / 2;
        let half_h = fh / 2;
        let region = Region { x: 0, y: 0, width: half_w, height: half_h };
        let cropped = expect(platform.capture_frame(Some(&region)), "capture_frame(Some(top-left region))");
        if cropped.width != half_w || cropped.height != half_h {
            fail(format!(
                "cropped frame is {}x{}, expected {half_w}x{half_h}",
                cropped.width, cropped.height
            ));
        }
        assert_uniform(&cropped.pixels, cropped.width, cropped.height, RED, "cropped top-left region");
        println!("region-crop OK: {}x{} uniformly red", cropped.width, cropped.height);

        expect(platform.stop_app(), "stop_app");

        println!("CAPTURE_INTEGRATION_PASS");
        std::process::exit(0);
    }
}
