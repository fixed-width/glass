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
//! ./examples/ios-fixture/build.sh
//! GLASS_IOS_APP="$PWD/examples/ios-fixture/build/GlassFixture.app" \
//!   cargo test -p glass-mcp --test apple_session_loop \
//!   glass_do_ios_click_is_semantically_confirmed_with_terminal_screenshot \
//!   -- --ignored --nocapture --test-threads=1
//! ```

#![cfg(target_os = "macos")]

mod common;

use std::path::{Path, PathBuf};
use std::process::Command;
use std::time::{Duration, Instant};

use common::mcp_http::{CallView, ImageView, ProcessMcpHarness, call, call_full};
use rmcp::{Peer, RoleClient};
use serde_json::json;

static APPLE_ONBOX_TEST: tokio::sync::Mutex<()> = tokio::sync::Mutex::const_new(());
const TREE_DEADLINE: Duration = Duration::from_secs(10);

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
#[ignore = "on-box only: needs macOS + Xcode + a booted iOS Simulator + idb_companion + GLASS_IOS_APP"]
async fn glass_do_ios_click_is_semantically_confirmed_with_terminal_screenshot() {
    let _serial = APPLE_ONBOX_TEST.lock().await;
    let fixture = std::env::var("GLASS_IOS_APP")
        .expect("GLASS_IOS_APP must point at the built examples/ios-fixture .app bundle");
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

    let enable_id = wait_for_named_id(&client, "Enable").await;
    let batch = call_full(
        &client,
        "glass_do",
        json!({
            "timeout_ms": 15_000,
            "actions": [
                {"action": "click_element", "id": enable_id},
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
    assert_completed_batch_with_screenshot(&batch, "macos");

    let (semantic, text) = call(
        &client,
        "glass_wait_for_element",
        json!({"name": "Enable", "condition": "checked", "timeout_ms": 1_000}),
    )
    .await;
    assert_eq!(semantic["matched"], json!(true), "{text}");
}

async fn ios_proof(client: Peer<RoleClient>, fixture: String) {
    call(
        &client,
        "glass_start",
        json!({
            "run": [fixture],
            "backend": "ios",
            "sandbox": "off",
            "timeout_ms": 30_000,
        }),
    )
    .await;

    let tap_id = wait_for_named_id(&client, "tapButton").await;
    let batch = call_full(
        &client,
        "glass_do",
        json!({
            "timeout_ms": 20_000,
            "actions": [
                {"action": "click_element", "id": tap_id},
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
    assert_completed_batch_with_screenshot(&batch, "ios");

    let (semantic, text) = call(
        &client,
        "glass_wait_for_element",
        json!({"name": "statusLabel", "value": "TAPPED", "timeout_ms": 1_000}),
    )
    .await;
    assert_eq!(semantic["matched"], json!(true), "{text}");
}

async fn wait_for_named_id(client: &Peer<RoleClient>, name: &str) -> u32 {
    let deadline = Instant::now() + TREE_DEADLINE;
    loop {
        let (_, outline) = call(client, "glass_a11y_snapshot", json!({})).await;
        if let Ok(id) = unique_named_id(&outline, name) {
            return id;
        }
        assert!(
            Instant::now() < deadline,
            "timed out waiting for unique element {name:?}:\n{outline}"
        );
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
}

fn unique_named_id(outline: &str, name: &str) -> Result<u32, String> {
    let needle = format!("\"{name}\"");
    let ids = outline
        .lines()
        .filter_map(|line| {
            let body = line.trim_start().strip_prefix('#')?;
            let (id, shape) = body.split_once(char::is_whitespace)?;
            shape
                .contains(&needle)
                .then(|| id.parse::<u32>().ok())
                .flatten()
        })
        .collect::<Vec<_>>();
    match ids.as_slice() {
        [id] => Ok(*id),
        _ => Err(format!(
            "expected one element named {name:?}, found {}:\n{outline}",
            ids.len()
        )),
    }
}

fn assert_completed_batch_with_screenshot(call: &CallView, backend: &str) {
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
    assert_real_webp(&call.images[0], backend);
}

fn assert_real_webp(image: &ImageView, backend: &str) {
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
    let root = PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-mcp")
        .to_path_buf();
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

#[test]
fn named_outline_selector_requires_exactly_one_public_id() {
    let outline = r#"
  #3 CheckBox "Enable" (20,40 120x24) [enabled,visible,checkable]
  #4 Button "Other" (20,80 80x24) [enabled,visible]
"#;
    assert_eq!(unique_named_id(outline, "Enable").unwrap(), 3);
    assert!(unique_named_id(outline, "Missing").is_err());
}
