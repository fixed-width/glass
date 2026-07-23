//! Host-conformance: prove glass's host-facing MCP surface on BOTH transports
//! (a spawned stdio child + an in-process HTTP server), the way a real MCP host
//! consumes it. Each path asserts: `initialize` negotiates a protocol version,
//! `tools/list` advertises the core loop tools, and a tool call returns a
//! decodable non-blank IMAGE content block. Cross-transport parity of the tool
//! set is asserted in `tool_sets_match_across_transports` (Task 3).
//!
//! `#[ignore]d` (needs Xvfb + the testapp); run via `./scripts/test-x11.sh`.

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::Duration;

use base64::Engine;
use common::Xvfb;

const TESTAPP: &str = env!("CARGO_BIN_EXE_glass-testapp");

/// Path to the `glass-mcp` binary. `env!("CARGO_BIN_EXE_glass-mcp")` isn't usable here:
/// Cargo only injects `CARGO_BIN_EXE_<name>` for binaries owned by the package the test
/// itself belongs to, and this test lives in `glass-testapp`, not `glass-mcp`. Every
/// workspace binary lands in the same Cargo output directory, so derive the path from the
/// sibling `glass-testapp` binary Cargo *does* inject the var for.
fn glass_mcp_path() -> std::path::PathBuf {
    std::path::Path::new(TESTAPP)
        .with_file_name(format!("glass-mcp{}", std::env::consts::EXE_SUFFIX))
}

/// The build → see → interact → debug loop tools every host must see. A subset;
/// the full set is compared across transports at runtime in the parity test, not
/// pinned here (the exact count changes as tools are added).
const CORE_TOOLS: &[&str] = &[
    "glass_start",
    "glass_screenshot",
    "glass_click",
    "glass_stop",
    "glass_list_windows",
];

/// Non-emptiness floor for `tools/list` — guards against an empty or truncated
/// listing without pinning the exact count (compared at runtime in the parity test).
const MIN_TOOLS: usize = 20;

/// Assert the listed tool names include the core loop and clear the floor.
fn assert_tool_surface(names: &[String]) {
    for t in CORE_TOOLS {
        assert!(
            names.iter().any(|n| n == t),
            "tools/list missing core tool {t}; got {names:?}"
        );
    }
    assert!(
        names.len() >= MIN_TOOLS,
        "tools/list returned only {} tools (< floor {MIN_TOOLS}): {names:?}",
        names.len()
    );
}

/// Assert base64 WebP decodes to a real, non-blank frame (image content crossed the wire).
fn assert_image_nonblank(webp_b64: &str) {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(webp_b64)
        .expect("screenshot image is valid base64");
    let img = image::load_from_memory(&bytes)
        .expect("screenshot image decodes as WebP")
        .to_rgba8();
    assert!(
        img.width() >= 320 && img.height() >= 240,
        "unexpected screenshot dims {:?}",
        img.dimensions()
    );
    let first = img.get_pixel(0, 0);
    assert!(
        img.pixels().any(|p| p != first),
        "screenshot is uniform/blank — image content did not cross the wire"
    );
}

// ---- raw JSON-RPC over stdio (what an arbitrary, non-rmcp host does) ----

fn send(stdin: &mut impl Write, msg: &serde_json::Value) {
    stdin.write_all(msg.to_string().as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
}

fn read_response(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    let mut line = String::new();
    for _ in 0..1000 {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("server closed stdout before responding to id {id}");
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                return v;
            }
        }
    }
    panic!("no response with id {id}");
}

/// A glass-mcp child driven over stdio with newline-delimited JSON-RPC.
struct StdioServer {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
}

impl StdioServer {
    fn start(display: &str) -> StdioServer {
        // The x11 backend reads GLASS_DISPLAY (never ambient $DISPLAY); set it on the child.
        let mut child = Command::new(glass_mcp_path())
            .env("GLASS_DISPLAY", display)
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::inherit())
            .spawn()
            .expect("spawn glass-mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        StdioServer {
            child,
            stdin,
            stdout,
            next_id: 1,
        }
    }

    fn request(&mut self, method: &str, params: serde_json::Value) -> serde_json::Value {
        let id = self.next_id;
        self.next_id += 1;
        send(
            &mut self.stdin,
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        );
        read_response(&mut self.stdout, id)
    }

    fn notify(&mut self, method: &str) {
        send(
            &mut self.stdin,
            &serde_json::json!({ "jsonrpc": "2.0", "method": method }),
        );
    }

    /// Send initialize + the initialized notification; return the initialize `result`.
    fn initialize(&mut self) -> serde_json::Value {
        let resp = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "glass-host-conformance", "version": "0" }
            }),
        );
        assert!(resp.get("result").is_some(), "initialize failed: {resp}");
        self.notify("notifications/initialized");
        resp["result"].clone()
    }

    fn list_tool_names(&mut self) -> Vec<String> {
        let resp = self.request("tools/list", serde_json::json!({}));
        resp["result"]["tools"]
            .as_array()
            .expect("tools array")
            .iter()
            .map(|t| t["name"].as_str().unwrap_or("").to_string())
            .collect()
    }

    /// Call a tool; return its `result` object.
    fn call(&mut self, name: &str, arguments: serde_json::Value) -> serde_json::Value {
        let resp = self.request(
            "tools/call",
            serde_json::json!({ "name": name, "arguments": arguments }),
        );
        resp["result"].clone()
    }
}

impl Drop for StdioServer {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[test]
#[ignore = "requires Xvfb + the testapp; run via ./scripts/test-x11.sh"]
fn stdio_host_can_initialize_list_tools_and_get_an_image() {
    let xvfb = Xvfb::start();
    let mut srv = StdioServer::start(&xvfb.display);

    let init = srv.initialize();
    let ver = init["protocolVersion"].as_str().unwrap_or("");
    assert!(
        !ver.is_empty(),
        "initialize did not negotiate a protocolVersion: {init}"
    );

    let names = srv.list_tool_names();
    assert_tool_surface(&names);

    let start = srv.call("glass_start", serde_json::json!({ "run": [TESTAPP] }));
    assert_ne!(
        start["isError"].as_bool(),
        Some(true),
        "glass_start returned an error result: {start}"
    );

    // Let the window render (mirrors the proven timing in tests/network.rs).
    std::thread::sleep(Duration::from_millis(300));

    let shot = srv.call("glass_screenshot", serde_json::json!({}));
    assert_ne!(
        shot["isError"].as_bool(),
        Some(true),
        "glass_screenshot returned an error result: {shot}"
    );
    let webp_b64 = shot["content"]
        .as_array()
        .expect("content array")
        .iter()
        .find_map(|c| {
            if c["type"] == "image" {
                c["data"].as_str()
            } else {
                None
            }
        })
        .expect("screenshot returned an image content block");
    assert_image_nonblank(webp_b64);

    let _ = srv.call("glass_stop", serde_json::json!({}));
}
