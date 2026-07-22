//! Shared body for the X11/Wayland "ignore regions" end-to-end tests
//! (`ignore_regions_e2e.rs` / `wayland_ignore_regions_e2e.rs`).
//!
//! Everything glass ships for `ignore` regions elsewhere is unit-tested against a fake
//! `Platform` or by calling glass-core's library API directly — neither path exercises the
//! MCP tool-argument parsing, the window-relative coordinate mapping, or the JSON schema the
//! rmcp server actually advertises. This drives a real `glass-mcp` server in-process over HTTP
//! (like `network.rs`'s roundtrip) against `glass-testapp --blink`, a small rectangle repainted
//! on a fixed schedule that stands in for a perpetually animating region (a blinking caret, a
//! clock). The user-facing motivation: such content prevents `wait_stable` from ever settling
//! and poisons baseline diffs unless the caller masks it out.

use std::time::Duration;

use glass_mcp::serve::config::ServeConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::{json, Value};

/// The blink rectangle glass-testapp's `--blink` mode animates (see its BLINK_* constants) —
/// fully inside the top-left quadrant, well clear of any seam, so masking it can't
/// accidentally exclude real (non-animating) content too.
const BLINK_REGION: (u32, u32, u32, u32) = (16, 16, 32, 32);

fn blink_region_json() -> Value {
    let (x, y, width, height) = BLINK_REGION;
    json!({ "x": x, "y": y, "width": width, "height": height })
}

/// Call `tool` with `args` (a JSON object), assert it did not error, and return the parsed
/// success envelope's `result` object — see glass-mcp's `tools::envelope`
/// (`{"ok":true,"tool":..., "result": {...}}`).
async fn call(client: &Peer<RoleClient>, tool: &str, args: Value) -> Value {
    let arguments = args
        .as_object()
        .unwrap_or_else(|| panic!("{tool} args must be a JSON object: {args}"))
        .clone();
    let result = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("{tool} call failed: {e}"));
    assert_ne!(
        result.is_error,
        Some(true),
        "{tool} returned an error result: {result:?}"
    );
    let text = result
        .content
        .iter()
        .find_map(|c| c.as_text().map(|t| t.text.clone()))
        .unwrap_or_else(|| panic!("{tool} returned no text content: {result:?}"));
    let envelope: Value = serde_json::from_str(&text)
        .unwrap_or_else(|e| panic!("{tool} text content is not JSON ({e}): {text}"));
    envelope["result"].clone()
}

/// Drive a real `glass-mcp` server end to end against `testapp --blink` launched under
/// `backend`: `wait_stable` must fail to settle without an `ignore` mask over the blink rect
/// (the region really is changing), must settle once masked (and never count the masked
/// motion as `saw_motion`), and a baseline diff with the same mask must report zero real
/// change plus exactly the masked pixel count.
pub async fn assert_blink_region_e2e(testapp: &str, backend: &str, start_timeout_ms: u64) {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().unwrap();
    let glass = glass_mcp::boot(None);
    let report = glass_mcp::audit::report_from_config(None, |_| None);
    tokio::spawn(async move {
        let cfg = ServeConfig {
            addr,
            token: Some("e2e".into()),
        };
        let _ = glass_mcp::serve::run_on(listener, cfg, glass, report).await;
    });
    // Give the listener a beat to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut tcfg = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/"));
    tcfg = tcfg.auth_header("e2e".to_string());
    let client =
        ().serve(StreamableHttpClientTransport::from_config(tcfg))
            .await
            .expect("initialize over http");

    call(
        &client,
        "glass_start",
        json!({
            "run": [testapp, "--blink"],
            "backend": backend,
            "timeout_ms": start_timeout_ms,
        }),
    )
    .await;

    // No ignore: the blink rect changes on every ~5ms tick, far faster than the 50ms poll
    // interval, so 3 consecutive identical polls (settle_frames) never happen within 400ms.
    let unmasked = call(
        &client,
        "glass_wait_stable",
        json!({
            "interval_ms": 50,
            "settle_frames": 3,
            "timeout_ms": 400,
            "include_image": false,
        }),
    )
    .await;
    assert_eq!(
        unmasked["settled"],
        json!(false),
        "must not settle without a mask over the blinking rect: {unmasked}"
    );
    assert_eq!(
        unmasked["saw_motion"],
        json!(true),
        "the blink must actually be observed changing: {unmasked}"
    );

    // Same interval, now masked: the rest of the fixture is static, so it settles quickly —
    // and the masked motion must never be reported as saw_motion (proves the mask, not a
    // merely-generous timeout, is why this settled).
    let masked = call(
        &client,
        "glass_wait_stable",
        json!({
            "interval_ms": 50,
            "settle_frames": 3,
            "timeout_ms": 3_000,
            "include_image": false,
            "ignore": [blink_region_json()],
        }),
    )
    .await;
    assert_eq!(
        masked["settled"],
        json!(true),
        "must settle once the blinking rect is masked: {masked}"
    );
    assert_eq!(
        masked["saw_motion"],
        json!(false),
        "motion confined to the ignore rect must never set saw_motion: {masked}"
    );

    call(&client, "glass_baseline_save", json!({ "name": "e2e" })).await;
    // Let the blink keep animating so the diff below genuinely exercises live, masked motion —
    // not two captures that happen to land in the same ~5ms tick.
    tokio::time::sleep(Duration::from_millis(150)).await;

    let diff = call(
        &client,
        "glass_diff",
        json!({
            "name": "e2e",
            "mode": "exact",
            "tolerance": 0,
            "ignore": [blink_region_json()],
        }),
    )
    .await;
    assert_eq!(
        diff["changed_pixels"],
        json!(0),
        "the only real change is the masked blink rect: {diff}"
    );
    let (_, _, w, h) = BLINK_REGION;
    assert_eq!(
        diff["ignored_pixels"],
        json!(u64::from(w * h)),
        "the mask must exclude exactly its own area: {diff}"
    );

    client.cancel().await.ok();
}
