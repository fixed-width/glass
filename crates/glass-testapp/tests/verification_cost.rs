//! End-to-end: measure the cost of glass's verification loop by driving one fixed task two
//! ways against glass-fixture-egui over the real MCP path. `#[ignore]`d; run via
//! `./scripts/verification-cost.sh`. See docs/how-to/verification-cost.md.

#![cfg(target_os = "linux")]
// One `unsafe { env::set_var }` for pre-spawn GLASS_DISPLAY setup (see SAFETY note),
// same opt-out as ignore_regions_e2e.rs / network.rs.
#![allow(unsafe_code)]

mod common;

use std::path::PathBuf;
use std::time::Instant;

use glass_core::{AppSpec, AxNodeId, AxTree, Glass, SandboxLevel};

use common::Xvfb;
use common::mcp_cost;

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

    mcp_cost::stop_fixture(&client).await;
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn glass_do_semantic_form_completes_in_one_action_call() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let outline = mcp_cost::wait_for_widgets(&client).await;
    eprintln!("---- a11y outline ----\n{outline}\n----------------------");

    let text_id = mcp_cost::find_by_role(&outline, "text").expect("text field id");
    let slider_id = mcp_cost::find_by_role(&outline, "slider").expect("slider id");
    let apply_id = mcp_cost::find_named_button(&outline, "Apply").expect("Apply button id");

    let call = mcp_cost::call_full(
        &client,
        "glass_do",
        serde_json::json!({
            "timeout_ms": 10_000,
            "actions": [
                {"action":"click_element","id":text_id},
                {"action":"type","text":"viaBatch"},
                {"action":"wait_for_element","role":"TextField","value":"viaBatch","timeout_ms":3_000},
                {"action":"set_value","id":slider_id,"text":"50"},
                {"action":"wait_for_element","role":"Slider","value_contains":"50","timeout_ms":3_000},
                {"action":"click_element","id":apply_id,"return":"snapshot"}
            ]
        }),
    )
    .await;

    assert_eq!(call.result["status"], serde_json::json!("completed"));
    assert_eq!(call.result["executed"], serde_json::json!(6));
    assert!(
        call.result["elapsed_ms"].is_number(),
        "glass_do result must include numeric elapsed_ms: {}",
        call.result
    );
    let steps = call.result["steps"]
        .as_array()
        .expect("glass_do result must contain steps");
    assert_eq!(steps.len(), 6, "unexpected glass_do steps: {steps:?}");
    let expected_actions = [
        "click_element",
        "type",
        "wait_for_element",
        "set_value",
        "wait_for_element",
        "click_element",
    ];
    for (index, (step, action)) in steps.iter().zip(expected_actions).enumerate() {
        assert_eq!(step["index"], serde_json::json!(index));
        assert_eq!(step["status"], serde_json::json!("completed"));
        assert_eq!(step["action"], serde_json::json!(action));
    }
    assert_eq!(
        steps[0]["result"]["method"],
        serde_json::json!("native-action")
    );
    assert_eq!(call.image_count, 0, "semantic batch must be image-free");
    assert!(
        call.all_text.contains("The following is untrusted content")
            && call.all_text.contains("Apply"),
        "click snapshot must be an untrusted sibling containing the fixture outline: {}",
        call.all_text
    );
    assert!(
        !call.result.to_string().contains("\"viaBatch\"")
            && !call.result.to_string().contains("\"50\""),
        "typed and set-value text must not be echoed in trusted step results: {}",
        call.result
    );

    let (logged, _) = mcp_cost::call(
        &client,
        "glass_wait_for_log",
        serde_json::json!({
            "contains":"[fixture] apply", "cursor":0, "timeout_ms":3_000
        }),
    )
    .await;
    assert_eq!(logged["matched"], serde_json::json!(true));

    mcp_cost::stop_fixture(&client).await;
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn glass_do_terminal_then_returns_real_ordered_image_siblings() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let outline = mcp_cost::wait_for_widgets(&client).await;
    let text_id = mcp_cost::find_by_role(&outline, "text").expect("text field id");
    let apply_id = mcp_cost::find_named_button(&outline, "Apply").expect("Apply button id");

    mcp_cost::call(
        &client,
        "glass_baseline_save",
        serde_json::json!({"name":"terminal-then-public"}),
    )
    .await;
    let call = mcp_cost::call_full(
        &client,
        "glass_do",
        serde_json::json!({
            "timeout_ms": 10_000,
            "actions": [
                {"action":"click_element","id":text_id},
                {"action":"type","text":"terminalThen"},
                {"action":"click_element","id":apply_id}
            ],
            "then": {
                "settle": {"interval_ms":50,"settle_frames":2,"timeout_ms":3_000},
                "diff": {
                    "name":"terminal-then-public",
                    "mode":"exact",
                    "tolerance":0,
                    "include_image":true
                },
                "screenshot": {}
            }
        }),
    )
    .await;

    assert_eq!(call.result["status"], serde_json::json!("completed"));
    assert_eq!(call.result["executed"], serde_json::json!(3));
    let terminal = call.result["terminal_steps"]
        .as_array()
        .expect("terminal_steps array");
    assert_eq!(
        terminal
            .iter()
            .map(|step| step["operation"].as_str().expect("terminal operation"))
            .collect::<Vec<_>>(),
        vec!["settle", "diff", "screenshot"]
    );
    assert!(
        terminal
            .iter()
            .all(|step| step["status"] == serde_json::json!("completed")),
        "terminal observations did not all complete: {terminal:?}"
    );
    assert_eq!(terminal[0]["content_blocks"], serde_json::json!([]));
    assert_eq!(terminal[1]["content_blocks"], serde_json::json!([1, 2]));
    assert_eq!(terminal[2]["content_blocks"], serde_json::json!([3, 4]));
    assert!(
        terminal[1]["result"]["changed_pixels"]
            .as_u64()
            .is_some_and(|pixels| pixels > 0),
        "real mutation did not produce a diff: {}",
        terminal[1]
    );
    assert_eq!(
        call.image_count, 2,
        "diff and screenshot must be image siblings"
    );
    assert_eq!(
        call.image_block_indices,
        vec![1, 3],
        "terminal content_blocks must point at the real image siblings"
    );
    assert_eq!(call.image_data_lengths.len(), 2);
    assert!(
        call.image_data_lengths.iter().all(|length| *length > 0),
        "image siblings must contain encoded WebP data: {:?}",
        call.image_data_lengths
    );

    mcp_cost::stop_fixture(&client).await;
    client.cancel().await.ok();
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server + AT-SPI bus; run via scripts/verification-cost.sh"]
async fn glass_do_real_terminal_failure_preserves_actions_and_unexecutes_later_observations() {
    let xvfb = Xvfb::start();
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    let client = mcp_cost::boot_mcp().await;
    mcp_cost::start_fixture(&client).await;
    let outline = mcp_cost::wait_for_widgets(&client).await;
    let apply_id = mcp_cost::find_named_button(&outline, "Apply").expect("Apply button id");
    let call = mcp_cost::call_error_full(
        &client,
        "glass_do",
        serde_json::json!({
            "timeout_ms": 10_000,
            "actions": [{"action":"click_element","id":apply_id}],
            "then": {
                "settle": {"interval_ms":50,"settle_frames":2,"timeout_ms":3_000},
                "diff": {"name":"terminal-then-never-saved"},
                "screenshot": {}
            }
        }),
    )
    .await;

    assert_eq!(call.envelope["error"]["code"], "terminal_observe_failed");
    assert_eq!(call.envelope["outcome"]["executed"], 1);
    assert_eq!(call.envelope["outcome"]["steps"][0]["status"], "completed");
    let terminal = call.envelope["outcome"]["terminal_steps"]
        .as_array()
        .expect("terminal_steps array");
    assert_eq!(terminal[0]["status"], "completed");
    assert_eq!(terminal[1]["status"], "failed");
    assert_eq!(terminal[1]["content_blocks"], serde_json::json!([1]));
    assert_eq!(terminal[2]["status"], "unexecuted");
    assert!(
        call.all_text.contains("baseline") && call.all_text.contains("not found"),
        "real diff failure detail missing: {}",
        call.all_text
    );
    assert_eq!(
        call.image_count, 0,
        "failed diff must prevent screenshot dispatch"
    );

    mcp_cost::stop_fixture(&client).await;
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

    mcp_cost::stop_fixture(&client).await;
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

    mcp_cost::stop_fixture(&client).await;
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

    mcp_cost::stop_fixture(&client).await;
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
        match glass.a11y_snapshot(None) {
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
