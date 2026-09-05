//! Test-only Streamable HTTP helpers shared by public MCP acceptance tests.

use std::time::Duration;

use base64::Engine;
use glass_core::Glass;
use glass_mcp::serve::config::ServeConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::Value;
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

const MCP_CLIENT_CANCEL_BUDGET: Duration = Duration::from_secs(2);
#[cfg(target_os = "macos")]
const MCP_STOP_BUDGET: Duration = Duration::from_secs(10);
// Allow 8s for both 3s server cleanup phases plus scheduling and transport cancellation.
const MCP_SERVER_JOIN_BUDGET: Duration = Duration::from_secs(8);
#[cfg(target_os = "macos")]
const PROCESS_START_BUDGET: Duration = Duration::from_secs(5);

/// One real image content block from an MCP tool response.
#[derive(Debug)]
pub struct ImageView {
    pub index: usize,
    pub mime_type: String,
    data: String,
}

impl ImageView {
    pub fn decode(&self) -> Result<Vec<u8>, base64::DecodeError> {
        base64::engine::general_purpose::STANDARD.decode(&self.data)
    }
}

/// The trusted envelope result plus every text and image block returned by one tool call.
#[derive(Debug)]
pub struct CallView {
    pub result: Value,
    pub all_text: String,
    pub images: Vec<ImageView>,
}

/// Parse only the requested tool's complete trusted success envelope, never app-derived siblings.
fn successful_envelope_result(text: &str, tool: &str) -> Option<Value> {
    let envelope = serde_json::from_str::<Value>(text).ok()?;
    (envelope.get("ok") == Some(&Value::Bool(true))
        && envelope.get("tool") == Some(&Value::String(tool.to_string())))
    .then(|| envelope.get("result").cloned())
    .flatten()
}

/// Call a public MCP tool while retaining its structured result and real content blocks.
pub async fn try_call_full(
    client: &Peer<RoleClient>,
    tool: &str,
    args: Value,
) -> Result<CallView, String> {
    let arguments = args
        .as_object()
        .ok_or_else(|| format!("{tool} args must be a JSON object: {args}"))?
        .clone();
    let response = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .map_err(|error| format!("{tool} transport failure: {error}"))?;
    let mut result = Value::Null;
    let mut all_text = String::new();
    let mut images = Vec::new();
    for (index, block) in response.content.iter().enumerate() {
        if let Some(text) = block.as_text() {
            all_text.push_str(&text.text);
            all_text.push('\n');
            if let Some(envelope_result) = successful_envelope_result(&text.text, tool) {
                result = envelope_result;
            }
        }
        if let Some(image) = block.as_image() {
            images.push(ImageView {
                index,
                mime_type: image.mime_type.clone(),
                data: image.data.clone(),
            });
        }
    }
    if response.is_error == Some(true) {
        return Err(format!("{tool} errored: {all_text}"));
    }
    if result == Value::Null {
        return Err(format!("{tool} lacked a trusted result: {all_text}"));
    }
    Ok(CallView {
        result,
        all_text,
        images,
    })
}

pub async fn call_full(client: &Peer<RoleClient>, tool: &str, args: Value) -> CallView {
    try_call_full(client, tool, args)
        .await
        .unwrap_or_else(|error| panic!("{error}"))
}

pub async fn call(client: &Peer<RoleClient>, tool: &str, args: Value) -> (Value, String) {
    let view = call_full(client, tool, args).await;
    (view.result, view.all_text)
}

async fn connect(url: String, token: &str) -> Result<RunningService<RoleClient, ()>, String> {
    let mut cfg = StreamableHttpClientTransportConfig::with_uri(url);
    cfg = cfg.auth_header(token.to_string());
    ().serve(StreamableHttpClientTransport::from_config(cfg))
        .await
        .map_err(|error| format!("initialize Streamable HTTP MCP client: {error}"))
}

/// An in-process HTTP MCP server with bounded, graceful teardown.
pub struct InProcessMcpHarness {
    client: RunningService<RoleClient, ()>,
    cancel: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

impl InProcessMcpHarness {
    pub async fn boot(glass: Glass, token: &str) -> Self {
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
            .await
            .expect("bind an ephemeral loopback port");
        let addr = listener.local_addr().expect("read loopback address");
        let report = glass_mcp::audit::report_from_config(None, |_| None);
        let cancel = CancellationToken::new();
        let shutdown = cancel.clone();
        let server_token = token.to_string();
        let server = tokio::spawn(async move {
            let cfg = ServeConfig {
                addr,
                token: Some(server_token),
                tool_profile: Default::default(),
            };
            glass_mcp::serve::run_on_until(listener, cfg, glass, report, async move {
                shutdown.cancelled().await;
            })
            .await
        });
        tokio::time::sleep(Duration::from_millis(50)).await;

        let client = match connect(format!("http://{addr}/"), token).await {
            Ok(client) => client,
            Err(error) => {
                cancel.cancel();
                match await_server(server, "MCP server startup cleanup").await {
                    Ok(()) => panic!("{error}"),
                    Err(cleanup) => panic!("{error}; startup cleanup also failed: {cleanup}"),
                }
            }
        };
        Self {
            client,
            cancel,
            server,
        }
    }

    pub fn peer(&self) -> Peer<RoleClient> {
        self.client.peer().clone()
    }

    pub async fn shutdown(self) -> Result<(), String> {
        let Self {
            client,
            cancel,
            server,
        } = self;
        // Signal first so a stalled DELETE cannot delay bounded server drain and session teardown.
        cancel.cancel();
        let client = await_cleanup(
            "MCP client cancellation",
            MCP_CLIENT_CANCEL_BUDGET,
            client.cancel(),
        )
        .await
        .and_then(|result| {
            result.map_err(|error| format!("MCP client cancellation failed: {error}"))
        });
        let server = await_server(server, "MCP server graceful shutdown").await;
        client.and(server)
    }
}

pub async fn await_cleanup<T>(
    what: &str,
    budget: Duration,
    future: impl std::future::Future<Output = T>,
) -> Result<T, String> {
    tokio::time::timeout(budget, future)
        .await
        .map_err(|_| format!("{what} exceeded {budget:?}"))
}

async fn await_server(server: JoinHandle<anyhow::Result<()>>, what: &str) -> Result<(), String> {
    match await_cleanup(what, MCP_SERVER_JOIN_BUDGET, server).await {
        Ok(Ok(Ok(()))) => Ok(()),
        Ok(Ok(Err(error))) => Err(format!("{what} failed: {error}")),
        Ok(Err(error)) => Err(format!("{what} task panicked or was cancelled: {error}")),
        Err(error) => Err(error),
    }
}

/// A real `glass-mcp serve --http` child. The process boundary is required on macOS so its `main`
/// initializes AppKit on thread zero before the server's platform worker uses the macOS factory.
#[cfg(target_os = "macos")]
pub struct ProcessMcpHarness {
    client: Option<RunningService<RoleClient, ()>>,
    child: Option<std::process::Child>,
    _token_file: tempfile::NamedTempFile,
    stderr_file: tempfile::NamedTempFile,
}

#[cfg(target_os = "macos")]
impl ProcessMcpHarness {
    pub async fn spawn(binary: &str, token: &str) -> Self {
        use std::io::Write;
        use std::process::{Command, Stdio};

        let listener = std::net::TcpListener::bind("127.0.0.1:0")
            .expect("reserve an ephemeral loopback address for glass-mcp");
        let addr = listener
            .local_addr()
            .expect("read reserved loopback address");
        drop(listener);

        let mut token_file = tempfile::NamedTempFile::new().expect("create MCP token file");
        writeln!(token_file, "{token}").expect("write MCP token file");
        token_file.flush().expect("flush MCP token file");
        let stderr_file = tempfile::NamedTempFile::new().expect("create MCP stderr file");
        let stderr = stderr_file.reopen().expect("reopen MCP stderr file");
        let child = Command::new(binary)
            .args([
                "serve",
                "--http",
                "--addr",
                &addr.to_string(),
                "--token-file",
            ])
            .arg(token_file.path())
            .stdout(Stdio::null())
            .stderr(Stdio::from(stderr))
            .spawn()
            .unwrap_or_else(|error| panic!("spawn {binary} serve --http: {error}"));
        let mut harness = Self {
            client: None,
            child: Some(child),
            _token_file: token_file,
            stderr_file,
        };
        harness.wait_until_ready(addr).await;
        let client = connect(format!("http://{addr}/"), token)
            .await
            .unwrap_or_else(|error| panic!("{error}; server stderr: {}", harness.stderr_text()));
        harness.client = Some(client);
        harness
    }

    pub fn peer(&self) -> Peer<RoleClient> {
        self.client
            .as_ref()
            .expect("live MCP client")
            .peer()
            .clone()
    }

    pub async fn shutdown(mut self) -> Result<(), String> {
        let mut errors = Vec::new();
        if let Some(client) = self.client.take() {
            let peer = client.peer().clone();
            match await_cleanup(
                "glass_stop during cleanup",
                MCP_STOP_BUDGET,
                try_call_full(&peer, "glass_stop", serde_json::json!({})),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) if error.contains("no active session") => {}
                Ok(Err(error)) => {
                    errors.push(format!("glass_stop during cleanup failed: {error}"));
                }
                Err(error) => errors.push(error),
            }
            match await_cleanup(
                "MCP client cancellation",
                MCP_CLIENT_CANCEL_BUDGET,
                client.cancel(),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => errors.push(format!("MCP client cancellation failed: {error}")),
                Err(error) => errors.push(error),
            }
        }
        if let Some(mut child) = self.child.take() {
            if let Err(error) = CommandExt::terminate(&child) {
                errors.push(error);
            }
            if let Err(error) = wait_for_child(&mut child, MCP_SERVER_JOIN_BUDGET).await {
                errors.push(error);
            }
        }
        if !errors.is_empty() {
            errors.push(format!("server stderr: {}", self.stderr_text()));
            return Err(errors.join("; "));
        }
        Ok(())
    }

    async fn wait_until_ready(&mut self, addr: std::net::SocketAddr) {
        let deadline = tokio::time::Instant::now() + PROCESS_START_BUDGET;
        loop {
            if tokio::net::TcpStream::connect(addr).await.is_ok() {
                return;
            }
            if let Some(status) = self
                .child
                .as_mut()
                .expect("server child")
                .try_wait()
                .expect("poll server child")
            {
                panic!(
                    "glass-mcp server exited before accepting HTTP ({status}); stderr: {}",
                    self.stderr_text()
                );
            }
            assert!(
                tokio::time::Instant::now() < deadline,
                "glass-mcp server did not accept HTTP within {PROCESS_START_BUDGET:?}; stderr: {}",
                self.stderr_text()
            );
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    fn stderr_text(&self) -> String {
        std::fs::read_to_string(self.stderr_file.path())
            .unwrap_or_else(|error| format!("<could not read stderr: {error}>"))
    }
}

#[cfg(target_os = "macos")]
impl Drop for ProcessMcpHarness {
    fn drop(&mut self) {
        if let Some(child) = self.child.as_mut() {
            let _ = child.kill();
            let _ = child.wait();
        }
    }
}

#[cfg(target_os = "macos")]
struct CommandExt;

#[cfg(target_os = "macos")]
impl CommandExt {
    fn terminate(child: &std::process::Child) -> Result<(), String> {
        let status = std::process::Command::new("/bin/kill")
            .args(["-TERM", &child.id().to_string()])
            .status()
            .map_err(|error| format!("signal glass-mcp server: {error}"))?;
        status
            .success()
            .then_some(())
            .ok_or_else(|| format!("/bin/kill -TERM {} exited {status}", child.id()))
    }
}

#[cfg(target_os = "macos")]
async fn wait_for_child(child: &mut std::process::Child, budget: Duration) -> Result<(), String> {
    let deadline = tokio::time::Instant::now() + budget;
    loop {
        if child
            .try_wait()
            .map_err(|error| format!("poll glass-mcp server exit: {error}"))?
            .is_some()
        {
            return Ok(());
        }
        if tokio::time::Instant::now() >= deadline {
            let _ = child.kill();
            let _ = child.wait();
            return Err(format!(
                "glass-mcp server exit exceeded {budget:?}; killed it"
            ));
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn envelope_parser_ignores_other_tools_and_app_text() {
        assert_eq!(
            successful_envelope_result(
                r#"{"ok":true,"tool":"glass_do","result":{"status":"completed"}}"#,
                "glass_do"
            ),
            Some(serde_json::json!({"status": "completed"}))
        );
        assert_eq!(
            successful_envelope_result(
                r#"{"ok":true,"tool":"glass_start","result":{"status":"completed"}}"#,
                "glass_do"
            ),
            None
        );
        assert_eq!(
            successful_envelope_result("fixture says completed", "glass_do"),
            None
        );
    }
}
