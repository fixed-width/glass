#![cfg(unix)]
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

use glass_core::Accessibility; // the trait must be in scope to call `snapshot` on the boxed reader
use glass_core::accessibility::{AxContext, WalkLimits};
use glass_core::{
    AppSpec, AxRole, AxTree, DescriptionSourcing, Platform, SandboxLevel,
    description_census_report, role_histogram,
};
use glass_ios::{IosPlatform, SimulatorRegistry};

/// Comma-separated bundle ids or `.app` paths to probe, e.g. `com.apple.Preferences`. Unset
/// (or set but empty) skips the probe rather than failing a run that never asked for it.
const PROBE_APPS_VAR: &str = "GLASS_A11Y_PROBE_APPS";

/// How long to wait after `start_app` before snapshotting — UIKit keeps building the view
/// hierarchy for a beat after the app is up, and a tree read too early is missing the content
/// the probe exists to see. A little longer than the sibling on-box tests settle for, on
/// purpose: they only need the app up, while a probe needs the screen fully populated or its
/// evidence is a half-drawn tree.
///
/// Overridable with [`SETTLE_MS_VAR`]: a stock app that loads its content asynchronously (a
/// contact list, a document browser) can still be showing an empty shell at this default, and
/// an empty shell is evidence of nothing.
const STARTUP_SETTLE: Duration = Duration::from_millis(1500);

/// Overrides [`STARTUP_SETTLE`], in whole milliseconds.
const SETTLE_MS_VAR: &str = "GLASS_A11Y_PROBE_SETTLE_MS";

/// The smallest tree this probe accepts as evidence. A snapshot taken before the app finished
/// drawing, or of the wrong window, reports a near-empty shell — which yields no histogram
/// buckets, no violations, and a green run that proves nothing. The floor is deliberately low:
/// the synthetic window root plus the application element account for two nodes on their own,
/// and a legitimate iOS screen can still be small (a document browser's empty state is six).
/// So this catches "we could not look", not "this app is sparse".
const MIN_EVIDENCE_NODES: usize = 4;

/// [`STARTUP_SETTLE`], or the [`SETTLE_MS_VAR`] override when it is set.
///
/// # Panics
///
/// Panics when the variable is set to a non-blank value that is not a whole number of
/// milliseconds; set-but-blank reads as unset. Falling back to the default on a typo would hand
/// the operator the very empty shell they set the knob to avoid, without saying so.
fn startup_settle() -> Duration {
    let Some(raw) = std::env::var_os(SETTLE_MS_VAR) else {
        return STARTUP_SETTLE;
    };
    // Set but blank reads as unset, the same way an empty PROBE_APPS_VAR does — a shell that
    // exports an unfilled variable has asked for nothing, not for a broken value.
    if raw.to_str().is_some_and(|v| v.trim().is_empty()) {
        return STARTUP_SETTLE;
    }
    let ms: u64 = raw
        .to_str()
        .and_then(|v| v.trim().parse().ok())
        .unwrap_or_else(|| {
            panic!(
                "{SETTLE_MS_VAR}={raw:?} is not a whole number of milliseconds — set it to \
                 e.g. 4000, or unset it for the {}ms default",
                STARTUP_SETTLE.as_millis()
            )
        });
    Duration::from_millis(ms)
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
    "AXGroup",
    "AXHeading",
];

// `AXGenericElement` is deliberately absent: it carries no role, so `Other` with the token
// preserved is the correct outcome, and a probe run that reports it is reporting the truth.

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

/// A description of `tree` being too small to be evidence of anything, or `None` when it
/// clears [`MIN_EVIDENCE_NODES`]. Returned for collection alongside the mapping violations
/// rather than panicking mid-loop, so one bad target still lets the rest print their evidence.
fn thin_tree_violation(label: &str, tree: &AxTree) -> Option<String> {
    (tree.count < MIN_EVIDENCE_NODES).then(|| {
        format!(
            "{label}: the tree has only {} node(s), under the {MIN_EVIDENCE_NODES} a real \
             screen clears — usually the app had not finished drawing, the wrong window was \
             foreground, or a system alert was covering the screen. Whatever this run printed \
             is not evidence; retry, raising {SETTLE_MS_VAR} if the app is slow to populate.",
            tree.count
        )
    })
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
    let mut described = 0usize;

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
        print!(
            "{}",
            description_census_report(target, &tree, DescriptionSourcing::Sourced)
        );
        described += glass_core::description_census(&tree).described();
        violations.extend(thin_tree_violation(target, &tree));
        violations.extend(mapped_token_violations(target, &tree));

        // Inside the loop: `start_app` overwrites the platform's stored running app without
        // terminating the previous one, so stopping only after the loop would leave every
        // earlier target running on the Simulator.
        platform.stop_app().expect("stop_app");
    }

    // Same rationale as the Android role-histogram probes' role_probe.rs: a silent zero is a
    // note here, not a failure, since the app list is the caller's.
    if described == 0 {
        println!(
            "\nNOTE: no app in this run reported a single described node — either these apps \
             give every element one label, or the reader stopped sourcing it (see the \
             `description` binding in axmap::map_node)"
        );
    }
    assert!(violations.is_empty(), "{}", violations.join("\n"));
}
