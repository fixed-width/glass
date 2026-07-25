//! Role-histogram PROBE for the iOS half of the accessibility role-parity work — not a
//! pass/fail assertion test. Launches whatever apps `GLASS_A11Y_PROBE_APPS` names (a
//! comma-separated list of bundle ids or `.app` paths — exactly what `AppSpec::run`'s first
//! element accepts on this backend), snapshots each through `idb` with the node cap lifted,
//! and prints `glass_core::role_histogram`: every AX role string the Simulator actually
//! reported, unmapped ([`glass_core::AxRole::Other`]) tokens first. The project's rule is
//! probe first, map second — a `Gap` cell in `glass_core::role_support::ROLE_SUPPORT` may only
//! get a match arm for a token that showed up in output like this — so this file's job is to
//! produce that evidence.
//!
//! It asserts exactly one thing *about* the evidence: a token glass does map must not come
//! back `AxRole::Other`, which would mean the reader stopped feeding `axmap::ax_role` what it
//! reads (see [`MAPPED_TOKENS`]). Otherwise it fails a run only when an app could not be
//! launched or snapshotted at all — a real breakage, distinct from an app simply exposing
//! something unexpected. Which role a token *should* map to is never asserted here.
//!
//! Ignored by default; run on a macOS host with a booted Simulator and `idb_companion`
//! available:
//!
//! ```sh
//! GLASS_A11Y_PROBE_APPS=com.apple.Preferences \
//!   cargo test -p glass-ios --test role_probe -- --ignored --nocapture
//! ```
//!
//! With `GLASS_A11Y_PROBE_APPS` unset (or set but empty) it prints what to set and passes
//! without probing, so a run that did not ask for this never fails because of it.

use std::time::Duration;

use glass_core::accessibility::{AxContext, WalkLimits};
use glass_core::Accessibility; // the trait must be in scope to call `snapshot` on the boxed reader
use glass_core::{role_histogram, AppSpec, AxRole, AxTree, Platform, SandboxLevel};
use glass_ios::{IosPlatform, SimulatorRegistry};

/// Comma-separated bundle ids or `.app` paths to probe, e.g. `com.apple.Preferences`. Unset
/// (or set but empty) skips the probe rather than failing a run that never asked for it.
const PROBE_APPS_VAR: &str = "GLASS_A11Y_PROBE_APPS";

/// How long to wait after `start_app` before snapshotting — UIKit keeps building the view
/// hierarchy for a beat after the app is up, and a tree read too early is missing the content
/// the probe exists to see. Matches the settle the sibling on-box tests use.
///
/// Overridable with `GLASS_A11Y_PROBE_SETTLE_MS`: a stock app that loads its content
/// asynchronously (a contact list, a document browser) can still be showing an empty shell at
/// this default, and an empty shell is evidence of nothing.
const STARTUP_SETTLE: Duration = Duration::from_millis(1500);

/// [`STARTUP_SETTLE`], or the `GLASS_A11Y_PROBE_SETTLE_MS` override when it parses.
fn startup_settle() -> Duration {
    std::env::var("GLASS_A11Y_PROBE_SETTLE_MS")
        .ok()
        .and_then(|v| v.trim().parse().ok())
        .map_or(STARTUP_SETTLE, Duration::from_millis)
}

/// AX role tokens glass maps to a role, and that a probed app has actually been seen to
/// report. A histogram bucket carrying one of these must not come back [`AxRole::Other`]: the
/// token reached the reader, so a mapped role is the only correct outcome, and `Other` would
/// mean the plumbing between the reader and `axmap::ax_role` broke. A token that simply does
/// not appear in a given app asserts nothing — apps differ, and an absent token is not a
/// regression.
const MAPPED_TOKENS: &[&str] = &[
    "AXButton",
    "AXStaticText",
    "AXTextField",
    "AXImage",
    "AXCell",
    "AXNavigationBar",
    "AXApplication",
    "AXWindow",
];

/// Every [`MAPPED_TOKENS`] bucket in `tree` that came back [`AxRole::Other`], described — the
/// one thing a histogram can check without becoming brittle about which app exposes what.
/// Everything else the probe prints is evidence for a human, not a pass/fail claim.
fn mapped_token_violations(label: &str, tree: &AxTree) -> Vec<String> {
    role_histogram(tree)
        .into_iter()
        .filter(|e| e.role == AxRole::Other && MAPPED_TOKENS.contains(&e.raw_role.as_str()))
        .map(|e| {
            format!(
                "{label}: {} node(s) reported token {:?} as Other, but glass maps that token \
                 — the reader is not feeding ax_role what it reads",
                e.count, e.raw_role
            )
        })
        .collect()
}

/// Print `role_histogram(tree)` as one line per `(token, role)` bucket — unmapped
/// ([`AxRole::Other`]) buckets first, which is already the histogram's own sort order, so the
/// tokens most worth a human's attention are the first thing printed.
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

#[test]
#[ignore = "on-box only: needs a macOS host with a booted iOS Simulator + idb_companion, and \
            GLASS_A11Y_PROBE_APPS naming bundle ids or .app paths"]
fn role_histogram_probe() {
    let raw = std::env::var(PROBE_APPS_VAR).unwrap_or_default();
    let targets: Vec<&str> = raw
        .split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .collect();
    if targets.is_empty() {
        println!(
            "skipped: set {PROBE_APPS_VAR} to a comma-separated list of bundle ids or .app \
             paths to probe, e.g. {PROBE_APPS_VAR}=com.apple.Preferences"
        );
        return;
    }

    let registry = SimulatorRegistry::new();
    let mut platform =
        IosPlatform::from_env(&registry).expect("from_env: resolve/boot a Simulator");
    let mut violations = Vec::new();

    for target in targets {
        let spec = AppSpec {
            build: None,
            run: vec![target.to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 30_000,
            sandbox: SandboxLevel::Off,
            a11y: true,
        };
        let window = platform
            .start_app(&spec)
            .unwrap_or_else(|e| panic!("start_app({target}): {e}"));
        std::thread::sleep(startup_settle());

        let mut a11y = platform
            .accessibility()
            .expect("accessibility(): connect an idb client")
            .expect("companion present on this on-box run, so a reader is available");
        let ctx = AxContext {
            pids: vec![],
            window,
            window_handle: None,
            a11y_bus_addr: None,
            // The node cap lifted, so a big app's tree is never truncated mid-probe. Depth
            // and per-level sibling rails keep their generous structural defaults regardless.
            limits: WalkLimits::from_max_nodes(Some(0)),
        };
        let mut tree = a11y
            .snapshot(&ctx)
            .unwrap_or_else(|e| panic!("snapshot({target}): {e}"));
        tree.assign_ids();
        print_role_histogram(target, &tree);
        violations.extend(mapped_token_violations(target, &tree));
    }

    platform.stop_app().expect("stop_app");
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
