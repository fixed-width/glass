//! Always-on network-transport tests: a real MCP handshake + tool call over HTTP,
//! plus auth and single-live-session takeover. Display-free (uses glass_doctor).

#![cfg(feature = "network")]

use std::time::Duration;

use glass_mcp::serve::config::ServeConfig;
use rmcp::ServiceExt;
use rmcp::model::{CallToolRequestParams, ErrorCode};
use rmcp::model::{SubscribeRequestParams, UnsubscribeRequestParams};
use rmcp::service::ServiceError;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;

/// Bind 127.0.0.1:0, start serve in the background, return the bound URL.
struct TestServer {
    url: String,
    cancel: tokio_util::sync::CancellationToken,
    task: tokio::task::JoinHandle<anyhow::Result<()>>,
}

impl TestServer {
    async fn shutdown(self) {
        self.cancel.cancel();
        tokio::time::timeout(Duration::from_secs(8), self.task)
            .await
            .expect("server shutdown bounded")
            .expect("join server")
            .expect("server shutdown");
    }
}

async fn start_server(token: Option<&str>) -> TestServer {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let cfg = ServeConfig {
        addr,
        token: token.map(String::from),
    };
    let glass = glass_mcp::boot(None);
    let report = glass_mcp::audit::report_from_config(None, |_| None);
    let cancel = tokio_util::sync::CancellationToken::new();
    let shutdown = cancel.clone();
    let task = tokio::spawn(async move {
        glass_mcp::serve::run_on_until(listener, cfg, glass, report, async move {
            shutdown.cancelled().await;
        })
        .await
    });
    TestServer {
        url: format!("http://{addr}/"),
        cancel,
        task,
    }
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

fn assert_method_not_found(error: ServiceError) {
    match error {
        ServiceError::McpError(error) => assert_eq!(
            error.code,
            ErrorCode::METHOD_NOT_FOUND,
            "unexpected MCP error: {error:?}"
        ),
        other => panic!("expected MCP method-not-found error, got {other:?}"),
    }
}

#[tokio::test]
async fn doctor_round_trips_over_http() {
    let server = start_server(Some("tok")).await;
    let client = ().serve(client_transport(&server.url, Some("tok"))).await.expect("initialize");
    let result = client
        .call_tool(CallToolRequestParams::new("glass_doctor"))
        .await
        .expect("glass_doctor call");
    // The call succeeded (not an error result) and reads like the doctor report.
    assert_ne!(
        result.is_error,
        Some(true),
        "glass_doctor returned an error result"
    );
    let text = format!("{result:?}");
    assert!(
        text.contains("backend") || text.contains("x11"),
        "unexpected doctor result: {text}"
    );
    client.cancel().await.ok();
    server.shutdown().await;
}

#[tokio::test]
async fn empty_glass_do_is_a_structured_error_over_http() {
    let server = start_server(Some("tok")).await;
    let client = ().serve(client_transport(&server.url, Some("tok"))).await.expect("initialize");
    let arguments = serde_json::json!({ "actions": [] })
        .as_object()
        .unwrap()
        .clone();

    let result = client
        .call_tool(CallToolRequestParams::new("glass_do").with_arguments(arguments))
        .await
        .expect("glass_do call");

    assert_eq!(result.is_error, Some(true));
    let envelope = result
        .content
        .iter()
        .filter_map(|block| block.as_text())
        .find_map(|text| serde_json::from_str::<serde_json::Value>(&text.text).ok())
        .expect("glass_do error result must contain a JSON envelope");
    assert_eq!(envelope["ok"], false);
    assert_eq!(envelope["tool"], "glass_do");
    assert_eq!(envelope["error"]["code"], "invalid_sequence");
    assert!(
        envelope.get("result").is_none(),
        "an invalid sequence must not carry a success result: {envelope}"
    );
    client.cancel().await.ok();
    server.shutdown().await;
}

#[tokio::test]
async fn rejects_missing_token() {
    let server = start_server(Some("tok")).await;
    // No auth header → initialize should fail (transport returns 401).
    let res = ().serve(client_transport(&server.url, None)).await;
    assert!(res.is_err(), "initialize without a token must fail");
    server.shutdown().await;
}

#[tokio::test]
async fn second_client_takes_over() {
    let server = start_server(Some("tok")).await;
    let c1 = ().serve(client_transport(&server.url, Some("tok"))).await.expect("first client");
    // A second client takes over the single live slot instead of being rejected —
    // this is the reconnect path (a client that dropped without a clean DELETE
    // would otherwise be locked out of its own server until the zombie expired).
    let c2 =
        ().serve(client_transport(&server.url, Some("tok")))
            .await
            .expect("second client takes over");
    // The newcomer is fully live over the taken-over slot.
    c2.call_tool(CallToolRequestParams::new("glass_doctor"))
        .await
        .expect("taken-over session serves calls");
    // c1's session was evicted server-side (its next request would 404). We don't
    // assert on c1 here: the rmcp client transparently re-initializes on a 404, so
    // a c1 call would silently heal into a fresh session rather than surface the
    // eviction. The one-live-slot invariant is covered precisely by the
    // session_gate unit tests; this test's job is the real-path reconnect admission.
    let _ = c1;
    c2.cancel().await.ok();
    drop(c1);
    server.shutdown().await;
}

#[tokio::test]
async fn resources_are_read_only_without_lists_templates_or_subscriptions() {
    let server = start_server(Some("tok")).await;
    let client = ().serve(client_transport(&server.url, Some("tok"))).await.expect("initialize");

    assert!(
        client
            .list_resources(None)
            .await
            .expect("list resources")
            .resources
            .is_empty()
    );
    assert!(
        client
            .list_resource_templates(None)
            .await
            .expect("list resource templates")
            .resource_templates
            .is_empty()
    );
    let subscribe = client
        .subscribe(SubscribeRequestParams::new("glass-artifact://server/id"))
        .await
        .expect_err("subscriptions are unsupported");
    let unsubscribe = client
        .unsubscribe(UnsubscribeRequestParams::new("glass-artifact://server/id"))
        .await
        .expect_err("unsubscriptions are unsupported");
    assert_method_not_found(subscribe);
    assert_method_not_found(unsubscribe);

    client.cancel().await.ok();
    server.shutdown().await;
}
