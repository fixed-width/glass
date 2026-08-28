//! On-box public-MCP acceptance smoke tests for the Apple platform factories. Both tests drive a
//! real `glass-mcp serve --http` child through rmcp's Streamable HTTP client, so `glass_start`
//! reaches the target-specific `make_platform` arm rather than a fake `Platform`.
//!
//! macOS, with Screen Recording + Accessibility granted to the launched server:
//!
//! ```sh
//! cargo test -p glass-mcp --test apple_session_loop \
//!   glass_do_macos_click_is_semantically_confirmed_with_terminal_screenshot \
//!   -- --ignored --nocapture --test-threads=1
//! ```
//!
//! iOS Simulator, with a booted Simulator and `idb_companion` on `PATH`:
//!
//! ```sh
//! cargo test -p glass-mcp --test apple_session_loop \
//!   glass_do_ios_click_is_semantically_confirmed_with_terminal_screenshot \
//!   -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(target_os = "macos")]

#[path = "common/apple_smoke.rs"]
mod apple_smoke;
mod common;

use std::fs;
use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use apple_smoke::{ElementRoi, assert_roi_changed, unique_named_element};
use common::mcp_http::{CallView, ImageView, ProcessMcpHarness, call, call_full};
use image::RgbaImage;
use rmcp::{Peer, RoleClient};
use serde_json::json;

static APPLE_ONBOX_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const TREE_DEADLINE: Duration = Duration::from_secs(10);
const PIXEL_DELTA_TOLERANCE: u8 = 24;

#[tokio::test(flavor = "multi_thread")]
#[ignore = "on-box only: needs macOS Screen Recording + Accessibility grants and swiftc"]
async fn glass_do_macos_click_is_semantically_confirmed_with_terminal_screenshot() {
    let _serial = APPLE_ONBOX_TEST.lock().await;
    let fixture = build_macos_fixture();
    let mcp = ProcessMcpHarness::spawn(env!("CARGO_BIN_EXE_glass-mcp"), "macos-loop").await;
    let peer = mcp.peer();
    let proof = tokio::spawn(async move { macos_proof(peer, fixture.path()).await }).await;
    finish_proof(mcp, proof).await;
}

#[tokio::test(flavor = "multi_thread")]
#[ignore = "on-box only: needs macOS + Xcode + a booted iOS Simulator + idb_companion"]
async fn glass_do_ios_click_is_semantically_confirmed_with_terminal_screenshot() {
    let _serial = APPLE_ONBOX_TEST.lock().await;
    let fixture = build_ios_fixture();
    let mcp = ProcessMcpHarness::spawn(env!("CARGO_BIN_EXE_glass-mcp"), "ios-loop").await;
    let peer = mcp.peer();
    let proof = tokio::spawn(async move { ios_proof(peer, fixture).await }).await;
    finish_proof(mcp, proof).await;
}

async fn macos_proof(client: Peer<RoleClient>, fixture: &Path) {
    call(
        &client,
        "glass_start",
        json!({
            "run": [fixture.to_string_lossy()],
            "backend": "macos",
            "sandbox": "off",
            "window_hint": {"title": "glass a11y fixture"},
            "timeout_ms": 10_000,
        }),
    )
    .await;

    let enable = wait_for_named_element(&client, "Enable").await;
    assert_semantic_state(
        &client,
        json!({"name": "Enable", "condition": "unchecked", "timeout_ms": 1_000}),
        "macOS Enable must start unchecked",
    )
    .await;
    let before = capture_pre_action_roi(&client, enable, "macOS unchecked Enable").await;
    let batch = call_full(
        &client,
        "glass_do",
        json!({
            "timeout_ms": 15_000,
            "actions": [
                {"action": "click_element", "id": enable.id},
                {
                    "action": "wait_for_element",
                    "name": "Enable",
                    "condition": "checked",
                    "timeout_ms": 5_000
                }
            ],
            "then": {"screenshot": {}}
        }),
    )
    .await;
    let after = completed_terminal_screenshot(&batch, "macos");
    assert_post_action_pixels(&before, &after, enable, "macOS Enable unchecked→checked");

    let (semantic, text) = call(
        &client,
        "glass_wait_for_element",
        json!({"name": "Enable", "condition": "checked", "timeout_ms": 1_000}),
    )
    .await;
    assert_eq!(semantic["matched"], json!(true), "{text}");
}

async fn ios_proof(client: Peer<RoleClient>, fixture: PathBuf) {
    call(
        &client,
        "glass_start",
        json!({
            "run": [fixture.to_string_lossy()],
            "backend": "ios",
            "sandbox": "off",
            "timeout_ms": 30_000,
        }),
    )
    .await;

    let status = wait_for_named_element(&client, "statusLabel").await;
    assert_semantic_state(
        &client,
        json!({"name": "statusLabel", "value": "READY", "timeout_ms": 1_000}),
        "iOS statusLabel must start READY",
    )
    .await;
    let before = capture_pre_action_roi(&client, status, "iOS READY statusLabel").await;
    let tap = wait_for_named_element(&client, "tapButton").await;
    let batch = call_full(
        &client,
        "glass_do",
        json!({
            "timeout_ms": 20_000,
            "actions": [
                {"action": "click_element", "id": tap.id},
                {
                    "action": "wait_for_element",
                    "name": "statusLabel",
                    "value": "TAPPED",
                    "timeout_ms": 5_000
                }
            ],
            "then": {"screenshot": {}}
        }),
    )
    .await;
    let after = completed_terminal_screenshot(&batch, "ios");
    assert_post_action_pixels(&before, &after, status, "iOS statusLabel READY→TAPPED");

    let (semantic, text) = call(
        &client,
        "glass_wait_for_element",
        json!({"name": "statusLabel", "value": "TAPPED", "timeout_ms": 1_000}),
    )
    .await;
    assert_eq!(semantic["matched"], json!(true), "{text}");
}

async fn wait_for_named_element(client: &Peer<RoleClient>, name: &str) -> ElementRoi {
    let deadline = Instant::now() + TREE_DEADLINE;
    loop {
        let (_, outline) = call(client, "glass_a11y_snapshot", json!({})).await;
        if let Ok(element) = unique_named_element(&outline, name) {
            return element;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for unique element {name:?}:\n{outline}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

async fn assert_semantic_state(client: &Peer<RoleClient>, args: serde_json::Value, label: &str) {
    let (semantic, text) = call(client, "glass_wait_for_element", args).await;
    assert_eq!(semantic["matched"], json!(true), "{label}: {text}");
}

async fn capture_pre_action_roi(
    client: &Peer<RoleClient>,
    roi: ElementRoi,
    label: &str,
) -> RgbaImage {
    let call = call_full(
        client,
        "glass_screenshot",
        json!({
            "region": {
                "x": roi.x,
                "y": roi.y,
                "width": roi.width,
                "height": roi.height,
            }
        }),
    )
    .await;
    assert_eq!(
        call.result["width"],
        json!(roi.width),
        "{label}: {}",
        call.all_text
    );
    assert_eq!(
        call.result["height"],
        json!(roi.height),
        "{label}: {}",
        call.all_text
    );
    assert_eq!(call.images.len(), 1, "{label}: {}", call.all_text);
    assert_eq!(call.images[0].index, 0, "{label}: {}", call.all_text);
    decode_real_webp(&call.images[0], label)
}

fn assert_post_action_pixels(
    before_roi: &RgbaImage,
    terminal_frame: &RgbaImage,
    roi: ElementRoi,
    label: &str,
) {
    let area = u64::from(roi.width) * u64::from(roi.height);
    let minimum_changed_pixels = (area / 1_000).max(8);
    assert_roi_changed(
        before_roi,
        terminal_frame,
        roi,
        PIXEL_DELTA_TOLERANCE,
        minimum_changed_pixels,
        label,
    )
    .unwrap_or_else(|error| panic!("{error}"));
}

fn completed_terminal_screenshot(call: &CallView, backend: &str) -> RgbaImage {
    assert_eq!(
        call.result["status"],
        json!("completed"),
        "{}",
        call.all_text
    );
    assert_eq!(call.result["executed"], json!(2), "{}", call.all_text);
    let steps = call.result["steps"].as_array().expect("two action steps");
    assert_eq!(steps.len(), 2, "{}", call.all_text);
    assert_eq!(
        steps[0]["action"],
        json!("click_element"),
        "{}",
        call.all_text
    );
    assert_eq!(steps[0]["status"], json!("completed"), "{}", call.all_text);
    assert!(
        steps[0]["result"]["method"].is_string(),
        "{}",
        call.all_text
    );
    assert_eq!(
        steps[1]["action"],
        json!("wait_for_element"),
        "{}",
        call.all_text
    );
    assert_eq!(steps[1]["status"], json!("completed"), "{}", call.all_text);
    assert_eq!(
        steps[1]["result"]["matched"],
        json!(true),
        "{}",
        call.all_text
    );

    let terminal = call.result["terminal_steps"]
        .as_array()
        .expect("one terminal screenshot step");
    assert_eq!(terminal.len(), 1, "{}", call.all_text);
    assert_eq!(
        terminal[0]["operation"],
        json!("screenshot"),
        "{}",
        call.all_text
    );
    assert_eq!(
        terminal[0]["status"],
        json!("completed"),
        "{}",
        call.all_text
    );
    assert_eq!(
        terminal[0]["content_blocks"],
        json!([1, 2]),
        "{}",
        call.all_text
    );
    assert_eq!(call.images.len(), 1, "{backend}: {}", call.all_text);
    assert_eq!(call.images[0].index, 1, "{backend}: {}", call.all_text);
    decode_real_webp(&call.images[0], backend)
}

fn decode_real_webp(image: &ImageView, backend: &str) -> RgbaImage {
    assert_eq!(image.mime_type, "image/webp", "{backend}: wrong image MIME");
    let bytes = image
        .decode()
        .expect("terminal screenshot base64 must decode");
    let frame = image::load_from_memory(&bytes)
        .unwrap_or_else(|error| {
            panic!("{backend}: terminal screenshot is not a real WebP: {error}")
        })
        .to_rgba8();
    assert!(
        frame.width() > 0 && frame.height() > 0,
        "{backend}: terminal screenshot dimensions must be non-zero"
    );
    let first = frame.get_pixel(0, 0);
    assert!(
        frame.pixels().any(|pixel| pixel != first),
        "{backend}: terminal screenshot is uniform/blank"
    );
    frame
}

async fn finish_proof(mcp: ProcessMcpHarness, proof: Result<(), tokio::task::JoinError>) {
    let cleanup = mcp.shutdown().await;
    match proof {
        Ok(()) => cleanup.expect("Apple MCP cleanup failed"),
        Err(proof) => {
            if let Err(cleanup) = cleanup {
                eprintln!("Apple MCP cleanup failed after proof panic: {cleanup}");
            }
            std::panic::resume_unwind(proof.into_panic());
        }
    }
}

struct MacosFixture {
    _dir: tempfile::TempDir,
    binary: PathBuf,
}

impl MacosFixture {
    fn path(&self) -> &Path {
        &self.binary
    }
}

fn build_macos_fixture() -> MacosFixture {
    let root = repo_root();
    let source = root.join("crates/glass-macos/fixture/a11y_fixture.swift");
    let dir = tempfile::tempdir().expect("create macOS fixture build directory");
    let binary = dir.path().join("a11y_fixture");
    let status = Command::new("swiftc")
        .arg("-O")
        .arg("-parse-as-library")
        .arg(&source)
        .arg("-o")
        .arg(&binary)
        .status()
        .expect("run swiftc for macOS a11y fixture");
    assert!(
        status.success(),
        "swiftc failed building {}",
        source.display()
    );
    MacosFixture { _dir: dir, binary }
}

fn build_ios_fixture() -> PathBuf {
    let fixture = repo_root().join("examples/ios-fixture");
    let script = fixture.join("build.sh");
    let status = Command::new(&script)
        .current_dir(&fixture)
        .status()
        .unwrap_or_else(|error| panic!("run {}: {error}", script.display()));
    assert!(status.success(), "{} failed", script.display());

    let app = fixture.join("build/GlassFixture.app");
    let built_info = app.join("Info.plist");
    let source_info = fixture.join("Info.plist");
    assert_eq!(
        fs::read(&built_info)
            .unwrap_or_else(|error| panic!("read {}: {error}", built_info.display())),
        fs::read(&source_info)
            .unwrap_or_else(|error| panic!("read {}: {error}", source_info.display())),
        "built iOS fixture metadata must be copied from the repository fixture"
    );
    let executable = app.join("GlassFixture");
    assert!(
        executable.is_file(),
        "missing built fixture executable {}",
        executable.display()
    );
    app
}

fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-mcp")
        .to_path_buf()
}
