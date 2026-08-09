//! Window-adoption PROBE for glass#263 — not a pass/fail assertion test. Launches an app
//! repeatedly and, on each run, prints the window `start_app` adopted alongside every
//! on-screen window the app's pid actually owns, then reports whether the accessibility
//! reader could resolve the adopted window at all.
//!
//! It exists because `scwindow::scan_on_screen_windows` (reached via
//! `backend::discover_window_pid` -> `scwindow::query_once_with_candidates` ->
//! `scwindow::query_once_inner`) adopts the FIRST
//! on-screen `SCWindow` in `SCShareableContent` order owned by the target pid — no filter on
//! layer, size or title, and no ordering guarantee from the framework. On 2026-07-29 one run
//! adopted a `468x101` window that matches no `AXWindow` Calculator owns, and the runs either
//! side of it adopted the real `230x408` window. This probe's job is to say what that window
//! is, with its title, rather than leave the next reader guessing.
//!
//! **Whether a `.app` bundle launches fresh or gets adopted depends on the path — read this before
//! trusting a sweep.** Apple platform code (e.g. Calculator, this probe's own example) hands off
//! straight to `NSWorkspace`/LaunchServices; a third-party bundle direct-spawns its inner
//! executable first, reaching LaunchServices only if that spawn exits before a window appears
//! (`start_bundle`'s doc). Either way, an instance already running at handoff is adopted and
//! raised, not spawned fresh, and `stop_app` deliberately leaves a pre-existing adoptee running
//! (glass only raised it, so it isn't glass's to kill — see `backend.rs`'s `Adopted`/`Disposition`
//! doc). A sweep against an app that's already open can therefore re-adopt the SAME process every
//! run, not N independent launches, and never re-test a fresh-launch race. To probe cold launches,
//! quit the target app completely before starting the sweep and again between sweeps; the per-run
//! pid this probe prints, and the summary's distinct-pid count, are how to tell a cold-launch sweep
//! from a re-adoption one after the fact.
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
    println!("skipped (not macOS): test");
}

#[cfg(target_os = "macos")]
fn main() {
    macos_main::run();
}

#[cfg(target_os = "macos")]
mod macos_main {
    use std::time::{Duration, Instant};

    use glass_a11y_macos::MacosA11y;
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxDeadline, Platform, SandboxLevel, WalkLimits,
        WindowGeometry, WindowInfo, WindowOp,
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

    /// How many times to re-enumerate the app's windows after `start_app` returns, and the gap
    /// between enumerations. The first pass runs immediately, before early sampling begins (see
    /// `EARLY_SAMPLE_WINDOW`) — previously, a transient that appeared and vanished in that gap
    /// was never enumerated. The remaining `ENUMERATIONS - 1` passes run after early sampling
    /// ends, `ENUMERATION_GAP` apart, so a transient a beat later is still caught too.
    const ENUMERATIONS: usize = 6;
    const ENUMERATION_GAP: Duration = Duration::from_millis(250);

    /// How often, and for how long, to re-read the adopted window's geometry once the first
    /// enumeration pass has run (see `ENUMERATIONS`) — so sampling starts one `list_windows` round
    /// trip after `start_app` returns, not at zero. macOS reports a window's geometry while it is
    /// still opening, so the value adoption captured can be a frame of that animation. 10ms is a
    /// floor, not a guarantee — each sample is a real round trip
    /// (`platform.window(&WindowOp::Geometry)`, an AX read — the reader #263's symptom was about,
    /// not the `SCShareableContent` path `start_app`/`settle_window` poll), so the printed
    /// offsets are what actually happened. Those offsets are relative to sampling's own start;
    /// the enumeration series prints `t+` from `start_app`'s return, so the two blocks' timestamps
    /// share a format but not an origin.
    const EARLY_SAMPLE_INTERVAL: Duration = Duration::from_millis(10);
    const EARLY_SAMPLE_WINDOW: Duration = Duration::from_millis(300);

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

    /// One run's outcome: whether the adopted geometry matched a window the app's pid actually
    /// owns, whether the accessibility reader could resolve the adopted window at all (#263's
    /// actual symptom), the pid `start_app` reported — needed to tell a cold launch from a
    /// re-adoption of an already-running process (see the module doc's caveat) — and how long
    /// `start_app` itself took: its total end-to-end wall clock (process launch through
    /// LaunchServices, first-window creation, window discovery, and the settle — `MacosPlatform`
    /// is already constructed by the time this timer starts, so its own AX/Screen-Recording
    /// preflight is not part of it). Launch-dominated, not the settle's own increment.
    struct RunOutcome {
        matched: bool,
        snapshot_ok: bool,
        pid: Option<u32>,
        start_app: Duration,
    }

    /// Re-read the adopted window's geometry every [`EARLY_SAMPLE_INTERVAL`] for
    /// [`EARLY_SAMPLE_WINDOW`], printing each reading that differs from the one before it, with
    /// the offset from when sampling began.
    ///
    /// Only distinct readings are printed: a settled window produces one line, and an animating
    /// one produces the shape of its animation instead of thirty identical rows.
    ///
    /// The final "MATCHES / DIFFERS from the adopted geometry" line compares an AX-sourced
    /// reading against `adopted` (SCShareableContent-sourced) — a small, non-animation offset
    /// between those two readers is possible (see `axwindow.rs`'s `FALLBACK_TOLERANCE_PX`), so a
    /// `DIFFERS` here is corroborating evidence, not on its own proof that the window kept moving.
    fn print_early_samples(platform: &mut MacosPlatform, adopted: &WindowGeometry) {
        let started = Instant::now();
        let mut last: Option<WindowGeometry> = None;
        let mut samples = 0usize;
        println!(
            "  early samples (adopted: {}x{} @({},{})):",
            adopted.width, adopted.height, adopted.x, adopted.y
        );
        while started.elapsed() < EARLY_SAMPLE_WINDOW {
            match platform.window(&WindowOp::Geometry) {
                Ok(g) => {
                    samples += 1;
                    if last.as_ref() != Some(&g) {
                        println!(
                            "    t+{:>4}ms  {}x{} @({},{})",
                            started.elapsed().as_millis(),
                            g.width,
                            g.height,
                            g.x,
                            g.y
                        );
                        last = Some(g);
                    }
                }
                // Never fatal: a read that fails mid-animation is itself worth seeing, and this
                // is a probe, not a gate.
                Err(e) => println!(
                    "    t+{:>4}ms  read failed: {e}",
                    started.elapsed().as_millis()
                ),
            }
            std::thread::sleep(EARLY_SAMPLE_INTERVAL);
        }
        let settled_matches_adopted = last.as_ref() == Some(adopted);
        println!(
            "    {samples} sample(s); final reading {} the adopted geometry",
            if settled_matches_adopted {
                "MATCHES"
            } else {
                "DIFFERS from"
            }
        );
    }

    /// One `list_windows()` call, printed as one line labeled by real elapsed time since
    /// `series_start`, not by call index — [`print_early_samples`] runs between the first and
    /// later calls, so an index-derived label understated every call after it by that window's
    /// real duration.
    fn enumerate_once(
        platform: &mut MacosPlatform,
        run0: &str,
        adopted: &WindowGeometry,
        series_start: Instant,
        matched: &mut bool,
    ) -> Result<(), String> {
        let windows = platform
            .list_windows()
            .map_err(|e| format!("list_windows({run0}): {e}"))?;
        println!(
            "  t+{}ms: {} window(s)",
            series_start.elapsed().as_millis(),
            windows.len()
        );
        for w in &windows {
            let is_adopted = &w.geometry == adopted;
            *matched |= is_adopted;
            let tag = if is_adopted { "  == ADOPTED" } else { "" };
            println!("    {}{tag}", render_window(w));
        }
        Ok(())
    }

    /// One launch/enumerate/stop cycle. `Err` is a real breakage — the app would not launch, or
    /// its windows could not be enumerated at all — distinct from either field of `RunOutcome`
    /// being the unwelcome value, which is a finding, not a probe failure.
    fn probe_once(run0: &str, run_index: usize) -> Result<RunOutcome, String> {
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
            let start_app_began = Instant::now();
            let adopted = platform
                .start_app(&spec)
                .map_err(|e| format!("start_app({run0}): {e}"))?;
            let start_app_elapsed = start_app_began.elapsed();
            let pid = platform.app_pids().first().copied();
            println!("\n===== run {run_index} =====");
            println!(
                "pid: {}",
                pid.map_or_else(|| "?".to_string(), |p| p.to_string())
            );
            println!("start_app: {}ms", start_app_elapsed.as_millis());
            println!("adopted: {adopted:?}");

            // t+0 for the enumeration series below — captured at `start_app`'s return, not
            // derived from a loop index.
            let series_start = Instant::now();
            let mut matched = false;
            // Ahead of early sampling — see `ENUMERATIONS`'s doc for why this pass has to come
            // first.
            enumerate_once(platform, run0, &adopted, series_start, &mut matched)?;

            print_early_samples(platform, &adopted);

            for _ in 1..ENUMERATIONS {
                std::thread::sleep(ENUMERATION_GAP);
                enumerate_once(platform, run0, &adopted, series_start, &mut matched)?;
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
                deadline: AxDeadline::UNBOUNDED,
            };
            let snapshot_ok = match MacosA11y::new().snapshot(&ctx) {
                Ok(_) => {
                    println!("  snapshot: ok");
                    true
                }
                Err(e) => {
                    println!("  snapshot: FAILED: {e}");
                    false
                }
            };

            if !matched {
                println!("  VERDICT: adopted geometry matched NO window owned by this pid");
            }
            Ok(RunOutcome {
                matched,
                snapshot_ok,
                pid,
                start_app: start_app_elapsed,
            })
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
        let mut snapshot_failures = 0usize;
        let mut pids_seen = std::collections::HashSet::new();
        let mut start_app_durations = Vec::new();
        let mut failures = Vec::new();
        for run_index in 1..=runs {
            match probe_once(&run0, run_index) {
                Ok(outcome) => {
                    if !outcome.matched {
                        mismatches += 1;
                    }
                    if !outcome.snapshot_ok {
                        snapshot_failures += 1;
                    }
                    pids_seen.extend(outcome.pid);
                    start_app_durations.push(outcome.start_app);
                }
                Err(e) => failures.push(format!("run {run_index}: {e}")),
            }
        }

        if !failures.is_empty() {
            fail(failures.join("\n\n"));
        }
        // Both counts and the distinct-pid tally live in one line so a `0 of N` mismatch count
        // can never be read alone — see the module doc: a low distinct-pid count means most of
        // `runs` re-adopted the same already-running process rather than launching fresh.
        println!(
            "\n{mismatches} of {runs} run(s) adopted a geometry matching no listed window; \
             {snapshot_failures} of {runs} run(s) failed the accessibility snapshot; \
             {} distinct pid(s) seen across {runs} run(s)",
            pids_seen.len()
        );
        // `start_app`'s total end-to-end wall clock, launch-dominated — not the settle loop's
        // own increment; that would need an A/B sweep against a build without it.
        // `start_app_durations` has one entry per successful run — the `fail()` above exits
        // before reaching here if any run errored — so the division below never sees a
        // zero-length vec.
        let min = start_app_durations
            .iter()
            .min()
            .copied()
            .unwrap_or_default();
        let max = start_app_durations
            .iter()
            .max()
            .copied()
            .unwrap_or_default();
        let mean_ms = start_app_durations
            .iter()
            .map(Duration::as_secs_f64)
            .sum::<f64>()
            * 1000.0
            / start_app_durations.len() as f64;
        println!(
            "start_app latency: min {}ms, max {}ms, mean {mean_ms:.1}ms across {} run(s)",
            min.as_millis(),
            max.as_millis(),
            start_app_durations.len()
        );
        println!("WINDOW_ADOPT_PROBE_PASS");
        std::process::exit(0);
    }
}
