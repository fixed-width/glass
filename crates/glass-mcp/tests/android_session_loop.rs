//! The Android click path as a user reaches it: a session whose backend comes from the real
//! `make_platform` factory, so the reader it selects is the one under test. Ignored; run with a
//! booted AVD + the built APKs (both come from the glass-android-agent repo:
//! `./gradlew :a11y:assembleDebug :fixture-compose:assembleDebug`):
//!   GLASS_ADB=/path/to/platform-tools/adb \
//!   GLASS_ANDROID_A11Y_APK=/path/to/a11y-debug.apk \
//!   GLASS_ANDROID_FIXTURE_APK=/path/to/fixture-compose-debug.apk \
//!     cargo test -p glass-mcp --test android_session_loop -- --ignored --nocapture

mod common;

use std::time::Duration;

use common::mcp_http::{InProcessMcpHarness, await_cleanup, call, try_call_full};
use glass_android::{
    A11yServiceRegistry, AgentRegistry, AndroidA11y, AndroidPlatform, EmulatorRegistry,
};
use glass_core::Deadline;
use glass_core::accessibility::{AxNode, AxTree, ClickMethod};
use glass_core::{
    AppSpec, Backend, BaselineStore, Glass, GlassError, PlatformFactory, SandboxLevel,
};
use rmcp::{Peer, RoleClient};
use serde_json::json;

/// Ceiling on the wait for the fixture's counter to reflect the click — the poll returns as soon
/// as it changes.
const AWAIT_DEADLINE: std::time::Duration = std::time::Duration::from_secs(5);
const AWAIT_INTERVAL: std::time::Duration = std::time::Duration::from_millis(150);
static ANDROID_DEVICE_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());

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

/// The same public MCP/session path with the baseline reader selected explicitly, so its
/// pre-dispatch native-focus refusal and core's automatic pointer fallback are exercised without
/// mutating process-global environment while the HTTP server is running.
fn uiautomator_session_glass(device: &Companions) -> Glass {
    let (emulators, agents) = (device.emulators.clone(), device.agents.clone());
    let factory: PlatformFactory = Box::new(move |backend| {
        if backend != "android" {
            return Err(GlassError::Backend(format!(
                "uiautomator acceptance supports only android, got {backend:?}"
            )));
        }
        let platform = AndroidPlatform::from_env(&emulators, &agents)?;
        let accessibility = AndroidA11y::for_adb(platform.resolved_adb());
        Ok(Backend {
            platform: Box::new(platform),
            accessibility: Some(Box::new(accessibility)),
        })
    });
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

fn outline_line<'a>(outline: &'a str, needle: &str) -> &'a str {
    outline
        .lines()
        .find(|line| line.contains(needle))
        .unwrap_or_else(|| panic!("outline has no {needle:?}:\n{outline}"))
}

fn quoted_value(line: &str, prefix: &str) -> String {
    line.split_once(prefix)
        .and_then(|(_, rest)| rest.split_once('"'))
        .map(|(value, _)| value.to_string())
        .unwrap_or_else(|| panic!("{line:?} has no quoted {prefix:?} value"))
}

fn semantic_status(outline: &str) -> String {
    let line = outline_line(outline, "desc=\"Semantic Status\"");
    line.split('"')
        .nth(1)
        .unwrap_or_else(|| panic!("status line has no name: {line}"))
        .to_string()
}

fn semantic_field_value(outline: &str) -> String {
    quoted_value(
        outline_line(outline, "TextField \"Semantic Field\""),
        "value=\"",
    )
}

fn semantic_focus_status(outline: &str) -> String {
    let line = outline_line(outline, "desc=\"Semantic Focus Status\"");
    line.split('"')
        .nth(1)
        .unwrap_or_else(|| panic!("focus status line has no name: {line}"))
        .to_string()
}

fn node_bounds(outline: &str, needle: &str) -> (i32, i32, u32, u32) {
    let line = outline_line(outline, needle);
    let tuple = line
        .rsplit_once('(')
        .and_then(|(_, rest)| rest.split_once(')'))
        .map(|(tuple, _)| tuple)
        .unwrap_or_else(|| panic!("node has no bounds: {line}"));
    let (position, size) = tuple
        .split_once(' ')
        .unwrap_or_else(|| panic!("node bounds have no size: {line}"));
    let (x, y) = position
        .split_once(',')
        .unwrap_or_else(|| panic!("node bounds have no position: {line}"));
    let (width, height) = size
        .split_once('x')
        .unwrap_or_else(|| panic!("node bounds have no dimensions: {line}"));
    (
        x.parse().expect("bounds x"),
        y.parse().expect("bounds y"),
        width.parse().expect("bounds width"),
        height.parse().expect("bounds height"),
    )
}

fn actionability(result: &serde_json::Value, check: &str) -> String {
    result["actionability"]
        .as_array()
        .and_then(|checks| checks.iter().find(|item| item["check"] == check))
        .and_then(|item| item["verdict"].as_str())
        .unwrap_or_else(|| panic!("missing actionability check {check:?}: {result}"))
        .to_string()
}

async fn snapshot_outline(client: &Peer<RoleClient>) -> String {
    call(client, "glass_a11y_snapshot", json!({})).await.1
}

async fn await_exact_status(client: &Peer<RoleClient>, expected: &str) -> String {
    let start = std::time::Instant::now();
    loop {
        let outline = snapshot_outline(client).await;
        let status = semantic_status(&outline);
        if status == expected {
            tokio::time::sleep(Duration::from_millis(350)).await;
            let quiet = snapshot_outline(client).await;
            assert_eq!(
                semantic_status(&quiet),
                expected,
                "handler count changed during the quiet window"
            );
            return quiet;
        }
        assert!(
            start.elapsed() < Duration::from_secs(5),
            "status did not reach {expected:?}; last was {status:?}\n{outline}"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
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
        "Name TextField",
        nodes
            .iter()
            .filter(|node| {
                (node.shape.starts_with("TextField value=\"\" ")
                    || node.shape.starts_with("TextField \"Name\" value=\"\" "))
                    && node.shape.contains("editable")
            })
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
    let _device_lock = ANDROID_DEVICE_TEST.blocking_lock();
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
    let _device_lock = ANDROID_DEVICE_TEST.lock().await;
    std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    let fixture =
        std::env::var("GLASS_ANDROID_FIXTURE_APK").expect("set GLASS_ANDROID_FIXTURE_APK");
    let device = Companions {
        agents: AgentRegistry::new(),
        a11y: A11yServiceRegistry::new(),
        emulators: EmulatorRegistry::new(),
    };
    let mcp = InProcessMcpHarness::boot(session_glass(&device), "android-loop").await;
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
                {"action": "wait_for_element", "role": "TextField", "value": "viaBatch", "timeout_ms": 5_000},
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
    let (_, after) = call(&client, "glass_a11y_snapshot", json!({})).await;
    let submitted = outline_nodes(&after)
        .into_iter()
        .filter(|node| {
            node.shape.starts_with("TextField value=\"viaBatch\" ")
                && node.shape.contains("editable")
        })
        .count();
    assert_eq!(submitted, 1, "expected one submitted TextField:\n{after}");

    call(&client, "glass_stop", json!({})).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "requires a booted AVD + GLASS_ADB + GLASS_ANDROID_A11Y_APK + GLASS_ANDROID_ROLE_FIXTURE_APK"]
async fn android_semantic_actions_are_conservative_and_exactly_once() {
    let _device_lock = ANDROID_DEVICE_TEST.lock().await;
    std::env::var("GLASS_ANDROID_A11Y_APK").expect("set GLASS_ANDROID_A11Y_APK");
    let fixture = std::env::var("GLASS_ANDROID_ROLE_FIXTURE_APK")
        .expect("set GLASS_ANDROID_ROLE_FIXTURE_APK");
    let component = "tech.fixedwidth.glassrolefixture/.MainActivity";

    let device = Companions {
        agents: AgentRegistry::new(),
        a11y: A11yServiceRegistry::new(),
        emulators: EmulatorRegistry::new(),
    };
    let mcp = InProcessMcpHarness::boot(session_glass(&device), "android-semantic").await;
    let client = mcp.peer();
    call(
        &client,
        "glass_start",
        json!({
            "run": [fixture, component],
            "backend": "android",
            "timeout_ms": 10_000,
        }),
    )
    .await;

    let initial = snapshot_outline(&client).await;
    assert!(
        !initial.contains("PASSWORD_SENTINEL"),
        "protected fixture text crossed the companion boundary: {initial}"
    );
    assert_eq!(
        semantic_status(&initial),
        "save=0 type=0 move=0 duplicate=0"
    );
    let duplicate_count = initial
        .lines()
        .filter(|line| line.contains("Button \"Duplicate semantic\""))
        .count();
    assert_eq!(
        duplicate_count, 2,
        "duplicate fixture must be genuinely ambiguous"
    );

    let native = call(
        &client,
        "glass_click_element",
        json!({
            "target": {"query": "Semantic Save", "role": "Button", "states": ["enabled"]},
            "mode": "native",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .0;
    assert_eq!(native["method"], "native-action");
    assert_eq!(native["dispatch"], "dispatched");
    await_exact_status(&client, "save=1 type=0 move=0 duplicate=0").await;

    let pointer = call(
        &client,
        "glass_click_element",
        json!({
            "target": {
                "query": "Semantic Save",
                "role": "Button",
                "states": ["enabled", "visible"]
            },
            "mode": "pointer",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .0;
    assert_eq!(pointer["method"], "pointer");
    assert_eq!(pointer["dispatch"], "dispatched");
    assert_eq!(actionability(&pointer, "non_occluded"), "unproven");
    await_exact_status(&client, "save=2 type=0 move=0 duplicate=0").await;

    let set = call(
        &client,
        "glass_set_value",
        json!({
            "target": {"query": "Semantic Field", "role": "TextField", "states": ["enabled", "visible"]},
            "text": "replaced",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .0;
    assert_eq!(set["method"], "accessibility-value");
    assert_eq!(set["dispatch"], "dispatched");
    let after_set = await_exact_status(&client, "save=2 type=1 move=0 duplicate=0").await;
    assert_eq!(semantic_field_value(&after_set), "replaced");

    let before_type = semantic_field_value(&after_set);
    let typed = call(
        &client,
        "glass_type",
        json!({
            "target": {"query": "Semantic Field", "role": "TextField", "states": ["enabled", "visible"]},
            "focus_mode": "native",
            "text": "Z",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .0;
    assert_eq!(typed["focus_method"], "native-action");
    assert_eq!(typed["focus_dispatch"], "dispatched");
    assert_eq!(typed["focus_confirmation"], "focus_confirmed");
    assert_eq!(typed["type_dispatch"], "dispatched");
    let after_type = await_exact_status(&client, "save=2 type=2 move=0 duplicate=0").await;
    assert_eq!(
        semantic_focus_status(&after_type),
        "focus_click=1 request=true focused=true",
        "the fixture must observe exactly one native focus handler"
    );
    let after_type_value = semantic_field_value(&after_type);
    assert_eq!(after_type_value.len(), before_type.len() + 1);
    assert_eq!(after_type_value.matches('Z').count(), 1);
    assert_eq!(after_type_value.replace('Z', ""), before_type);

    let before_refusals = semantic_status(&after_type);
    let disabled = try_call_full(
        &client,
        "glass_click_element",
        json!({
            "target": {"query": "Disabled semantic", "role": "Button"},
            "mode": "pointer",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .expect_err("disabled target must be refused");
    assert!(
        disabled.contains("\"code\":\"not_actionable\""),
        "{disabled}"
    );
    assert!(
        disabled.contains("\"dispatch\":\"not_dispatched\""),
        "{disabled}"
    );
    await_exact_status(&client, &before_refusals).await;

    let duplicate = try_call_full(
        &client,
        "glass_click_element",
        json!({
            "target": {"query": "Duplicate semantic", "role": "Button"},
            "mode": "native",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .expect_err("duplicate target must be refused");
    assert!(
        duplicate.contains("\"code\":\"ambiguous_target\""),
        "{duplicate}"
    );
    assert!(
        duplicate.contains("\"dispatch\":\"not_dispatched\""),
        "{duplicate}"
    );
    await_exact_status(&client, &before_refusals).await;

    call(
        &client,
        "glass_click_element",
        json!({
            "target": {"query": "Restart movement", "role": "Button"},
            "mode": "native",
            "timeout_ms": 5_000,
        }),
    )
    .await;
    let movement_started = std::time::Instant::now();
    let mut movement_samples = Vec::new();
    while movement_started.elapsed() < Duration::from_millis(500) {
        let outline = snapshot_outline(&client).await;
        movement_samples.push((
            movement_started.elapsed(),
            node_bounds(&outline, "Moving semantic"),
        ));
        if movement_samples
            .iter()
            .map(|(_, bounds)| bounds)
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1
        {
            break;
        }
    }
    println!("Android 300ms movement samples: {movement_samples:?}");
    assert!(
        movement_samples
            .iter()
            .map(|(_, bounds)| bounds)
            .collect::<std::collections::HashSet<_>>()
            .len()
            > 1,
        "the 300ms fixture motion produced no observed bounds change: {movement_samples:?}"
    );

    call(
        &client,
        "glass_click_element",
        json!({
            "target": {"query": "Restart movement", "role": "Button"},
            "mode": "native",
            "timeout_ms": 5_000,
        }),
    )
    .await;
    let moving = call(
        &client,
        "glass_click_element",
        json!({
            "target": {"query": "Moving semantic", "role": "Button", "states": ["enabled", "visible"]},
            "mode": "pointer",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .0;
    assert_eq!(moving["method"], "pointer");
    assert_eq!(actionability(&moving, "stable"), "passed");
    assert_eq!(actionability(&moving, "non_occluded"), "unproven");
    await_exact_status(&client, "save=2 type=2 move=1 duplicate=0").await;

    let stale_snapshot = snapshot_outline(&client).await;
    let stale_status = outline_nodes(&stale_snapshot)
        .into_iter()
        .find(|node| node.shape.contains("desc=\"Semantic Status\""))
        .expect("semantic status id");
    let (x, y, width, height) = node_bounds(&stale_snapshot, "Button \"Semantic Save\"");
    call(
        &client,
        "glass_click",
        json!({"x": x + i32::try_from(width / 2).unwrap(), "y": y + i32::try_from(height / 2).unwrap()}),
    )
    .await;
    tokio::time::sleep(Duration::from_millis(350)).await;
    let stale = try_call_full(
        &client,
        "glass_click_element",
        json!({"id": stale_status.id, "mode": "native"}),
    )
    .await
    .expect_err("changed status identity must refuse the stale id");
    assert!(stale.contains("\"code\":\"stale_element\""), "{stale}");
    assert!(stale.contains("\"dispatch\":\"not_dispatched\""), "{stale}");
    await_exact_status(&client, "save=3 type=2 move=1 duplicate=0").await;

    call(&client, "glass_stop", json!({})).await;
    mcp.shutdown().await.expect("companion semantic cleanup");

    let fallback =
        InProcessMcpHarness::boot(uiautomator_session_glass(&device), "android-fallback").await;
    let client = fallback.peer();
    call(
        &client,
        "glass_start",
        json!({
            "run": [std::env::var("GLASS_ANDROID_ROLE_FIXTURE_APK").unwrap(), component],
            "backend": "android",
            "timeout_ms": 10_000,
        }),
    )
    .await;
    let before = snapshot_outline(&client).await;
    let before_value = semantic_field_value(&before);
    let typed = call(
        &client,
        "glass_type",
        json!({
            "target": {"query": "Semantic Field", "role": "TextField", "states": ["enabled", "visible"]},
            "focus_mode": "auto",
            "text": "Q",
            "timeout_ms": 30_000,
        }),
    )
    .await
    .0;
    assert_eq!(typed["focus_method"], "pointer");
    assert_eq!(typed["focus_confirmation"], "focus_confirmed");
    assert_eq!(typed["type_dispatch"], "dispatched");
    let after = await_exact_status(&client, "save=0 type=1 move=0 duplicate=0").await;
    let after_value = semantic_field_value(&after);
    assert_eq!(after_value.len(), before_value.len() + 1);
    assert_eq!(after_value.matches('Q').count(), 1);
    assert_eq!(after_value.replace('Q', ""), before_value);
    call(&client, "glass_stop", json!({})).await;
    fallback
        .shutdown()
        .await
        .expect("fallback semantic cleanup");
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "set GLASS_ANDROID_VERIFY_SCHEMA1=1 and point GLASS_ANDROID_A11Y_APK at the released schema-1 APK"]
async fn released_schema_one_companion_falls_back_without_exposing_secure_text() {
    if std::env::var("GLASS_ANDROID_VERIFY_SCHEMA1").as_deref() != Ok("1") {
        eprintln!("schema-1 fallback acceptance not requested");
        return;
    }
    let _device_lock = ANDROID_DEVICE_TEST.lock().await;
    std::env::var("GLASS_ANDROID_A11Y_APK").expect("set the released schema-1 APK");
    let fixture = std::env::var("GLASS_ANDROID_ROLE_FIXTURE_APK")
        .expect("set GLASS_ANDROID_ROLE_FIXTURE_APK");
    let device = Companions {
        agents: AgentRegistry::new(),
        a11y: A11yServiceRegistry::new(),
        emulators: EmulatorRegistry::new(),
    };
    let mcp = InProcessMcpHarness::boot(session_glass(&device), "android-schema-one").await;
    let client = mcp.peer();
    call(
        &client,
        "glass_start",
        json!({
            "run": [fixture, "tech.fixedwidth.glassrolefixture/.MainActivity"],
            "backend": "android",
            "timeout_ms": 10_000,
        }),
    )
    .await;

    let before = snapshot_outline(&client).await;
    assert!(!before.contains("PASSWORD_SENTINEL"), "{before}");
    assert_eq!(semantic_status(&before), "save=0 type=0 move=0 duplicate=0");
    let before_value = semantic_field_value(&before);
    let native = try_call_full(
        &client,
        "glass_type",
        json!({
            "target": {"query": "Semantic Field", "role": "TextField"},
            "focus_mode": "native",
            "text": "NATIVE_SENTINEL",
            "timeout_ms": 5_000,
        }),
    )
    .await
    .expect_err("uiautomator cannot claim native focus");
    assert!(native.contains("unsupported"), "{native}");
    assert!(
        native.contains("\"dispatch\":\"not_dispatched\""),
        "{native}"
    );
    assert!(!native.contains("NATIVE_SENTINEL"), "{native}");
    let unchanged = await_exact_status(&client, "save=0 type=0 move=0 duplicate=0").await;
    assert_eq!(semantic_field_value(&unchanged), before_value);

    let typed = call(
        &client,
        "glass_type",
        json!({
            "target": {"query": "Semantic Field", "role": "TextField"},
            "focus_mode": "auto",
            "text": "Q",
            "timeout_ms": 30_000,
        }),
    )
    .await
    .0;
    assert_eq!(typed["focus_method"], "pointer");
    assert_eq!(typed["focus_confirmation"], "focus_confirmed");
    assert_eq!(typed["type_dispatch"], "dispatched");
    let after = await_exact_status(&client, "save=0 type=1 move=0 duplicate=0").await;
    let after_value = semantic_field_value(&after);
    assert_eq!(after_value.matches('Q').count(), 1);
    assert_eq!(after_value.replace('Q', ""), before_value);

    call(&client, "glass_stop", json!({})).await;
    mcp.shutdown().await.expect("schema-one fallback cleanup");
}

#[test]
fn outline_selectors_require_the_observed_unique_control_shapes() {
    let outline = r#"
  #3 TextField value="" (63,338 735x147) [focusable,enabled,visible,editable]
    #4 Group "Name" (63,338 735x147) [enabled,visible]
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
    assert!(error.contains("expected exactly one Name TextField"));
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
