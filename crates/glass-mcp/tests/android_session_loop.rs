//! The Android click path as a user reaches it: a session whose backend comes from the real
//! `make_platform` factory, so the reader it selects is the one under test. Ignored; run with a
//! booted AVD + the built APKs (both come from the glass-android-agent repo:
//! `./gradlew :a11y:assembleDebug :fixture-compose:assembleDebug`):
//!   GLASS_ADB=/path/to/platform-tools/adb \
//!   GLASS_ANDROID_A11Y_APK=/path/to/a11y-debug.apk \
//!   GLASS_ANDROID_FIXTURE_APK=/path/to/fixture-compose-debug.apk \
//!     cargo test -p glass-mcp --test android_session_loop -- --ignored --nocapture

use std::time::Duration;

use glass_android::{A11yServiceRegistry, AgentRegistry, AndroidPlatform, EmulatorRegistry};
use glass_core::Deadline;
use glass_core::accessibility::{AxNode, AxTree, ClickMethod};
use glass_core::{AppSpec, BaselineStore, Glass, PlatformFactory, SandboxLevel};
use glass_mcp::serve::config::ServeConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::{Value, json};
use tokio::task::JoinHandle;
use tokio_util::sync::CancellationToken;

/// Ceiling on the wait for the fixture's counter to reflect the click — the poll returns as soon
/// as it changes.
const AWAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
const AWAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
const MCP_CLIENT_CANCEL_BUDGET: Duration = Duration::from_secs(2);
// Allow 8s for both 3s server cleanup phases plus scheduling and transport cancellation.
const MCP_SERVER_JOIN_BUDGET: Duration = Duration::from_secs(8);
static ANDROID_DEVICE_TEST: std::sync::Mutex<()> = std::sync::Mutex::new(());

fn lock_android_device() -> std::sync::MutexGuard<'static, ()> {
    ANDROID_DEVICE_TEST
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
}

/// Build `glass_mcp::boot`'s session factory with registries owned by the cleanup guard.
fn session_glass(device: &Companions) -> Glass {
    // On macOS, `make_platform` also captures the iOS registry whose cloned handle lets `device`
    // shut down shared state.
    #[cfg(target_os = "macos")]
    let sim = glass_ios::SimulatorRegistry::new();
    #[cfg(target_os = "macos")]
    let factory: PlatformFactory = {
        let (emulators, agents, a11y) = (
            device.emulators.clone(),
            device.agents.clone(),
            device.a11y.clone(),
        );
        Box::new(move |b| glass_mcp::make_platform(b, &emulators, &agents, &a11y, &sim))
    };
    #[cfg(not(target_os = "macos"))]
    let factory: PlatformFactory = {
        let (emulators, agents, a11y) = (
            device.emulators.clone(),
            device.agents.clone(),
            device.a11y.clone(),
        );
        Box::new(move |b| glass_mcp::make_platform(b, &emulators, &agents, &a11y))
    };

    let baselines = tempfile::tempdir()
        .expect("a temp dir for the baseline store")
        .keep();
    Glass::new(
        factory,
        "android".to_string(),
        BaselineStore::new(&baselines),
        10_000,
    )
}

/// An HTTP MCP client whose server task is gracefully shut down even after a proof panic.
struct McpHarness {
    client: RunningService<RoleClient, ()>,
    cancel: CancellationToken,
    server: JoinHandle<anyhow::Result<()>>,
}

impl McpHarness {
    fn peer(&self) -> Peer<RoleClient> {
        self.client.peer().clone()
    }

    async fn shutdown(self) -> Result<(), String> {
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

async fn await_cleanup<T>(
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

#[tokio::test]
async fn harness_cleanup_wait_is_bounded() {
    let result: Result<(), String> = await_cleanup(
        "persistent test future",
        Duration::ZERO,
        std::future::pending(),
    )
    .await;
    let error = result.unwrap_err();
    assert!(error.contains("persistent test future"));
}

/// Boot a token-protected Streamable HTTP MCP session for a caller-provided Android session.
async fn boot_mcp(glass: Glass) -> McpHarness {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind an ephemeral loopback port");
    let addr = listener.local_addr().expect("read loopback address");
    let report = glass_mcp::audit::report_from_config(None, |_| None);
    let cancel = CancellationToken::new();
    let shutdown = cancel.clone();
    let server = tokio::spawn(async move {
        let cfg = ServeConfig {
            addr,
            token: Some("android-loop".into()),
        };
        glass_mcp::serve::run_on_until(listener, cfg, glass, report, async move {
            shutdown.cancelled().await;
        })
        .await
    });
    tokio::time::sleep(Duration::from_millis(50)).await;

    let mut cfg = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/"));
    cfg = cfg.auth_header("android-loop".to_string());
    let client = match ().serve(StreamableHttpClientTransport::from_config(cfg)).await {
        Ok(client) => client,
        Err(error) => {
            cancel.cancel();
            match await_server(server, "MCP server startup cleanup").await {
                Ok(()) => panic!("initialize Android MCP client: {error}"),
                Err(cleanup) => panic!(
                    "initialize Android MCP client: {error}; startup cleanup also failed: {cleanup}"
                ),
            }
        }
    };
    McpHarness {
        client,
        cancel,
        server,
    }
}

/// Parse only the requested tool's complete trusted success envelope, never app-derived siblings.
fn successful_envelope_result(text: &str, tool: &str) -> Option<Value> {
    let envelope = serde_json::from_str::<Value>(text).ok()?;
    (envelope.get("ok") == Some(&Value::Bool(true))
        && envelope.get("tool") == Some(&Value::String(tool.to_string())))
    .then(|| envelope.get("result").cloned())
    .flatten()
}

async fn call(client: &Peer<RoleClient>, tool: &str, args: Value) -> (Value, String) {
    let arguments = args
        .as_object()
        .expect("tool args must be a JSON object")
        .clone();
    let response = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("{tool} transport failure: {e}"));
    let mut result = Value::Null;
    let mut all_text = String::new();
    for block in &response.content {
        if let Some(text) = block.as_text() {
            all_text.push_str(&text.text);
            all_text.push('\n');
            if let Some(envelope_result) = successful_envelope_result(&text.text, tool) {
                result = envelope_result;
            }
        }
    }
    assert_ne!(response.is_error, Some(true), "{tool} errored: {all_text}");
    assert_ne!(
        result,
        Value::Null,
        "{tool} lacked a trusted result: {all_text}"
    );
    (result, all_text)
}

#[derive(Debug)]
struct OutlineNode<'a> {
    id: u32,
    indent: usize,
    shape: &'a str,
}

fn outline_nodes(outline: &str) -> Vec<OutlineNode<'_>> {
    outline
        .lines()
        .filter_map(|line| {
            let trimmed = line.trim_start();
            let id_and_shape = trimmed.strip_prefix('#')?;
            let (id, shape) = id_and_shape.split_once(char::is_whitespace)?;
            Some(OutlineNode {
                id: id.parse().ok()?,
                indent: line.len() - trimmed.len(),
                shape: shape.trim_start(),
            })
        })
        .collect()
}

fn unique_outline_id(
    outline: &str,
    label: &str,
    candidates: impl Iterator<Item = u32>,
) -> Result<u32, String> {
    let candidates = candidates.collect::<Vec<_>>();
    match candidates.as_slice() {
        [id] => Ok(*id),
        _ => Err(format!(
            "expected exactly one {label} candidate, found {}:\n{outline}",
            candidates.len()
        )),
    }
}

fn name_id_from_outline_result(outline: &str) -> Result<u32, String> {
    let nodes = outline_nodes(outline);
    unique_outline_id(
        outline,
        "TextField \"Name\"",
        nodes
            .iter()
            .filter(|node| node.shape.starts_with("TextField \"Name\" value="))
            .map(|node| node.id),
    )
}

fn name_id_from_outline(outline: &str) -> u32 {
    name_id_from_outline_result(outline).unwrap_or_else(|error| panic!("{error}"))
}

fn save_button_id_from_outline_result(outline: &str) -> Result<u32, String> {
    let nodes = outline_nodes(outline);
    unique_outline_id(
        outline,
        "Save Button",
        nodes.iter().enumerate().filter_map(|(index, node)| {
            let previous = index.checked_sub(1).and_then(|index| nodes.get(index))?;
            (node.shape.starts_with("Button (")
                && previous.indent == node.indent
                && previous.shape.starts_with("Label \"Save\" ("))
            .then_some(node.id)
        }),
    )
}

fn save_button_id_from_outline(outline: &str) -> u32 {
    save_button_id_from_outline_result(outline).unwrap_or_else(|error| panic!("{error}"))
}

/// The registries whose `ensure` switches something on for the whole device, put back when this
/// goes out of scope — a trailing `shutdown()` is skipped by a panic. Only `shutdown` restores
/// the secure settings and removes the `adb forward` each one opened; the APK stays installed,
/// as glass owns that package (glass#419).
///
/// A guard, not a block around the assertions: `Glass::start` runs the factory — which enables
/// the companion — before `start_app`, so a failing launch panics with the state already on.
struct Companions {
    agents: AgentRegistry,
    a11y: A11yServiceRegistry,
    emulators: EmulatorRegistry,
}

impl Drop for Companions {
    fn drop(&mut self) {
        // Reached while a panic unwinds, where a second panic aborts the process — nothing here
        // may assert.
        self.agents.shutdown(Deadline::UNBOUNDED);
        self.a11y.shutdown(Deadline::UNBOUNDED);
        // A no-op unless glass booted the emulator rather than attaching to one.
        self.emulators.kill_all(Deadline::UNBOUNDED);
    }
}

/// glass#287: the glass-android device tests build `ServiceA11y` by hand, so none of them can see
/// which reader a session would have picked — and picking the uiautomator one makes every click a
/// pointer tap that still reports `Ok`.
#[test]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK + GLASS_ANDROID_FIXTURE_APK"]
fn a_session_click_reports_the_native_accessibility_action() {
    let _device_lock = lock_android_device();
    // An unset APK path is itself the degradation this test detects, so it has to fail as
    // misconfiguration rather than as a verdict.
    std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    let fixture =
        std::env::var("GLASS_ANDROID_FIXTURE_APK").expect("set GLASS_ANDROID_FIXTURE_APK");

    // First, so it drops last: the app has to be force-stopped before the companion reading it
    // goes away. Under the default attach-or-boot lifecycle a second `EmulatorRegistry` with no
    // device attached boots a second emulator, so the install below and the factory share one.
    let device = Companions {
        agents: AgentRegistry::new(),
        a11y: A11yServiceRegistry::new(),
        emulators: EmulatorRegistry::new(),
    };

    {
        let p = AndroidPlatform::from_env(&device.emulators, &device.agents)
            .expect("attach to a device");
        p.resolved_adb()
            .run(["install", "-r", "-g", &fixture])
            .expect("install the fixture APK");
    }

    let mut glass = session_glass(&device);
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["com.fixedwidth.glassfixture/.InvokeViewFixtureActivity".to_string()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 10_000,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .expect("launch the view fixture");

    let tree = glass.a11y_snapshot(None).expect("snapshot");
    let before = counter(&tree);
    let save = find(&tree.root, "SaveBtn").expect("the fixture's SaveBtn is present");
    let method = glass.click_element(save.id).expect("click SaveBtn");

    // `Pointer`'s `native_fallback` names the error that sent the click down the synthetic path.
    assert!(
        matches!(method, ClickMethod::NativeAction { .. }),
        "a session click on an enabled button must fire the native action, got {method:?}"
    );

    // `NativeAction` is the session's claim about which path it took; the counter is the
    // fixture's report that something actuated.
    await_change("SaveBtn's click to register", &before, || {
        counter(&glass.a11y_snapshot(None).expect("snapshot"))
    });

    // The user's own path; a panic above skips it and the drops do the same work —
    // `AndroidPlatform` force-stops the app, `Companions` puts the settings back.
    glass.stop().expect("stop the session");
}

/// Prove one batch survives IME relayout across the unique Name field, Save button, and Counter
/// `"Clicked 1"` state.
#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK + GLASS_ANDROID_FIXTURE_APK"]
async fn glass_do_android_ime_form_is_confirmed_end_to_end() {
    let _device_lock = lock_android_device();
    std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    let fixture =
        std::env::var("GLASS_ANDROID_FIXTURE_APK").expect("set GLASS_ANDROID_FIXTURE_APK");
    let device = Companions {
        agents: AgentRegistry::new(),
        a11y: A11yServiceRegistry::new(),
        emulators: EmulatorRegistry::new(),
    };
    let mcp = boot_mcp(session_glass(&device)).await;
    let peer = mcp.peer();
    let proof = tokio::spawn(async move { ime_form_proof(peer, fixture).await });
    let proof = proof.await;
    let cleanup = mcp.shutdown().await;
    match proof {
        Ok(()) => cleanup.expect("Android MCP cleanup failed"),
        Err(proof) => {
            if let Err(cleanup) = cleanup {
                eprintln!("Android MCP cleanup failed after proof panic: {cleanup}");
            }
            std::panic::resume_unwind(proof.into_panic());
        }
    }
}

async fn ime_form_proof(client: Peer<RoleClient>, fixture: String) {
    call(
        &client,
        "glass_start",
        json!({
            "run": [fixture, "com.fixedwidth.glassfixture/.MainActivity"],
            "backend": "android",
            "timeout_ms": 10_000,
        }),
    )
    .await;
    let (_metadata, outline) = call(&client, "glass_a11y_snapshot", json!({})).await;
    let name_id = name_id_from_outline(&outline);
    let save_id = save_button_id_from_outline(&outline);

    let (result, all_text) = call(
        &client,
        "glass_do",
        json!({
            "timeout_ms": 20_000,
            "actions": [
                {"action": "set_value", "id": name_id, "text": "viaBatch"},
                {"action": "wait_for_element", "name": "Name", "value": "viaBatch", "timeout_ms": 5_000},
                {"action": "click_element", "id": save_id},
                {"action": "wait_for_element", "name": "Clicked 1", "description": "Counter", "timeout_ms": 5_000}
            ]
        }),
    )
    .await;

    assert_eq!(result["status"], json!("completed"), "{all_text}");
    assert_eq!(result["executed"], json!(4), "{all_text}");
    assert!(result["elapsed_ms"].is_number(), "{all_text}");
    let steps = result["steps"].as_array().expect("four batch steps");
    assert_eq!(steps.len(), 4, "{all_text}");
    for (index, (step, action)) in steps
        .iter()
        .zip([
            "set_value",
            "wait_for_element",
            "click_element",
            "wait_for_element",
        ])
        .enumerate()
    {
        assert_eq!(step["index"], json!(index), "{all_text}");
        assert_eq!(step["status"], json!("completed"), "{all_text}");
        assert_eq!(step["action"], json!(action), "{all_text}");
    }
    assert_eq!(
        steps[2]["result"]["method"],
        json!("native-action"),
        "{all_text}"
    );

    call(&client, "glass_stop", json!({})).await;
}

#[test]
fn outline_selectors_require_the_observed_unique_control_shapes() {
    let outline = r#"
  #3 TextField "Name" value="" (63,338 735x147) [focusable,enabled,visible,editable]
    #5 Label "Save" (126,522 80x53) [enabled,visible]
    #6 Button (63,496 206x105) [enabled,visible]
"#;
    assert_eq!(name_id_from_outline(outline), 3);
    assert_eq!(save_button_id_from_outline(outline), 6);
}

#[test]
fn outline_selectors_reject_ambiguous_controls() {
    let outline = r#"
  #3 TextField "Name" value="" (63,338 735x147) [focusable,enabled,visible,editable]
  #4 TextField "Name" value="" (63,338 735x147) [focusable,enabled,visible,editable]
"#;
    let error = name_id_from_outline_result(outline).unwrap_err();
    assert!(error.contains("expected exactly one TextField \"Name\""));
    assert!(error.contains(outline.trim()));

    let outline = r#"
    #5 Label "Save" (126,522 80x53) [enabled,visible]
    #6 Button (63,496 206x105) [enabled,visible]
    #7 Label "Save" (126,622 80x53) [enabled,visible]
    #8 Button (63,596 206x105) [enabled,visible]
"#;
    let error = save_button_id_from_outline_result(outline).unwrap_err();
    assert!(error.contains("expected exactly one Save Button"));
    assert!(error.contains(outline.trim()));
}

/// The fixture's click counter, as the a11y tree reports it.
fn counter(tree: &AxTree) -> String {
    find(&tree.root, "Counter")
        .and_then(|n| n.name.clone().or_else(|| n.value.clone()))
        .expect("the fixture's Counter is present")
}

/// The first node in `n`'s subtree whose description or name is `label`.
fn find<'a>(n: &'a AxNode, label: &str) -> Option<&'a AxNode> {
    if n.description.as_deref() == Some(label) || n.name.as_deref() == Some(label) {
        return Some(n);
    }
    n.children.iter().find_map(|c| find(c, label))
}

/// Poll `read` until it differs from `before`. A timeout is a real failure, not a silent pass.
fn await_change(what: &str, before: &str, mut read: impl FnMut() -> String) {
    let start = std::time::Instant::now();
    loop {
        let now = read();
        if now != before {
            return;
        }
        let elapsed = start.elapsed();
        assert!(
            elapsed < AWAIT_DEADLINE,
            "timed out after {elapsed:?} waiting for {what}; still reads {now:?} \
             (started at {before:?})"
        );
        std::thread::sleep(AWAIT_INTERVAL);
    }
}
