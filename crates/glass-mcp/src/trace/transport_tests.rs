use std::sync::{Arc, Mutex};
use std::time::Duration;

use glass_core::{Frame, ProtectedHostPath};
use rmcp::{ServiceExt, model::CallToolRequestParams};
use serde_json::{Value, json};

use crate::server::GlassServer;
use crate::tool_profile::ToolProfile;
use crate::tools::testutil::{FakePlatform, fake_tree, glass_with_a11y};

use super::{TraceConfig, TraceRecorder, inspect};

struct Harness {
    root: tempfile::TempDir,
    server: GlassServer,
    task: tokio::task::JoinHandle<()>,
    client: rmcp::service::RunningService<rmcp::RoleClient, ()>,
    recorder: TraceRecorder,
    captures: Arc<Mutex<usize>>,
    paths: Arc<Mutex<Vec<ProtectedHostPath>>>,
    inputs: Arc<Mutex<Vec<String>>>,
}

impl Harness {
    async fn stdio(profile: ToolProfile, max_bytes: Option<u64>) -> Self {
        let root = super::tests::private_root();
        let captures = Arc::new(Mutex::new(0));
        let paths = Arc::new(Mutex::new(vec![]));
        let inputs = Arc::new(Mutex::new(vec![]));
        let mut platform = FakePlatform::new(100, 100)
            .with_frames(vec![Frame::solid(100, 100, [1, 2, 3, 255])])
            .with_capture_log(captures.clone())
            .with_event_log(inputs.clone());
        platform.protected_paths = Some(paths.clone());
        let glass = glass_with_a11y(platform, fake_tree());
        let config = TraceConfig::new(root.path().to_owned(), max_bytes).unwrap();
        let server = GlassServer::new_configured(
            glass,
            crate::audit::report_from_config(None, |_| None),
            profile,
            Some(&config),
            "stdio",
        )
        .unwrap();
        let recorder = server.trace_recorder().unwrap();
        let (server_io, client_io) = tokio::io::duplex(64 * 1024);
        let service = server.clone();
        let task = tokio::spawn(async move {
            service
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap();
        });
        let client = ().serve(client_io).await.unwrap();
        Self {
            root,
            server,
            task,
            client,
            recorder,
            captures,
            paths,
            inputs,
        }
    }

    async fn call(&self, tool: &str, args: Value) -> rmcp::model::CallToolResult {
        self.client
            .call_tool(
                CallToolRequestParams::new(tool.to_owned())
                    .with_arguments(args.as_object().unwrap().clone()),
            )
            .await
            .unwrap()
    }

    async fn start(&self) {
        let result = self.call("glass_start", json!({"run": ["app"], "sandbox": "off", "env": {"SECRET_ENV_NAME": "SECRET_ENV_VALUE"}})).await;
        assert!(!result.is_error.unwrap_or(false), "{result:?}");
    }

    async fn finish(
        self,
    ) -> (
        tempfile::TempDir,
        std::path::PathBuf,
        super::inspect::Inspection,
    ) {
        self.client.cancel().await.unwrap();
        self.task.await.unwrap();
        crate::shutdown::run_shutdown_with_trace(
            self.server.sessions(),
            glass_core::TEARDOWN_BUDGET,
            Some(self.recorder.clone()),
        )
        .await;
        let path = self.recorder.path().to_owned();
        crate::cleanup_evidence(Some(self.recorder.clone()), self.server.artifact_store()).await;
        let report = inspect(&path).unwrap_or_else(|error| {
            panic!(
                "trace inspection after cleanup: {error:#}; recorder: {}",
                self.recorder.status()
            )
        });
        (self.root, path, report)
    }
}

fn bounded_image_bytes(result: &rmcp::model::CallToolResult) -> Vec<u8> {
    use base64::Engine;
    assert!(!result.is_error.unwrap_or(false));
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(
            &result
                .content
                .iter()
                .find_map(|b| b.as_image())
                .unwrap()
                .data,
        )
        .unwrap();
    assert_eq!(
        glass_core::frame_from_webp(&bytes).unwrap(),
        Frame::solid(25, 25, [1, 2, 3, 255])
    );
    let envelope: Value = serde_json::from_str(
        &result
            .content
            .iter()
            .find_map(|b| b.as_text())
            .unwrap()
            .text,
    )
    .unwrap();
    assert_eq!(
        envelope["result"]["image"]["source"],
        json!({"x":0,"y":0,"width":100,"height":100})
    );
    assert_eq!(envelope["result"]["image"]["pixel_exact"], false);
    assert!(
        result
            .content
            .iter()
            .filter_map(|b| b.as_text())
            .any(|t| t.text == crate::untrusted::IMAGE_NOTE)
    );
    bytes
}

fn assert_bounded_trace(path: &std::path::Path, report: &super::inspect::Inspection, bytes: &[u8]) {
    assert!(report.complete, "{report:?}");
    let images = report
        .events
        .iter()
        .flat_map(|e| e["evidence"].as_array().into_iter().flatten())
        .filter(|e| e["mime_type"] == "image/webp")
        .collect::<Vec<_>>();
    assert!(!images.is_empty());
    for image in images {
        assert_eq!(
            std::fs::read(path.join(image["payload"]["path"].as_str().unwrap())).unwrap(),
            bytes
        );
    }
    let content = String::from_utf8_lossy(&all_content(path)).into_owned();
    assert!(content.contains("\"max_width\":25"));
    assert!(content.contains("\"pixel_exact\":false"));
}

#[tokio::test]
async fn bounded_stdio_images_and_traces_match_in_both_profiles() {
    for profile in [ToolProfile::Full, ToolProfile::Lean] {
        let harness = Harness::stdio(profile, None).await;
        harness.start().await;
        for terminal in ["screenshot", "diff"] {
            let invalid = harness
                .call(
                    "glass_do",
                    json!({"actions":[{"action":"key","chord":"Return"}],
                "then":{terminal:{"name":"base","include_image":false,"max_width":0}}}),
                )
                .await;
            assert!(invalid.is_error.unwrap_or(false));
        }
        assert!(harness.inputs.lock().unwrap().is_empty());
        assert_eq!(*harness.captures.lock().unwrap(), 0);
        let result = harness
            .call("glass_screenshot", json!({"max_width":25}))
            .await;
        let bytes = bounded_image_bytes(&result);
        assert_eq!(*harness.captures.lock().unwrap(), 1);
        let (_root, path, report) = harness.finish().await;
        assert_bounded_trace(&path, &report, &bytes);
    }
}

#[cfg(feature = "network")]
#[tokio::test]
async fn bounded_http_image_and_trace_retain_the_same_requested_pixels() {
    use rmcp::transport::StreamableHttpClientTransport;
    let root = super::tests::private_root();
    let captures = Arc::new(Mutex::new(0));
    let mut platform = FakePlatform::new(100, 100)
        .with_frames(vec![Frame::solid(100, 100, [1, 2, 3, 255])])
        .with_capture_log(captures.clone());
    platform.protected_paths = Some(Arc::new(Mutex::new(vec![])));
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = crate::serve::config::ServeConfig {
        addr,
        token: None,
        tool_profile: ToolProfile::Lean,
        trace: Some(TraceConfig::new(root.path().to_owned(), None).unwrap()),
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let stopping = cancel.clone();
    let server = tokio::spawn(crate::serve::run_on_until(
        listener,
        config,
        crate::tools::testutil::glass_with(platform),
        crate::audit::report_from_config(None, |_| None),
        async move { stopping.cancelled().await },
    ));
    let client = ()
        .serve(StreamableHttpClientTransport::from_uri(format!(
            "http://{addr}/"
        )))
        .await
        .unwrap();
    let start = client
        .call_tool(
            CallToolRequestParams::new("glass_start").with_arguments(
                json!({"run":["app"],"sandbox":"off"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await
        .unwrap();
    assert!(!start.is_error.unwrap_or(false), "{start:?}");
    let result = client
        .call_tool(
            CallToolRequestParams::new("glass_screenshot")
                .with_arguments(json!({"max_width":25}).as_object().unwrap().clone()),
        )
        .await
        .unwrap();
    let bytes = bounded_image_bytes(&result);
    assert_eq!(*captures.lock().unwrap(), 1);
    client.cancel().await.unwrap();
    cancel.cancel();
    server.await.unwrap().unwrap();
    let path = std::fs::read_dir(root.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    assert_bounded_trace(&path, &inspect(&path).unwrap(), &bytes);
}

#[tokio::test]
async fn writer_failure_does_not_change_or_repeat_input() {
    let harness = Harness::stdio(ToolProfile::Full, None).await;
    harness.start().await;
    harness.recorder.idle().await;
    harness.recorder.fail_next_write();
    let result = harness
        .call("glass_type", json!({"text": "one dispatch"}))
        .await;
    assert!(!result.is_error.unwrap_or(false));
    assert_eq!(*harness.inputs.lock().unwrap(), ["type(one dispatch)"]);
    harness.recorder.idle().await;
    assert_eq!(harness.recorder.status()["state"], "failed");
    let (_root, _path, report) = harness.finish().await;
    assert_eq!(report.exit_code(), 2);
}

#[tokio::test]
async fn failed_replacement_clears_the_previous_session_context() {
    let harness = Harness::stdio(ToolProfile::Full, None).await;
    harness.start().await;
    let failed = harness
        .call(
            "glass_start",
            json!({"run": ["replacement"], "sandbox": "off"}),
        )
        .await;
    assert!(failed.is_error.unwrap_or(false));
    assert!(
        harness
            .call("glass_logs", json!({}))
            .await
            .is_error
            .unwrap_or(false)
    );
    let (_root, _path, report) = harness.finish().await;
    assert!(report.complete);
    let contexts: Vec<_> = report
        .events
        .iter()
        .filter(|event| event["kind"] == "session_context")
        .collect();
    assert_eq!(contexts.len(), 3);
    assert_eq!(contexts[0]["data"]["backend"], "x11");
    assert!(contexts[0]["data"]["session"].is_number());
    for context in &contexts[1..] {
        assert!(context["data"]["backend"].is_null());
        assert!(context["data"]["session"].is_null());
    }
}

#[cfg(feature = "network")]
#[tokio::test]
async fn http_takeover_retains_distinct_clients_and_does_not_record_transport_secrets() {
    use rmcp::transport::StreamableHttpClientTransport;
    use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
    let root = super::tests::private_root();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let config = crate::serve::config::ServeConfig {
        addr,
        token: Some("HTTP_TOKEN_CANARY".into()),
        tool_profile: ToolProfile::Lean,
        trace: Some(TraceConfig::new(root.path().to_owned(), None).unwrap()),
    };
    let cancel = tokio_util::sync::CancellationToken::new();
    let stopping = cancel.clone();
    let server = tokio::spawn(crate::serve::run_on_until(
        listener,
        config,
        crate::boot(None),
        crate::audit::report_from_config(None, |_| None),
        async move { stopping.cancelled().await },
    ));
    let connect = || {
        StreamableHttpClientTransport::from_config(
            StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/"))
                .auth_header("HTTP_TOKEN_CANARY"),
        )
    };
    let first = ().serve(connect()).await.unwrap();
    let mut request = CallToolRequestParams::new("glass_do")
        .with_arguments(json!({"actions": []}).as_object().unwrap().clone());
    request.meta = Some(rmcp::model::Meta(
        json!({"secret":"META_CANARY"}).as_object().unwrap().clone(),
    ));
    assert!(
        first
            .call_tool(request)
            .await
            .unwrap()
            .is_error
            .unwrap_or(false)
    );
    let second = ().serve(connect()).await.unwrap();
    assert!(
        second
            .call_tool(CallToolRequestParams::new("glass_logs"))
            .await
            .unwrap()
            .is_error
            .unwrap_or(false)
    );
    second.cancel().await.unwrap();
    let _ = first.cancel().await;
    cancel.cancel();
    server.await.unwrap().unwrap();
    let path = std::fs::read_dir(root.path())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let report = inspect(&path).unwrap();
    assert!(report.complete, "{report:?}");
    let clients: std::collections::BTreeSet<_> = report
        .events
        .iter()
        .filter(|e| e["kind"] == "call_received")
        .map(|e| e["client"].as_u64().unwrap())
        .collect();
    assert_eq!(clients.len(), 2);
    let body = String::from_utf8_lossy(&all_content(&path)).into_owned();
    assert!(!body.contains("HTTP_TOKEN_CANARY"));
    assert!(!body.contains("META_CANARY"));
}

#[tokio::test]
async fn oversized_text_is_portable_after_artifact_cleanup() {
    let harness = Harness::stdio(ToolProfile::Full, None).await;
    harness.start().await;
    let expected = "unicode observation 🌍\n".repeat(2000);
    let call = harness.recorder.begin_call("glass_test", 1).unwrap();
    call.arguments(&json!({}));
    let output_text = expected.clone();
    let response = super::ACTIVE_CALL
        .scope(
            call,
            harness.server.run_trace_test_job(move |_| {
                crate::output::ToolOutput(vec![crate::output::OutContent::untrusted_observation(
                    &output_text,
                )])
            }),
        )
        .await
        .unwrap();
    let link = response
        .content
        .iter()
        .find_map(|block| match block {
            rmcp::model::ContentBlock::ResourceLink(link) => Some(link),
            _ => None,
        })
        .unwrap();
    let artifact_path = link.meta.as_ref().unwrap().0["glass"]["localPath"]
        .as_str()
        .unwrap()
        .to_owned();
    let original = std::fs::read(&artifact_path).unwrap();
    let (_root, path, report) = harness.finish().await;
    assert!(!std::path::Path::new(&artifact_path).exists());
    assert!(report.complete, "{report:?}");
    let resource = report
        .events
        .iter()
        .flat_map(|event| event["evidence"].as_array().into_iter().flatten())
        .find(|e| e["source_uri"] == link.uri)
        .unwrap();
    assert_eq!(
        std::fs::read(path.join(resource["payload"]["path"].as_str().unwrap())).unwrap(),
        original
    );
    let descriptor = report
        .events
        .iter()
        .flat_map(|event| event["evidence"].as_array().into_iter().flatten())
        .find(|e| e["label"] == "resource_descriptor")
        .unwrap();
    let recorded: Value = serde_json::from_slice(
        &std::fs::read(path.join(descriptor["payload"]["path"].as_str().unwrap())).unwrap(),
    )
    .unwrap();
    assert_eq!(
        recorded,
        serde_json::to_value(rmcp::model::ContentBlock::ResourceLink(link.clone())).unwrap()
    );
}

fn all_content(path: &std::path::Path) -> Vec<u8> {
    let mut result = std::fs::read(path.join("events.jsonl")).unwrap();
    for entry in std::fs::read_dir(path.join("blobs")).unwrap() {
        result.extend(std::fs::read(entry.unwrap().path()).unwrap());
    }
    result
}

#[tokio::test]
async fn stdio_preserves_requested_evidence_and_excludes_environment_and_invalid_arguments() {
    let harness = Harness::stdio(ToolProfile::Full, None).await;
    harness.start().await;
    let configured = harness.paths.lock().unwrap().clone();
    assert!(
        configured
            .iter()
            .any(|path| path.path == harness.root.path())
    );
    assert_eq!(
        configured.len(),
        3,
        "trace root and both artifact paths must be composed"
    );
    let before = *harness.captures.lock().unwrap();
    let screenshot = harness.call("glass_screenshot", json!({})).await;
    assert!(!screenshot.is_error.unwrap_or(false));
    assert_eq!(*harness.captures.lock().unwrap(), before + 1);
    let invalid = harness
        .call(
            "glass_type",
            json!({"text": {"bad": "INVALID_INPUT_SECRET"}}),
        )
        .await;
    assert!(invalid.is_error.unwrap_or(false));
    let text = harness
        .call(
            "glass_type",
            json!({"text": "retained text", "ignored_unknown": "UNKNOWN_FIELD_SECRET"}),
        )
        .await;
    assert!(!text.is_error.unwrap_or(false));
    assert_eq!(
        *harness.captures.lock().unwrap(),
        before + 1,
        "tracing adds no screenshots"
    );
    let (_root, path, report) = harness.finish().await;
    assert!(report.complete, "{report:?}");
    let content = String::from_utf8_lossy(&all_content(&path)).into_owned();
    for secret in [
        "SECRET_ENV_NAME",
        "SECRET_ENV_VALUE",
        "INVALID_INPUT_SECRET",
        "UNKNOWN_FIELD_SECRET",
    ] {
        assert!(!content.contains(secret), "leaked {secret}");
    }
    assert!(content.contains("retained text"));
    let image = report
        .events
        .iter()
        .flat_map(|event| event["evidence"].as_array().into_iter().flatten())
        .find(|e| e["mime_type"] == "image/webp")
        .unwrap();
    let bytes = std::fs::read(path.join(image["payload"]["path"].as_str().unwrap())).unwrap();
    let returned = screenshot
        .content
        .iter()
        .find_map(|b| b.as_image())
        .unwrap();
    use base64::Engine;
    assert_eq!(
        bytes,
        base64::engine::general_purpose::STANDARD
            .decode(&returned.data)
            .unwrap()
    );
}

#[tokio::test]
async fn lean_batch_keeps_fail_fast_outcomes_and_refused_tool_body_private() {
    let harness = Harness::stdio(ToolProfile::Lean, None).await;
    harness.start().await;
    let missing = harness
        .client
        .call_tool(
            CallToolRequestParams::new("glass_type").with_arguments(
                json!({"text":"OMITTED_TOOL_SECRET"})
                    .as_object()
                    .unwrap()
                    .clone(),
            ),
        )
        .await;
    assert!(missing.is_err());
    let batch = harness.call("glass_do", json!({"actions":[{"action":"type", "text":"first"}, {"action":"wait_for_element", "name":"missing", "timeout_ms":0}, {"action":"type","text":"unexecuted"}]})).await;
    assert!(batch.is_error.unwrap_or(false));
    let (_root, path, report) = harness.finish().await;
    assert!(report.complete, "{report:?}");
    let content = String::from_utf8_lossy(&all_content(&path)).into_owned();
    assert!(!content.contains("OMITTED_TOOL_SECRET"));
    assert!(content.contains("unexecuted"));
    assert!(content.contains("predicate_not_matched"));
}

#[tokio::test]
async fn cancelled_request_keeps_the_workers_later_outcome() {
    let harness = Harness::stdio(ToolProfile::Full, None).await;
    let call = harness.recorder.begin_call("glass_test", 1).unwrap();
    let call_id = call.id;
    call.arguments(&json!({}));
    let (started_tx, started_rx) = tokio::sync::oneshot::channel();
    let (finish_tx, finish_rx) = std::sync::mpsc::channel();
    let server = harness.server.clone();
    let operation = tokio::spawn(super::ACTIVE_CALL.scope(call.clone(), async move {
        let mut guard = super::RequestGuard::new(call);
        let result = server
            .run_trace_test_job(move |_| {
                let _ = started_tx.send(());
                finish_rx.recv_timeout(Duration::from_secs(5)).unwrap();
                crate::output::ToolOutput::result("glass_test", json!({"mutations":1}))
            })
            .await;
        guard.complete();
        result
    }));
    started_rx.await.unwrap();
    operation.abort();
    let _ = operation.await;
    finish_tx.send(()).unwrap();
    // A following worker job is a barrier for the cancelled job's completion.
    harness.call("glass_logs", json!({})).await;
    let (_root, _path, report) = harness.finish().await;
    let events: Vec<_> = report
        .events
        .iter()
        .filter(|event| event["call"] == call_id)
        .collect();
    assert!(
        events
            .iter()
            .any(|event| event["kind"] == "request_abandoned")
    );
    assert_eq!(
        events
            .iter()
            .filter(|event| event["kind"] == "logical_outcome")
            .count(),
        1
    );
    assert!(
        !events
            .iter()
            .any(|event| event["kind"] == "response_constructed")
    );
}
