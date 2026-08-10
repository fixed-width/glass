//! Helpers shared by the mac-gated integration tests and probes in this directory.
//!
//! All of them exist because these targets are `harness = false` (see `Cargo.toml`): there is no
//! libtest to format a panic, spawn a thread per test, or reap a fixture process, so each target
//! has to do those itself — and each had grown its own copy of the same seven or eight functions.
//!
//! Cargo does not treat a subdirectory of `tests/` as a target, so this is a module the targets
//! include rather than a test in its own right.
#![cfg(target_os = "macos")]
// Every target compiles the whole module and uses a subset of it — the probes never build a Swift
// fixture, and only the two pixel tests sample a frame.
#![allow(dead_code)]

use std::path::{Path, PathBuf};
use std::process::Command;

use glass_core::Platform;
use glass_macos::MacosPlatform;

/// Print a clear failure message and exit non-zero — the `harness = false` contract.
pub fn fail(msg: impl AsRef<str>) -> ! {
    eprintln!("FAIL: {}", msg.as_ref());
    std::process::exit(1);
}

/// Unwrap a `Result`, failing the whole test process with `context` prefixed to the error.
///
/// Only safe before a fixture process has been spawned, or once its cleanup is already done: it
/// exits immediately, skipping destructors, so anything still needing `stop_app` must go through
/// [`try_expect`] instead.
pub fn expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
    match result {
        Ok(v) => v,
        Err(e) => fail(format!("{context}: {e}")),
    }
}

/// Like [`expect`], but returns the error as a `String` rather than exiting, so a failure raised
/// with a fixture already spawned still flows back to the cleanup that reaps it.
pub fn try_expect<T, E: std::fmt::Display>(
    result: Result<T, E>,
    context: &str,
) -> Result<T, String> {
    result.map_err(|e| format!("{context}: {e}"))
}

pub fn swiftc_available() -> bool {
    Command::new("swiftc")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Build `fixture/{stem}.swift` to a fresh temp dir, returning the binary and the dir. The caller
/// removes the dir; [`run_fixture_test`] does it for the targets that use it.
///
/// The dir is named for the calling target — `env!` here expands per including target, so the name
/// cannot desync from it — and for the pid, which is what separates two runs of the same target.
/// The target's name is what makes a dir left behind by a crashed run say who leaked it, and stops
/// a later target that draws the same pid from re-entering it.
pub fn build_fixture(stem: &str) -> (PathBuf, PathBuf) {
    let tag = env!("CARGO_CRATE_NAME");
    let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
    let source = manifest_dir.join("fixture").join(format!("{stem}.swift"));
    if !source.is_file() {
        fail(format!("fixture source not found at {}", source.display()));
    }

    let out_dir =
        std::env::temp_dir().join(format!("glass-macos-{tag}-test-{}", std::process::id()));
    expect(
        std::fs::create_dir_all(&out_dir),
        "creating fixture build dir",
    );
    let out_bin = out_dir.join(stem);

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

/// Build the fixture, run `checks` against a fresh `MacosPlatform`, and clean up whatever happened.
///
/// The cleanup ordering is the point: `stop_app` and the temp-dir removal run before any
/// `process::exit`, which skips destructors — so without them a failed run leaves the fixture
/// process alive. `checks` returns `Result` rather than exiting for the same reason.
pub fn run_fixture_test(
    stem: &str,
    pass_marker: &str,
    checks: impl FnOnce(&mut MacosPlatform, &Path) -> Result<(), String>,
) {
    if !swiftc_available() {
        println!("skipped (no swiftc)");
        return;
    }

    let (fixture_bin, fixture_dir) = build_fixture(stem);
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

    let result = checks(&mut platform, &fixture_bin);

    let stop_result = platform.stop_app();
    let _ = std::fs::remove_dir_all(&fixture_dir);

    match result {
        Ok(()) => {
            expect(stop_result, "stop_app");
            println!("{pass_marker}");
            std::process::exit(0);
        }
        Err(msg) => {
            // `stop_app` is infallible today, but this is the last point before the process exits
            // and so the only chance to report a future failure of it.
            if let Err(e) = stop_result {
                eprintln!("(additionally) stop_app failed: {e}");
            }
            fail(msg);
        }
    }
}

/// Run `body`, then always `stop_app` afterwards, whether or not `body` succeeded.
///
/// On success a `stop_app` failure becomes the caller's failure; on an already-failing `body` it is
/// only logged, so it cannot mask the original.
pub fn with_stop_app<T>(
    platform: &mut MacosPlatform,
    label: &str,
    body: impl FnOnce(&mut MacosPlatform) -> Result<T, String>,
) -> Result<T, String> {
    let result = body(platform);
    let stop_result = platform.stop_app();
    match result {
        Ok(v) => stop_result
            .map(|()| v)
            .map_err(|e| format!("stop_app({label}): {e}")),
        Err(e) => {
            if let Err(stop_err) = stop_result {
                eprintln!("(additionally) stop_app({label}) failed: {stop_err}");
            }
            Err(e)
        }
    }
}

/// Per-channel RGBA tolerance for a sampled pixel against its expected colour, shared by the two
/// targets that sample one. Generous next to a `deviceRGB` fill, which lands close to exact; sample
/// points are quadrant centres, away from colour boundaries, so this is margin rather than a
/// load-bearing bound.
pub const PIXEL_TOLERANCE: i32 = 40;

/// Fetch pixel `(x, y)` from a tightly-packed RGBA8 buffer.
pub fn pixel_at(pixels: &[u8], frame_width: u32, x: u32, y: u32) -> [u8; 4] {
    let idx = (y as usize * frame_width as usize + x as usize) * 4;
    [
        pixels[idx],
        pixels[idx + 1],
        pixels[idx + 2],
        pixels[idx + 3],
    ]
}

pub fn close(a: [u8; 4], b: [u8; 4]) -> bool {
    a.iter()
        .zip(b.iter())
        .all(|(x, y)| (*x as i32 - *y as i32).abs() <= PIXEL_TOLERANCE)
}

/// Assert the pixel at `(x, y)` is within tolerance of `expected`, returning the actual bytes on
/// mismatch. `Result`, not a process exit, so a mismatch found with a fixture spawned still
/// reaches the cleanup that reaps it.
pub fn assert_pixel(
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
            "{label} pixel at ({x},{y}) = {got:?}, expected ~{expected:?} \
             (tolerance {PIXEL_TOLERANCE})"
        ));
    }
    Ok(())
}
