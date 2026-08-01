//! Window-adoption PROBE for glass#263 — not a pass/fail assertion test. Launches an app
//! repeatedly and, on each run, prints the window `start_app` adopted alongside every
//! on-screen window the app's pid actually owns, then reports whether the accessibility
//! reader could resolve the adopted window at all.
//!
//! It exists because `scwindow::find_on_screen_window` adopts the FIRST on-screen `SCWindow`
//! in `SCShareableContent` order owned by the target pid — no filter on layer, size or title,
//! and no ordering guarantee from the framework. On 2026-07-29 one run adopted a `468x101`
//! window that matches no `AXWindow` Calculator owns, and the runs either side of it adopted
//! the real `230x408` window. This probe's job is to say what that window is, with its title,
//! rather than leave the next reader guessing.
//!
//! Asserts nothing about which window is correct. It fails a run only when an app could not be
//! launched or enumerated at all — a real breakage, distinct from an app exposing something
//! unexpected. Same convention as `role_probe.rs`.
//!
//! With `GLASS_WINDOW_PROBE_APP` unset (or set but empty), prints what to set and exits 0.
//!
//! **`harness = false`** (see `Cargo.toml`'s `[[test]] name = "window_adopt_probe"` entry):
//! same reason as `capture.rs`/`input.rs`/`windows.rs`/`a11y.rs`/`bundle_launch.rs`/
//! `role_probe.rs` — `MacosPlatform::start_app` reaches AppKit's `ffi::app_kit_init()`, which
//! needs the process's TRUE main thread; libtest's per-`#[test]` worker threads can't provide
//! that, so this file defines its own `fn main()`.
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
        Accessibility, AppSpec, AxContext, Platform, SandboxLevel, WalkLimits, WindowInfo,
    };
    use glass_macos::MacosPlatform;

    /// The launch command to probe — a `.app` bundle path or a plain executable path, exactly
    /// what `AppSpec::run`'s first element accepts (see `MacosPlatform::start_app`'s
    /// bundle-vs-direct-spawn dispatch). Unset (or set but empty) skips the whole probe —
    /// prints what to set, exits 0 — rather than failing a run that never asked for it.
    const PROBE_APP_VAR: &str = "GLASS_WINDOW_PROBE_APP";

    /// How many launch/enumerate/stop cycles to run. The adoption this probe hunts was seen
    /// once across a handful of runs, so the default is high enough to give a rare event a
    /// chance without needing an argument every time.
    const PROBE_RUNS_VAR: &str = "GLASS_WINDOW_PROBE_RUNS";
    const DEFAULT_RUNS: usize = 30;

    /// How many times to re-enumerate the app's windows after `start_app` returns, and the
    /// gap between enumerations. Spread across ~1.25s (5 gaps) so a window that appears (or
    /// vanishes) a beat after adoption is still caught — the transient this probe is looking for.
    const ENUMERATIONS: usize = 6;
    const ENUMERATION_GAP: Duration = Duration::from_millis(250);

    /// Print a clear failure message and exit non-zero — the `harness = false` contract (no
    /// libtest to format a panic for us). Mirrors the sibling integration tests.
    fn fail(msg: impl AsRef<str>) -> ! {
        eprintln!("FAIL: {}", msg.as_ref());
        std::process::exit(1);
    }

    /// One window as one line: everything needed to tell a real app window from a transient
    /// panel — its id, title, pixel geometry, and whether the backend considers it active.
    fn render_window(w: &WindowInfo) -> String {
        format!(
            "id={} {} {}x{} @({},{}){}",
            w.id.0,
            w.title
                .as_deref()
                .map_or_else(|| "<untitled>".to_string(), |t| format!("{t:?}")),
            w.geometry.width,
            w.geometry.height,
            w.geometry.x,
            w.geometry.y,
            if w.active { " ACTIVE" } else { "" }
        )
    }

    /// Run `body`, then always call `platform.stop_app()` afterward regardless of whether
    /// `body` succeeded — mirrors `tests/role_probe.rs`'s identically-named helper.
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

    /// One launch/enumerate/stop cycle. `Ok(true)` means the adopted geometry matched at least
    /// one window the app's pid actually owns during the enumeration window; `Ok(false)` is the
    /// condition this probe hunts. `Err` is a real breakage — the app would not launch, or its
    /// windows could not be enumerated at all.
    fn probe_once(run0: &str, run_index: usize) -> Result<bool, String> {
        let mut platform =
            MacosPlatform::new().map_err(|e| format!("MacosPlatform::new(): {e}"))?;

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
            let adopted = platform
                .start_app(&spec)
                .map_err(|e| format!("start_app({run0}): {e}"))?;
            println!("\n===== run {run_index} =====");
            println!("adopted: {adopted:?}");

            let mut matched = false;
            for enumeration in 0..ENUMERATIONS {
                let windows = platform
                    .list_windows()
                    .map_err(|e| format!("list_windows({run0}): {e}"))?;
                println!(
                    "  t+{}ms: {} window(s)",
                    enumeration as u128 * ENUMERATION_GAP.as_millis(),
                    windows.len()
                );
                for w in &windows {
                    let is_adopted = w.geometry == adopted;
                    matched |= is_adopted;
                    let tag = if is_adopted { "  == ADOPTED" } else { "" };
                    println!("    {}{tag}", render_window(w));
                }
                if enumeration + 1 < ENUMERATIONS {
                    std::thread::sleep(ENUMERATION_GAP);
                }
            }

            // The symptom the issue reports: whether the reader can resolve the window the
            // backend adopted. Never fatal — a `WindowNotFound` here is the finding, not a
            // probe breakage.
            let ctx = AxContext {
                pids: platform.app_pids(),
                window: adopted,
                window_handle: None,
                a11y_bus_addr: None,
                // Only Ok/Err is read below, and that outcome turns on root window resolution,
                // not tree size — cap at 1 node instead of paying for the full tree.
                limits: WalkLimits::from_max_nodes(Some(1)),
            };
            match MacosA11y::new().snapshot(&ctx) {
                Ok(_) => println!("  snapshot: ok"),
                Err(e) => println!("  snapshot: FAILED: {e}"),
            }

            if !matched {
                println!("  VERDICT: adopted geometry matched NO window owned by this pid");
            }
            Ok(matched)
        })
    }

    pub(super) fn run() {
        let run0 = match std::env::var(PROBE_APP_VAR) {
            Ok(v) if !v.trim().is_empty() => v.trim().to_string(),
            _ => {
                println!(
                    "skipped: set {PROBE_APP_VAR} to a launch command (a .app bundle path or an \
                     executable path) to probe, e.g. \
                     {PROBE_APP_VAR}=/System/Applications/Calculator.app"
                );
                std::process::exit(0);
            }
        };
        let runs = std::env::var(PROBE_RUNS_VAR)
            .ok()
            .and_then(|v| v.trim().parse::<usize>().ok())
            .filter(|n| *n > 0)
            .unwrap_or(DEFAULT_RUNS);

        let mut mismatches = 0usize;
        let mut failures = Vec::new();
        for run_index in 1..=runs {
            match probe_once(&run0, run_index) {
                Ok(true) => {}
                Ok(false) => mismatches += 1,
                Err(e) => failures.push(format!("run {run_index}: {e}")),
            }
        }

        if !failures.is_empty() {
            fail(failures.join("\n\n"));
        }
        println!("\n{mismatches} of {runs} run(s) adopted a geometry matching no listed window");
        println!("WINDOW_ADOPT_PROBE_PASS");
        std::process::exit(0);
    }
}
