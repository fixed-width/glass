//! Minimal i3/sway IPC client: enough to read the window tree and run commands
//! (e.g. focus a container). Wire format: magic "i3-ipc", u32 LE payload length,
//! u32 LE message type, then a JSON payload.
use std::io::{Read, Write};
use std::os::unix::net::UnixStream;
use std::path::{Path, PathBuf};

use glass_core::{GlassError, Result};
use serde::Deserialize;

const MAGIC: &[u8; 6] = b"i3-ipc";
const MSG_RUN_COMMAND: u32 = 0;
const MSG_GET_TREE: u32 = 4;

/// One node from `get_tree` we care about (recursive).
#[derive(Debug, Deserialize)]
pub struct Node {
    pub id: i64,
    #[serde(default)]
    pub name: Option<String>,
    #[serde(default)]
    pub app_id: Option<String>,
    #[serde(default)]
    pub focused: bool,
    #[serde(default)]
    pub rect: Rect,
    #[serde(default)]
    pub foreign_toplevel_identifier: Option<String>,
    #[serde(default)]
    pub window_properties: Option<WindowProperties>,
    #[serde(default)]
    pub nodes: Vec<Node>,
    #[serde(default)]
    pub floating_nodes: Vec<Node>,
}

#[derive(Debug, Default, Deserialize)]
pub struct Rect {
    pub x: i32,
    pub y: i32,
    pub width: i32,
    pub height: i32,
}

#[derive(Debug, Deserialize)]
pub struct WindowProperties {
    #[serde(default)]
    pub class: Option<String>,
}

/// A flattened app window: any node with a foreign-toplevel identifier.
pub struct Window {
    pub con_id: i64,
    pub title: Option<String>,
    pub class: Option<String>,
    pub rect: Rect,
    pub focused: bool,
    pub identifier: String,
}

impl Node {
    /// Depth-first collect of windows that have a foreign-toplevel identifier.
    pub fn windows(&self) -> Vec<Window> {
        let mut out = Vec::new();
        self.collect(&mut out);
        out
    }
    fn collect(&self, out: &mut Vec<Window>) {
        if let Some(id) = &self.foreign_toplevel_identifier {
            let class = self
                .app_id
                .clone()
                .or_else(|| self.window_properties.as_ref().and_then(|w| w.class.clone()));
            out.push(Window {
                con_id: self.id,
                title: self.name.clone(),
                class,
                rect: Rect {
                    x: self.rect.x,
                    y: self.rect.y,
                    width: self.rect.width,
                    height: self.rect.height,
                },
                focused: self.focused,
                identifier: id.clone(),
            });
        }
        for c in self.nodes.iter().chain(self.floating_nodes.iter()) {
            c.collect(out);
        }
    }
}

/// A connected sway IPC client.
pub struct Ipc {
    sock: UnixStream,
}

impl Ipc {
    /// Find the `sway-ipc.*.sock` in the private runtime dir and connect.
    pub fn connect(runtime_dir: &Path) -> Result<Ipc> {
        let path = find_ipc_socket(runtime_dir)
            .ok_or_else(|| GlassError::Backend("sway IPC socket not found".into()))?;
        let sock = UnixStream::connect(&path)
            .map_err(|e| GlassError::Backend(format!("connect sway IPC: {e}")))?;
        Ok(Ipc { sock })
    }

    fn request(&mut self, msg_type: u32, payload: &[u8]) -> Result<Vec<u8>> {
        let mut buf = Vec::with_capacity(14 + payload.len());
        buf.extend_from_slice(MAGIC);
        buf.extend_from_slice(&(payload.len() as u32).to_ne_bytes());
        buf.extend_from_slice(&msg_type.to_ne_bytes());
        buf.extend_from_slice(payload);
        self.sock.write_all(&buf).map_err(|e| GlassError::Backend(format!("sway IPC write: {e}")))?;

        let mut header = [0u8; 14];
        self.sock
            .read_exact(&mut header)
            .map_err(|e| GlassError::Backend(format!("sway IPC read: {e}")))?;
        if &header[0..6] != MAGIC {
            return Err(GlassError::Backend("sway IPC bad magic".into()));
        }
        let len = u32::from_ne_bytes(header[6..10].try_into().unwrap()) as usize;
        let mut reply = vec![0u8; len];
        self.sock
            .read_exact(&mut reply)
            .map_err(|e| GlassError::Backend(format!("sway IPC read: {e}")))?;
        Ok(reply)
    }

    /// `get_tree` -> the app windows (those with a foreign-toplevel identifier).
    pub fn windows(&mut self) -> Result<Vec<Window>> {
        let reply = self.request(MSG_GET_TREE, b"")?;
        let root: Node = serde_json::from_slice(&reply)
            .map_err(|e| GlassError::Backend(format!("parse get_tree: {e}")))?;
        Ok(root.windows())
    }

    /// Run a sway command (e.g. `[con_id=N] focus`). Errors if sway reports the
    /// command failed (no silent fallback).
    pub fn run_command(&mut self, cmd: &str) -> Result<()> {
        let reply = self.request(MSG_RUN_COMMAND, cmd.as_bytes())?;
        check_command_reply(&reply, cmd)
    }
}

/// One entry of a `RUN_COMMAND` reply (one per command in the payload).
#[derive(Debug, Deserialize)]
struct CommandOutcome {
    success: bool,
    #[serde(default)]
    error: Option<String>,
}

/// Parse a `RUN_COMMAND` reply (a JSON array of outcomes) and error if any
/// sub-command failed.
fn check_command_reply(reply: &[u8], cmd: &str) -> Result<()> {
    let outcomes: Vec<CommandOutcome> = serde_json::from_slice(reply)
        .map_err(|e| GlassError::Backend(format!("parse run_command reply: {e}")))?;
    for o in outcomes {
        if !o.success {
            return Err(GlassError::Backend(format!(
                "sway command {cmd:?} failed: {}",
                o.error.as_deref().unwrap_or("unknown")
            )));
        }
    }
    Ok(())
}

/// The IPC socket sway creates in `XDG_RUNTIME_DIR`: `sway-ipc.<uid>.<pid>.sock`.
fn find_ipc_socket(dir: &Path) -> Option<PathBuf> {
    std::fs::read_dir(dir).ok()?.flatten().find_map(|e| {
        let n = e.file_name();
        let n = n.to_string_lossy();
        (n.starts_with("sway-ipc.") && n.ends_with(".sock")).then(|| e.path())
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_get_tree_and_flattens_app_windows() {
        let json = r#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1280,"height":720},
          "nodes":[{"id":2,"name":"HEADLESS-1","rect":{"x":0,"y":0,"width":1280,"height":720},
            "nodes":[{"id":7,"name":"glass-testapp-1","focused":true,
              "rect":{"x":480,"y":240,"width":320,"height":240},
              "foreign_toplevel_identifier":"abc123",
              "window_properties":{"class":"glass-testapp"}}],
            "floating_nodes":[]}],
          "floating_nodes":[]}"#;
        let root: Node = serde_json::from_str(json).unwrap();
        let wins = root.windows();
        assert_eq!(wins.len(), 1);
        let w = &wins[0];
        assert_eq!(w.con_id, 7);
        assert_eq!(w.title.as_deref(), Some("glass-testapp-1"));
        assert_eq!(w.class.as_deref(), Some("glass-testapp"));
        assert_eq!((w.rect.x, w.rect.y, w.rect.width, w.rect.height), (480, 240, 320, 240));
        assert!(w.focused);
        assert_eq!(w.identifier, "abc123");
    }

    #[test]
    fn ignores_nodes_without_foreign_toplevel_identifier() {
        let json = r#"{"id":1,"name":"root","rect":{"x":0,"y":0,"width":1,"height":1},"nodes":[],"floating_nodes":[]}"#;
        let root: Node = serde_json::from_str(json).unwrap();
        assert!(root.windows().is_empty());
    }

    #[test]
    fn command_reply_ok_when_all_succeed() {
        assert!(check_command_reply(br#"[{"success":true}]"#, "[con_id=1] focus").is_ok());
    }

    #[test]
    fn command_reply_errors_with_sways_message() {
        let reply = br#"[{"success":false,"error":"No matching node."}]"#;
        let err = check_command_reply(reply, "[con_id=9] resize set width 1 px height 1 px").unwrap_err();
        assert!(matches!(err, GlassError::Backend(_)));
        assert!(err.to_string().contains("No matching node."), "{err}");
    }
}
