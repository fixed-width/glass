//! Role-histogram PROBE for the macOS half of the accessibility role-parity work — not a
//! pass/fail assertion test. Launches whatever apps `GLASS_A11Y_PROBE_APPS` names (a
//! comma-separated list of launch commands — see this file's `macos_main::PROBE_APPS_VAR`
//! doc comment), snapshots each through `glass-a11y-macos` with the node cap lifted, and
//! prints `glass_core::role_histogram`: every native AX role string the app
//! actually emitted, unmapped ([`glass_core::AxRole::Other`]) tokens first. The project's rule
//! is probe first, map second — a `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` may
//! only get a match arm for a token that showed up in output like this — so this file's job is
//! to produce that evidence. It asserts exactly one thing *about* the evidence: a token glass
//! does map must not come back `AxRole::Other`, which would mean the reader stopped feeding
//! `map_role` what it reads (see `macos_main::MAPPED_TOKENS`). Otherwise it fails a run only
//! when an app could not be launched or snapshotted at all — a real breakage, distinct from an
//! app simply exposing something unexpected. Which role a token *should* map to is never
//! asserted here.
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
    use std::time::{Duration, Instant};

    use glass_a11y_macos::MacosA11y;
    use glass_core::{
        Accessibility, AppSpec, AxContext, AxRole, AxTree, DescriptionSourcing, Platform,
        SandboxLevel, WalkLimits, description_census, description_census_report, role_histogram,
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

    /// AX role tokens glass maps to a role, and that a probed app has actually been seen to
    /// emit. A histogram bucket carrying one of these must not come back [`AxRole::Other`]: the
    /// token reached the reader, so a mapped role is the only correct outcome, and `Other` would
    /// mean the plumbing between the reader and `mapping::map_role` broke. A token that simply
    /// does not appear in a given app asserts nothing — apps differ, and an absent token is not
    /// a regression.
    ///
    /// `"AXRow/AXOutlineRow"` is the `raw_role` the reader builds for a row whose `AXSubrole`
    /// says it is an outline row (`role`/`subrole`, joined) — the one node shape where reading
    /// the subrole changes the mapped role, so it is exactly the wiring most worth pinning.
    const MAPPED_TOKENS: &[&str] = &[
        "AXOutline",
        "AXScrollArea",
        "AXSplitGroup",
        "AXSplitter",
        "AXHeading",
        "AXMenuButton",
        "AXRow/AXOutlineRow",
    ];

    /// Every [`MAPPED_TOKENS`] bucket in `tree` that came back [`AxRole::Other`], described —
    /// the one thing a histogram can check without becoming brittle about which app exposes
    /// what. Everything else the probe prints is evidence for a human, not a pass/fail claim.
    fn mapped_token_violations(label: &str, tree: &AxTree) -> Vec<String> {
        role_histogram(tree)
            .into_iter()
            .filter(|e| e.role == AxRole::Other && MAPPED_TOKENS.contains(&e.raw_role.as_str()))
            .map(|e| {
                format!(
                    "{label}: {} node(s) reported token {:?} as Other, but glass maps that \
                     token — the reader is not feeding map_role what it reads",
                    e.count, e.raw_role
                )
            })
            .collect()
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

    /// Extra snapshots [`print_snapshot_cost`] takes of each probed app. Every sample is printed
    /// beside the mean, so a single slow outlier is visible rather than hidden in the average.
    const COST_REPEATS: usize = 10;

    /// Re-snapshot the already-launched app [`COST_REPEATS`] times and print each sample's
    /// wall-clock.
    ///
    /// Repeating inside one launch leaves app *startup* out of the number, but each sample is
    /// still a whole `snapshot` call — the Accessibility-grant gate and the `AXWindows` resolve,
    /// and then the walk. So the absolute figure is not walk time, and only the *difference*
    /// between two runs of this block is attributable to a change in what the walk reads per node
    /// (glass's `description`). A per-node figure is that difference over the node count in the
    /// histogram header above, never the mean over it.
    ///
    /// Never asserted and never fatal: a latency bound would flake on a loaded box, and a snapshot
    /// that fails here must not cost this app its role-parity check or the later apps their runs.
    /// Twin of `render_snapshot_cost` in `glass-windows/tests/onbox.rs` — keep the two in step.
    fn print_snapshot_cost(a11y: &mut MacosA11y, ctx: &AxContext) {
        let mut samples = Vec::with_capacity(COST_REPEATS);
        for repeat in 0..COST_REPEATS {
            let started = Instant::now();
            match a11y.snapshot(ctx) {
                Ok(tree) => {
                    // Inside the timed window, so no future laziness could move walk work out
                    // from under the timer.
                    std::hint::black_box(&tree);
                    samples.push(started.elapsed().as_secs_f64() * 1000.0);
                }
                // The samples taken so far go out with the error: "the first one failed" and
                // "they grew from 40ms to 900ms and then failed" are different findings.
                Err(e) => {
                    println!(
                        "  snapshot cost: repeat {repeat} of {COST_REPEATS} FAILED: {e}\n  \
                         samples before it: {}",
                        render_cost_samples(&samples)
                    );
                    return;
                }
            }
        }
        let mean = samples.iter().sum::<f64>() / samples.len() as f64;
        println!(
            "  snapshot cost over {COST_REPEATS} snapshots (mean {mean:.0}ms): {}",
            render_cost_samples(&samples)
        );
    }

    /// Cost samples as `19ms, 20ms, …`, or `(none)` when the first snapshot failed.
    fn render_cost_samples(samples: &[f64]) -> String {
        if samples.is_empty() {
            return "(none)".to_string();
        }
        samples
            .iter()
            .map(|ms| format!("{ms:.0}ms"))
            .collect::<Vec<_>>()
            .join(", ")
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
    /// it. Returns `Err` when the app could not be launched, a snapshot could not be taken at
    /// all, or a token glass maps came back unmapped ([`mapped_token_violations`]) — never
    /// merely because an app exposed something unexpected (see this file's module doc). Beyond
    /// that one check the histogram's contents are never asserted; reading the printed output
    /// to decide which `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` a real native
    /// token now justifies filling is the human's job.
    /// `Ok` carries how many of this app's nodes the reader gave a description — an observation
    /// per app (an app with no `AXHelp` is a legitimate zero), summed by [`run`] so a whole run
    /// of zeros can be challenged there.
    fn probe_one(run0: &str) -> Result<usize, String> {
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
            // `Sourced`: this reader reads `AXHelp` (else `AXDescription` where `AXTitle` took
            // the name), so a zero here is a fact about the app — modulo a read that failed,
            // which `read_label` logs rather than counting.
            print!(
                "{}",
                description_census_report(run0, &tree, DescriptionSourcing::Sourced)
            );
            print_snapshot_cost(&mut a11y, &ctx);
            // Checked after the histogram is printed, so the evidence that explains a failure
            // is already in the output when the failure is reported.
            let violations = mapped_token_violations(run0, &tree);
            if violations.is_empty() {
                Ok(description_census(&tree).described())
            } else {
                Err(violations.join("\n"))
            }
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
        let mut described = 0usize;
        for run0 in targets {
            match probe_one(run0) {
                Ok(n) => described += n,
                Err(e) => failures.push(e),
            }
        }

        if !failures.is_empty() {
            fail(failures.join("\n\n"));
        }
        // Not a failure: which apps carry `AXHelp` is up to the apps, and the caller chooses them
        // (System Settings really does report none). But the census now prints without a caveat,
        // so a reader regressed to `description: None` would print a plausible zero for every app
        // and this probe would pass silently. Say it once, at the end, where a whole-run zero is
        // visible as one claim rather than N app facts.
        if described == 0 {
            println!(
                "\nNOTE: no app in this run reported a single described node — either these apps \
                 carry no AXHelp, or the reader stopped sourcing it (see read_label in \
                 glass-a11y-macos)"
            );
        }
        println!("\nROLE_PROBE_PASS");
        std::process::exit(0);
    }
}
