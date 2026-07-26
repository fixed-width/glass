//! The real transport: spawn this same binary as a stdio MCP server and speak
//! JSON-RPC to it, exactly as an MCP client would.

use crate::smoke::transport::{CallResult, McpTransport};
use serde_json::Value;
use std::io::{BufRead, BufReader, Write};
use std::process::{Child, ChildStdin, ChildStdout, Command, Stdio};
use std::time::{Duration, Instant};

/// A server that has not answered in this long is treated as hung.
const CALL_TIMEOUT: Duration = Duration::from_secs(120);

#[derive(Debug)]
pub struct StdioClient {
    child: Child,
    stdin: ChildStdin,
    stdout: BufReader<ChildStdout>,
    next_id: i64,
    version: String,
}

impl StdioClient {
    /// Spawn `exe` as a stdio MCP server and complete the initialize handshake.
    pub fn spawn(exe: &std::path::Path, env: &[(&str, &str)]) -> Result<Self, String> {
        let mut cmd = Command::new(exe);
        cmd.stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null());
        for (k, v) in env {
            cmd.env(k, v);
        }
        let mut child = cmd
            .spawn()
            .map_err(|e| format!("could not spawn {}: {e}", exe.display()))?;
        let stdin = child.stdin.take().ok_or("no stdin on the spawned server")?;
        let stdout = BufReader::new(
            child
                .stdout
                .take()
                .ok_or("no stdout on the spawned server")?,
        );
        let mut c = Self {
            child,
            stdin,
            stdout,
            next_id: 0,
            version: crate::VERSION.to_string(),
        };
        c.initialize()?;
        Ok(c)
    }

    fn initialize(&mut self) -> Result<(), String> {
        let init = self.request(
            "initialize",
            serde_json::json!({
                "protocolVersion": "2024-11-05",
                "capabilities": {},
                "clientInfo": { "name": "glass-smoke", "version": crate::VERSION }
            }),
        )?;
        if init.get("result").is_none() {
            return Err(format!("initialize failed: {init}"));
        }
        if let Some(v) = init["result"]["serverInfo"]["version"].as_str() {
            self.version = v.to_string();
        }
        self.notify("notifications/initialized", serde_json::json!({}))
    }

    fn send(&mut self, msg: &Value) -> Result<(), String> {
        let line = format!("{msg}\n");
        self.stdin
            .write_all(line.as_bytes())
            .map_err(|e| format!("write to server: {e}"))?;
        self.stdin
            .flush()
            .map_err(|e| format!("flush to server: {e}"))
    }

    fn notify(&mut self, method: &str, params: Value) -> Result<(), String> {
        self.send(&serde_json::json!({ "jsonrpc": "2.0", "method": method, "params": params }))
    }

    fn request(&mut self, method: &str, params: Value) -> Result<Value, String> {
        self.next_id += 1;
        let id = self.next_id;
        self.send(
            &serde_json::json!({ "jsonrpc": "2.0", "id": id, "method": method, "params": params }),
        )?;
        let deadline = Instant::now() + CALL_TIMEOUT;
        let mut line = String::new();
        loop {
            if Instant::now() > deadline {
                return Err(format!(
                    "{method}: no response within {}s",
                    CALL_TIMEOUT.as_secs()
                ));
            }
            line.clear();
            match self.stdout.read_line(&mut line) {
                Ok(0) => return Err(format!("{method}: server closed stdout")),
                Ok(_) => {}
                Err(e) => return Err(format!("{method}: read from server: {e}")),
            }
            if let Ok(v) = serde_json::from_str::<Value>(line.trim())
                && v.get("id").and_then(Value::as_i64) == Some(id)
            {
                return Ok(v);
            }
        }
    }
}

impl McpTransport for StdioClient {
    fn call(&mut self, tool: &str, args: Value) -> Result<CallResult, String> {
        let v = self.request(
            "tools/call",
            serde_json::json!({ "name": tool, "arguments": args }),
        )?;
        if let Some(err) = v.get("error") {
            return Err(format!("{tool}: JSON-RPC error {err}"));
        }
        Ok(CallResult::from_mcp(&v["result"]))
    }

    fn server_version(&mut self) -> Result<String, String> {
        Ok(self.version.clone())
    }
}

impl Drop for StdioClient {
    fn drop(&mut self) {
        let _ = self.child.kill();
        let _ = self.child.wait();
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn spawning_a_nonexistent_binary_reports_the_path() {
        let err =
            StdioClient::spawn(std::path::Path::new("/nonexistent/glass-mcp"), &[]).unwrap_err();
        assert!(
            err.contains("/nonexistent/glass-mcp"),
            "must name the path: {err}"
        );
    }
}
