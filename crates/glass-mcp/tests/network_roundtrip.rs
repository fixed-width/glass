//! Always-on network-transport tests: a real MCP handshake + tool call over HTTP,
//! plus auth and single-client enforcement. Display-free (uses glass_doctor).

#![cfg(feature = "network")]

use std::time::Duration;

use glass_mcp::serve::config::ServeConfig;
use rmcp::ServiceExt;
use rmcp::model::CallToolRequestParams;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

/// Bind 127.0.0.1:0, start serve in the background, return the bound URL.
async fn start_server(token: Option<&str>) -> String {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = ServeConfig { addr, token: token.map(String::from) };
    let glass = glass_mcp::boot();
    tokio::spawn(async move {
        let _ = glass_mcp::serve::run_on(listener, cfg, glass).await;
    });
    // Give the listener a beat to start accepting.
    tokio::time::sleep(Duration::from_millis(50)).await;
    format!("http://{addr}/")
}

/// Build an rmcp Streamable-HTTP client transport for `url`, optionally bearing `token`.
///
/// NOTE: `auth_header` takes the bare token (no `Bearer ` prefix). The reqwest transport
/// sends it via `RequestBuilder::bearer_auth`, which prepends `Bearer ` itself — passing
/// `"Bearer tok"` here would put `Authorization: Bearer Bearer tok` on the wire and 401.
fn client_transport(
    url: &str,
    token: Option<&str>,
) -> StreamableHttpClientTransport<reqwest::Client> {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url.to_string());
    if let Some(t) = token {
        cfg = cfg.auth_header(t.to_string());
    }
    StreamableHttpClientTransport::from_config(cfg)
}

#[tokio::test]
async fn doctor_round_trips_over_http() {
    let url = start_server(Some("tok")).await;
    let client = ().serve(client_transport(&url, Some("tok"))).await.expect("initialize");
    let result = client
        .call_tool(CallToolRequestParams::new("glass_doctor"))
        .await
        .expect("glass_doctor call");
    // The call succeeded (not an error result) and reads like the doctor report.
    assert_ne!(result.is_error, Some(true), "glass_doctor returned an error result");
    let text = format!("{result:?}");
    assert!(text.contains("backend") || text.contains("x11"), "unexpected doctor result: {text}");
    client.cancel().await.ok();
}

#[tokio::test]
async fn rejects_missing_token() {
    let url = start_server(Some("tok")).await;
    // No auth header → initialize should fail (transport returns 401).
    let res = ().serve(client_transport(&url, None)).await;
    assert!(res.is_err(), "initialize without a token must fail");
}

#[tokio::test]
async fn second_client_is_rejected() {
    let url = start_server(Some("tok")).await;
    let _c1 = ().serve(client_transport(&url, Some("tok"))).await.expect("first client");
    // Second initialize must fail while the first session is live.
    let c2 = ().serve(client_transport(&url, Some("tok"))).await;
    assert!(c2.is_err(), "second concurrent client must be rejected");
}
