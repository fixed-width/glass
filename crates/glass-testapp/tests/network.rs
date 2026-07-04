//! End-to-end: drive glass-mcp over HTTP against the testapp under Xvfb. `#[ignore]d`;
//! run via `./scripts/test-x11.sh network_screenshot_over_http`.

mod common;

use std::time::Duration;

use base64::Engine;
use common::Xvfb;
use glass_mcp::serve::config::ServeConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::ServiceExt;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires an X server; run via scripts/test-x11.sh"]
async fn network_screenshot_over_http() {
    let xvfb = Xvfb::start();
    // The x11 backend reads GLASS_DISPLAY (never ambient $DISPLAY).
    // SAFETY: single-threaded test setup; runs before any server task spawns.
    unsafe { std::env::set_var("GLASS_DISPLAY", &xvfb.display) };

    // Start serve on an ephemeral loopback port.
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
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

    // Connect the rmcp Streamable-HTTP client. `auth_header` takes the BARE token;
    // the reqwest transport prepends `Bearer ` itself.
    let mut tcfg = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/"));
    tcfg = tcfg.auth_header("e2e".to_string());
    let client =
        ().serve(StreamableHttpClientTransport::from_config(tcfg))
            .await
            .expect("initialize over http");

    // Launch the testapp.
    let mut start_args = serde_json::Map::new();
    start_args.insert(
        "run".to_string(),
        serde_json::Value::Array(vec![serde_json::Value::String(TESTAPP.to_string())]),
    );
    let start = client
        .call_tool(CallToolRequestParams::new("glass_start").with_arguments(start_args))
        .await
        .expect("glass_start call");
    assert_ne!(
        start.is_error,
        Some(true),
        "glass_start returned an error result: {start:?}"
    );

    // Let it render, then screenshot.
    tokio::time::sleep(Duration::from_millis(300)).await;
    let shot = client
        .call_tool(CallToolRequestParams::new("glass_screenshot"))
        .await
        .expect("glass_screenshot call");
    assert_ne!(
        shot.is_error,
        Some(true),
        "glass_screenshot returned an error result: {shot:?}"
    );

    // Extract the image content (base64 WebP) and decode it.
    let webp_b64 = shot
        .content
        .iter()
        .find_map(|c| c.as_image().map(|img| img.data.clone()))
        .expect("screenshot returned image content");
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(webp_b64)
        .expect("base64 decode");
    let img = image::load_from_memory(&bytes)
        .expect("decode webp")
        .to_rgba8();
    assert!(
        img.width() >= 320 && img.height() >= 240,
        "unexpected dims {:?}",
        img.dimensions()
    );

    // The testapp paints colored quadrants — assert a non-uniform (non-blank) frame.
    let first = img.get_pixel(0, 0);
    let any_different = img.pixels().any(|p| p != first);
    assert!(
        any_different,
        "frame is uniform/blank — image content did not cross the wire"
    );

    client.cancel().await.ok();
}
