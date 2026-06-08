//! End-to-end MCP smoke test over stdio. #[ignore]d (needs Xvfb); run via:
//!   cargo test -p glass-mcp --test smoke -- --ignored

mod common;

use std::io::{BufRead, BufReader, Write};
use std::process::{Command, Stdio};

use common::Xvfb;

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

fn send(stdin: &mut impl Write, msg: &serde_json::Value) {
    stdin.write_all(msg.to_string().as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
}

fn read_response(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    let mut line = String::new();
    for _ in 0..100 {
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

#[test]
#[ignore = "requires Xvfb; run via: cargo test -p glass-mcp --test smoke -- --ignored"]
fn initialize_list_tools_and_call_stop() {
    let xvfb = Xvfb::start();
    let mut child = Command::new(SERVER)
        .env("DISPLAY", &xvfb.display)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn glass-mcp");
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(&mut stdin, &serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": {
            "protocolVersion": "2024-11-05",
            "capabilities": {},
            "clientInfo": { "name": "glass-smoke", "version": "0" }
        }
    }));
    let init = read_response(&mut stdout, 1);
    assert!(init.get("result").is_some(), "initialize failed: {init}");

    send(&mut stdin, &serde_json::json!({
        "jsonrpc": "2.0", "method": "notifications/initialized"
    }));

    send(&mut stdin, &serde_json::json!({
        "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}
    }));
    let tools = read_response(&mut stdout, 2);
    let names: Vec<String> = tools["result"]["tools"]
        .as_array()
        .expect("tools array")
        .iter()
        .map(|t| t["name"].as_str().unwrap_or("").to_string())
        .collect();
    for expected in ["glass_start", "glass_screenshot", "glass_click", "glass_stop",
                     "glass_list_windows", "glass_select_window"] {
        assert!(names.iter().any(|n| n == expected), "missing tool {expected}; got {names:?}");
    }

    send(&mut stdin, &serde_json::json!({
        "jsonrpc": "2.0", "id": 3, "method": "tools/call",
        "params": { "name": "glass_stop", "arguments": {} }
    }));
    let call = read_response(&mut stdout, 3);
    let result = &call["result"];
    assert_eq!(result["isError"].as_bool(), Some(true), "expected error result: {call}");
    let text = result["content"][0]["text"].as_str().unwrap_or("");
    assert!(text.contains("no active session"), "unexpected message: {text}");

    let _ = child.kill();
    let _ = child.wait();
}
