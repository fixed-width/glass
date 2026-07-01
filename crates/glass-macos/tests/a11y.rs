//! Mac-gated accessibility-reader integration test — the first real-AX-tree proof through
//! the whole `glass-a11y-macos` snapshot path (`MacosPlatform::start_app` -> `AxContext` ->
//! `MacosA11y::snapshot` -> AXUIElement walk -> `AxTree`), driven against the `a11y_fixture`
//! Cocoa app (a "Save" button, an "Enable" checkbox, and an editable "Note" field holding
//! "hello").
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "a11y"` entry): like
//! `capture.rs`/`input.rs`/`windows.rs`, `MacosPlatform::start_app` reaches
//! `ffi::app_kit_init()` -> `NSApplication::sharedApplication(mtm)`, which requires the
//! process's TRUE main thread. libtest runs every `#[test]` on a worker thread, so this file
//! defines its own `fn main()` that — run directly rather than through libtest — is on the
//! real main thread. `MacosA11y::snapshot` itself runs inline on that same thread (AX has no
//! separate thread-affinity requirement).
//!
//! Needs the Accessibility (and Screen Recording, for `MacosPlatform::new`'s preflight) TCC
//! grants, which only the signed, granted `GlassProbe.app` bundle holds on this project's
//! dev Mac (`mini`) — see `capture.rs`'s module doc and `scripts/test-macos.sh` for how the
//! granted run copies this binary into that bundle. The fixture binary path is taken from
//! `GLASS_A11Y_FIXTURE_BIN` when set (the granted run pre-builds it); otherwise this builds
//! `fixture/a11y_fixture.swift` with `swiftc`, or skips if neither is available.

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

    use glass_core::{Accessibility, AppSpec, AxContext, Platform, SandboxLevel};
    use glass_macos::MacosPlatform;

    /// The three elements the fixture exposes, asserted as substrings of the tree outline.
    /// `to_outline` renders each node as `#<id> <Role> "<name>" ...`, so the button and
    /// checkbox match `Role "name"` and the editable field matches its value surfaced as the
    /// node name (`"hello"`).
    const NEEDLES: [&str; 3] = ["Button \"Save\"", "CheckBox \"Enable\"", "\"hello\""];

    /// Print a clear failure message and exit non-zero — the `harness = false` contract (no
    /// libtest to format a panic for us). Mirrors `capture.rs`.
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// Unwrap a `Result`, failing the process with `context` prefixed on `Err`. Only safe
    /// before a fixture process is spawned (it skips destructors) — post-spawn failures go
    /// through `try_expect`/`run_checks` so `stop_app` still runs. Mirrors `capture.rs`.
    fn expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> T {
        match result {
            Ok(v) => v,
            Err(e) => fail(format!("{context}: {e}")),
        }
    }

    /// Like `expect`, but returns the error as a `String` so a failure flows back to `run()`
    /// for `stop_app` + temp-dir cleanup before the process exits. Mirrors `capture.rs`.
    fn try_expect<T, E: std::fmt::Display>(result: Result<T, E>, context: &str) -> Result<T, String> {
        result.map_err(|e| format!("{context}: {e}"))
    }

    fn swiftc_available() -> bool {
        Command::new("swiftc").arg("--version").output().is_ok_and(|o| o.status.success())
    }

    /// Build `fixture/a11y_fixture.swift` to a fresh temp path. Returns the built binary's
    /// path and the temp build dir it lives in (the caller removes the dir when done).
    /// Mirrors `capture.rs::build_fixture` (`@main` type -> `-parse-as-library`).
    fn build_fixture() -> (PathBuf, PathBuf) {
        let manifest_dir = PathBuf::from(env!("CARGO_MANIFEST_DIR"));
        let source = manifest_dir.join("fixture").join("a11y_fixture.swift");
        if !source.is_file() {
            fail(format!("fixture source not found at {}", source.display()));
        }

        let out_dir = std::env::temp_dir().join(format!("glass-macos-a11y-test-{}", std::process::id()));
        expect(std::fs::create_dir_all(&out_dir), "creating fixture build dir");
        let out_bin = out_dir.join("a11y_fixture");

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
        (out_bin, out_dir)
    }

    /// Launch the fixture, snapshot its accessibility tree, and assert the outline contains
    /// each of [`NEEDLES`]. Returns `Err` instead of exiting so `run()` can always reach
    /// `stop_app` first (a bare `process::exit` here would skip `MacosPlatform::Drop` and
    /// leak the spawned fixture — same rationale as `capture.rs::run_checks`).
    fn run_checks(platform: &mut MacosPlatform, fixture_bin: &std::path::Path) -> Result<(), String> {
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

        // start_app only waits for the window to exist, not for AppKit to finish building
        // the accessibility tree behind it — give it a moment to settle before snapshotting.
        std::thread::sleep(Duration::from_millis(800));

        let ctx = AxContext {
            pids: platform.app_pids(),
            window: geometry.clone(),
            window_handle: None,
            a11y_bus_addr: None,
        };

        let mut a11y = glass_a11y_macos::MacosA11y::new();
        let mut tree = try_expect(a11y.snapshot(&ctx), "snapshot")?;
        tree.assign_ids(); // number nodes so the diagnostic outline reads naturally
        let outline = tree.to_outline();
        println!("a11y snapshot ({} nodes):\n{outline}", tree.count);

        for needle in NEEDLES {
            if !outline.contains(needle) {
                return Err(format!("missing {needle} in outline:\n{outline}"));
            }
        }
        Ok(())
    }

    pub(super) fn run() {
        // Prefer a pre-built fixture (the granted run supplies `GLASS_A11Y_FIXTURE_BIN`);
        // otherwise build it here, skipping cleanly if `swiftc` is unavailable.
        let (fixture_bin, fixture_dir) = match std::env::var_os("GLASS_A11Y_FIXTURE_BIN") {
            Some(p) => {
                let path = PathBuf::from(p);
                if !path.is_file() {
                    fail(format!("GLASS_A11Y_FIXTURE_BIN set but not a file: {}", path.display()));
                }
                (path, None)
            }
            None => {
                if !swiftc_available() {
                    println!("skipped (GLASS_A11Y_FIXTURE_BIN unset and no swiftc)");
                    return;
                }
                let (bin, dir) = build_fixture();
                (bin, Some(dir))
            }
        };
        println!("using a11y fixture at {}", fixture_bin.display());

        let cleanup_dir = |dir: &Option<PathBuf>| {
            if let Some(d) = dir {
                let _ = std::fs::remove_dir_all(d);
            }
        };

        let mut platform = match MacosPlatform::new() {
            Ok(p) => p,
            Err(e) => {
                cleanup_dir(&fixture_dir);
                fail(format!("MacosPlatform::new() (Screen Recording / Accessibility grant missing?): {e}"));
            }
        };

        let result = run_checks(&mut platform, &fixture_bin);

        // Reached on every path and BEFORE any process::exit below: stop_app is idempotent,
        // so this guarantees the fixture process never survives a failed run.
        let stop_result = platform.stop_app();
        cleanup_dir(&fixture_dir);

        match result {
            Ok(()) => {
                expect(stop_result, "stop_app");
                println!("A11Y_SNAPSHOT_PASS");
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
