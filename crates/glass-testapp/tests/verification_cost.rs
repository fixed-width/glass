//! End-to-end: measure the cost of glass's verification loop by driving one fixed task two
//! ways against glass-fixture-egui over the real MCP path. `#[ignore]`d; run via
//! `./scripts/verification-cost.sh`. See docs/how-to/verification-cost.md.

// One `unsafe { env::set_var }` for pre-spawn GLASS_DISPLAY setup (see SAFETY note),
// same opt-out as ignore_regions_e2e.rs / network.rs.
#![allow(unsafe_code)]

mod common;

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
