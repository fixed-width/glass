//! End-to-end: measure the cost of glass's verification loop by driving one fixed task two
//! ways against glass-fixture-egui over the real MCP path. `#[ignore]`d; run via
//! `./scripts/verification-cost.sh`. See docs/how-to/verification-cost.md.

// One `unsafe { env::set_var }` for pre-spawn GLASS_DISPLAY setup (see SAFETY note),
// same opt-out as ignore_regions_e2e.rs / network.rs.
#![allow(unsafe_code)]

mod common;

use std::path::PathBuf;
use std::time::Instant;

use glass_core::{AppSpec, AxNodeId, AxTree, Glass, SandboxLevel};

use common::mcp_cost;
use common::Xvfb;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn probe_fixture_a11y_tree_is_reachable() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let outline = mcp_cost::wait_for_widgets(&client).await;
    eprintln!("---- a11y outline ----\n{outline}\n----------------------");

    assert!(
        mcp_cost::find_named_button(&outline, "Apply").is_some(),
        "Apply button not addressable in the a11y tree:\n{outline}"
    );
    assert!(
        mcp_cost::find_by_role(&outline, "slider").is_some(),
        "slider not addressable in the a11y tree:\n{outline}"
    );
    assert!(
        mcp_cost::find_by_role(&outline, "text").is_some(),
        "editable text field not addressable in the a11y tree:\n{outline}"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn arm_a_is_text_only_and_completes() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let report = mcp_cost::run_arm_a(&client).await;
    eprintln!("{report}");

    assert!(
        report.round_trips >= 4,
        "arm A should take several steps: {report}"
    );
    assert_eq!(report.image_count, 0, "arm A must be image-free: {report}");
    assert_eq!(
        report.image_b64_bytes, 0,
        "arm A must carry no image bytes: {report}"
    );

    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn arm_b_uses_images_and_completes() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let report = mcp_cost::run_arm_b(&client).await;
    eprintln!("{report}");

    assert!(
        report.image_count >= 3,
        "arm B must screenshot repeatedly: {report}"
    );
    assert!(
        !report.image_dims.is_empty(),
        "arm B must record image dims: {report}"
    );

    client.cancel().await.ok();
}

/// The headline result: drive one fixed task both ways end to end, assert arm A's
/// determinism and the cross-arm invariants, and write the JSON artifact both arms feed.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn verification_cost_semantic_beats_screenshot() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let (a, b) = mcp_cost::run_verification_cost(&client).await;

    eprintln!("\n{a}\n{b}\n");
    // Exact, not just directional: these are the measured constants the published doc table
    // (docs/how-to/verification-cost.md) quotes. Pinning them here means a future change that
    // alters the task and shifts these primitives makes this test fail — which is exactly the
    // signal that the doc's table needs updating too, rather than letting the numbers silently
    // drift out of sync with what's published.
    assert_eq!(
        a.round_trips, 6,
        "arm A round-trips drifted from the published number"
    );
    assert_eq!(a.image_count, 0, "arm A must be image-free");
    assert_eq!(
        b.round_trips, 8,
        "arm B round-trips drifted from the published number"
    );
    assert_eq!(
        b.image_count, 4,
        "arm B image count drifted from the published number"
    );
    assert!(
        b.image_dims.iter().all(|&d| d == (400, 300)),
        "arm B image dims drifted from the published 400x300 fixture size: {:?}",
        b.image_dims
    );
    assert!(a.text_bytes > 0 && b.text_bytes > 0);

    client.cancel().await.ok();
}

/// Sync counterpart to `mcp_cost::start_fixture`: launches the same fixture app directly
/// against a `glass_core::Glass` session instead of over the MCP wire. The wire has no route
/// to the property this file's cost-and-integrity test needs — `glass_a11y_snapshot` always
/// answers with `render_compact`'s output (see `glass_mcp::tools::a11y_snapshot`), never the
/// uncompacted `AxTree::to_outline` render — so that test drives `Glass` directly to see both.
/// The caller must point `GLASS_DISPLAY` at a live Xvfb before calling this, the same as the
/// wire-based tests in this suite: `start_on("x11", ...)` attaches to whatever display
/// `GLASS_DISPLAY` names rather than spawning its own.
fn start_fixture_sync(glass: &mut Glass) {
    let (build, run, cwd) = mcp_cost::fixture_run_spec();
    let spec = AppSpec {
        build: Some(build),
        run: vec![run],
        cwd: Some(PathBuf::from(cwd)),
        env: vec![],
        window_hint: None,
        timeout_ms: 120_000, // first egui build is slow, same as start_fixture
        sandbox: SandboxLevel::Off,
        a11y: true,
    };
    glass
        .start_on("x11", &spec)
        .unwrap_or_else(|e| panic!("start_on(x11) failed: {e}"));
}

/// Sync counterpart to `mcp_cost::wait_for_widgets`: polls `Glass::a11y_snapshot` directly
/// rather than an MCP `Peer`, for the same reason as `start_fixture_sync`. Same retry
/// reasoning as `wait_for_widgets`/`a11y_outline`: the launched app's toolkit can transiently
/// error (its AT-SPI subtree not registered yet) or answer with a placeholder root before its
/// widgets are filled in, so both are retried up to the combined budget.
fn wait_for_widgets_sync(glass: &mut Glass) -> AxTree {
    let deadline =
        Instant::now() + mcp_cost::A11Y_SETTLE_TIMEOUT + mcp_cost::WIDGETS_SETTLE_TIMEOUT;
    loop {
        match glass.a11y_snapshot() {
            Ok(tree) => {
                let outline = tree.to_outline();
                let ready = mcp_cost::find_named_button(&outline, "Apply").is_some()
                    && mcp_cost::find_by_role(&outline, "slider").is_some()
                    && mcp_cost::find_by_role(&outline, "text").is_some();
                if ready {
                    return tree;
                }
                if Instant::now() >= deadline {
                    panic!(
                        "wait_for_widgets_sync: timed out waiting for the fixture's widgets; \
                         last-seen outline:\n{outline}"
                    );
                }
            }
            Err(e) if Instant::now() < deadline => {
                let _ = e; // transient during app startup; keep polling
            }
            Err(e) => panic!("a11y_snapshot errored: {e}"),
        }
        std::thread::sleep(mcp_cost::A11Y_POLL_INTERVAL);
    }
}

/// Every `#<n>` id at the start of an outline's lines (see `mcp_cost::find_by_role`'s doc for
/// the line shape) — `split_whitespace` + `strip_prefix('#')`, no regex dependency.
fn ids_in(outline: &str) -> Vec<AxNodeId> {
    let mut ids = Vec::new();
    for line in outline.lines() {
        let Some(tok) = line.split_whitespace().next() else {
            continue;
        };
        let Some(digits) = tok.strip_prefix('#') else {
            continue;
        };
        let Ok(id) = digits.parse::<u32>() else {
            continue;
        };
        ids.push(AxNodeId(id));
    }
    ids
}

/// `render_compact` must remove lines from a real tree (the direction of the change; the exact
/// ratio is fixture-dependent and deliberately not asserted), and every id it keeps must still
/// resolve in the full tree — the property that keeps an elided element addressable by
/// `glass_click_element` / `glass_set_value` after compaction: ids are assigned over the full
/// tree and compaction must never invent or renumber one.
#[test]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
fn compact_outline_is_smaller_and_every_id_still_resolves() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let mut glass = glass_mcp::boot(None);
    start_fixture_sync(&mut glass);
    let tree = wait_for_widgets_sync(&mut glass);

    let full = tree.to_outline();
    let compact = glass_core::outline::render_compact(&tree);

    assert!(
        compact.lines().count() < full.lines().count(),
        "compaction must remove lines (full {} / compact {})",
        full.lines().count(),
        compact.lines().count()
    );
    for id in ids_in(&compact) {
        assert!(
            tree.find(id).is_some(),
            "#{} appears in the compact outline but resolves to nothing — compaction must \
             never invent or renumber ids",
            id.0
        );
    }

    let _ = glass.stop();
}
