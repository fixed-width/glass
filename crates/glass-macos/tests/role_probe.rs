//! Role-histogram PROBE for the macOS half of the accessibility role-parity work — not a
//! pass/fail assertion test. Launches whatever apps `GLASS_A11Y_PROBE_APPS` names (a
//! comma-separated list of launch commands — see this file's `macos_main::PROBE_APPS_VAR`
//! doc comment), snapshots each through `glass-a11y-macos` with the node cap lifted, and
//! prints `glass_core::role_histogram`: every native AX role string the app
//! actually emitted, unmapped ([`glass_core::AxRole::Other`]) tokens first. The project's rule
//! is probe first, map second — a `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` may
//! only get a match arm for a token that showed up in output like this — so this file's only
//! job is to produce that evidence. It never asserts what the tokens should be, and fails a
//! run only when a snapshot could not be taken at all for one of the requested apps (a real
//! breakage, distinct from an app simply exposing something unexpected).
//!
//! With `GLASS_A11Y_PROBE_APPS` unset (or set but empty), prints what to set and exits 0 —
//! the same skip-not-fail convention `tests/a11y.rs` uses for its own missing fixture, so a
//! plain on-box run that isn't asking for this probe never fails because of it.
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "role_probe"` entry): same
//! reason as `capture.rs`/`input.rs`/`windows.rs`/`a11y.rs`/`bundle_launch.rs` —
//! `MacosPlatform::start_app` reaches AppKit's `ffi::app_kit_init()`, which needs the
//! process's TRUE main thread; libtest's per-`#[test]` worker threads can't provide that, so
//! this file defines its own `fn main()`, run directly rather than through libtest.
//!
//! Needs the same Accessibility (and Screen Recording, for `MacosPlatform::new`'s preflight)
//! TCC grants as `tests/a11y.rs` — see that file's module doc for how a granted run supplies
//! them.

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
    use std::time::Duration;

    use glass_a11y_macos::MacosA11y;
    use glass_core::{
        role_histogram, Accessibility, AppSpec, AxContext, AxRole, AxTree, Platform, SandboxLevel,
        WalkLimits,
    };
    use glass_macos::MacosPlatform;

    /// Comma-separated launch commands to probe, e.g.
    /// `/System/Applications/TextEdit.app,/System/Applications/Calculator.app`. Each element
    /// is either a `.app` bundle path or a plain executable path — exactly what
    /// `AppSpec::run`'s first element accepts (see `MacosPlatform::start_app`'s
    /// bundle-vs-direct-spawn dispatch, exercised by `tests/bundle_launch.rs`). Read once at
    /// the top of [`run`]; unset (or set but empty) skips the whole probe — prints what to
    /// set, exits 0 — rather than failing a run that never asked for it.
    const PROBE_APPS_VAR: &str = "GLASS_A11Y_PROBE_APPS";

    /// How long to wait after `start_app` before snapshotting — AppKit finishes building the
    /// accessibility tree behind a window a beat after the window itself appears; mirrors
    /// `tests/a11y.rs`'s identically-reasoned settle.
    const STARTUP_SETTLE: Duration = Duration::from_millis(800);

    /// Print a clear failure message and exit non-zero — the `harness = false` contract (no
    /// libtest to format a panic for us). Mirrors the sibling integration tests.
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// Print `role_histogram(tree)` as one line per `(token, role)` bucket — unmapped
    /// ([`AxRole::Other`]) buckets first, which is already the histogram's own sort order, so
    /// the tokens most worth a human's attention are the first thing printed.
    fn print_role_histogram(label: &str, tree: &AxTree) {
        let hist = role_histogram(tree);
        println!("\n===== role histogram: {label} =====");
        println!(
            "{} nodes, {} distinct (token, role) buckets",
            tree.count,
            hist.len()
        );
        if let Some(t) = &tree.truncated {
            println!("  NOTE: {}", t.notice());
        }
        for entry in &hist {
            let tag = if entry.role == AxRole::Other {
                "UNMAPPED"
            } else {
                "mapped"
            };
            println!(
                "  {tag:>8}  x{:<5} role={:?} token={:?}",
                entry.count, entry.role, entry.raw_role
            );
        }
    }

    /// Run `body`, then always call `platform.stop_app()` afterward regardless of whether
    /// `body` succeeded — mirrors `tests/bundle_launch.rs`'s identically-named helper.
    fn with_stop_app<T>(
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

    /// Launch `run0`, snapshot it with the node cap lifted (so a big app's tree is never
    /// truncated mid-probe — depth/siblings keep their generous structural-rail defaults
    /// regardless; see [`WalkLimits::from_max_nodes`]), print its role histogram, then stop
    /// it. Returns `Err` only when the app could not be launched or a snapshot could not be
    /// taken at all — a real breakage, never merely an unexpected role (see this file's
    /// module doc). What the histogram actually contains is never asserted here; that's the
    /// human's job, reading the printed output to decide which `Gap` cell in
    /// `glass_core::role_support::ROLE_SUPPORT` a real native token now justifies filling.
    fn probe_one(run0: &str) -> Result<(), String> {
        println!("\n--- launching {run0} ---");
        let mut platform =
            MacosPlatform::new().map_err(|e| format!("MacosPlatform::new() for {run0}: {e}"))?;

        let spec = AppSpec {
            build: None,
            run: vec![run0.to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 15_000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };

        with_stop_app(&mut platform, run0, |platform| {
            let geometry = platform
                .start_app(&spec)
                .map_err(|e| format!("start_app({run0}): {e}"))?;
            println!("started {run0}: {geometry:?}");
            std::thread::sleep(STARTUP_SETTLE);

            let ctx = AxContext {
                pids: platform.app_pids(),
                window: geometry,
                window_handle: None,
                a11y_bus_addr: None,
                limits: WalkLimits::from_max_nodes(Some(0)),
            };
            let mut a11y = MacosA11y::new();
            let mut tree = a11y
                .snapshot(&ctx)
                .map_err(|e| format!("snapshot({run0}): {e}"))?;
            // `MacosA11y::snapshot` deliberately leaves `count` (and node ids) at their
            // zero default — `glass-a11y-macos`'s own doc says numbering is assigned by
            // `glass-core` so it's identical across backends (`tests/a11y.rs` calls this
            // too, right after its own snapshot). Skipping it here was the earlier bug: the
            // printed histogram's bucket counts were always right (`role_histogram` walks
            // the tree structurally, not by id), but the "N nodes" header read 0 regardless
            // of how big the tree actually was.
            tree.assign_ids();
            print_role_histogram(run0, &tree);
            Ok(())
        })
    }

    pub(super) fn run() {
        let apps = match std::env::var(PROBE_APPS_VAR) {
            Ok(v) if !v.trim().is_empty() => v,
            _ => {
                println!(
                    "skipped: set {PROBE_APPS_VAR} to a comma-separated list of launch \
                     commands (.app bundle paths or executable paths) to probe, e.g. \
                     {PROBE_APPS_VAR}=/System/Applications/TextEdit.app"
                );
                std::process::exit(0);
            }
        };
        let targets: Vec<&str> = apps
            .split(',')
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .collect();
        if targets.is_empty() {
            println!("skipped: {PROBE_APPS_VAR} was set but named no launch commands");
            std::process::exit(0);
        }

        let mut failures = Vec::new();
        for run0 in targets {
            if let Err(e) = probe_one(run0) {
                failures.push(e);
            }
        }

        if !failures.is_empty() {
            fail(failures.join("\n\n"));
        }
        println!("\nROLE_PROBE_PASS");
        std::process::exit(0);
    }
}
