//! Concrete audit sink: implements `glass_core::AuditSink` by appending JSONL to a
//! file, redacting content by default. The seam (when/what/completeness) lives in
//! glass-core; this owns the wire format + redaction policy.

use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use glass_core::{
    Actuation, ActuationContext, AuditOutcome, AuditSink, KeyEvent, Modifier, MouseButton,
    PointerEvent, WindowOp,
};
use rand::RngCore;
use serde::Serialize;
use serde_json::{json, Value};
use sha2::{Digest, Sha256};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ContentMode {
    None,
    Redacted,
    Full,
}

impl ContentMode {
    pub fn parse(s: &str) -> Self {
        match s.trim().to_ascii_lowercase().as_str() {
            "none" => ContentMode::None,
            "full" => ContentMode::Full,
            _ => ContentMode::Redacted,
        }
    }
}

#[derive(Debug, Clone, Copy)]
pub struct AuditConfig {
    pub content: ContentMode,
    pub prefix_len: usize,
}

impl Default for AuditConfig {
    fn default() -> Self {
        AuditConfig { content: ContentMode::Redacted, prefix_len: 8 }
    }
}

fn sha256_hex(raw: &str) -> String {
    Sha256::digest(raw.as_bytes()).iter().map(|b| format!("{b:02x}")).collect()
}

fn char_prefix(raw: &str, n: usize) -> &str {
    match raw.char_indices().nth(n) {
        Some((i, _)) => &raw[..i],
        None => raw,
    }
}

/// Content descriptor for one content-bearing actuation (`None` when mode is `None`).
/// `len` is the UTF-8 **byte** length of the content (not the char count).
pub fn render_content(raw: &str, cfg: &AuditConfig) -> Option<Value> {
    match cfg.content {
        ContentMode::None => None,
        ContentMode::Redacted => {
            let mut o = json!({ "len": raw.len(), "sha256": sha256_hex(raw) });
            if cfg.prefix_len > 0 {
                o["prefix"] = Value::String(char_prefix(raw, cfg.prefix_len).into());
            }
            Some(o)
        }
        ContentMode::Full => {
            Some(json!({ "len": raw.len(), "sha256": sha256_hex(raw), "text": raw }))
        }
    }
}

fn fmt_button(b: &MouseButton) -> &'static str {
    match b {
        MouseButton::Left => "left",
        MouseButton::Right => "right",
        MouseButton::Middle => "middle",
    }
}

fn fmt_mods(m: &[Modifier]) -> Vec<String> {
    m.iter().map(|x| format!("{x:?}").to_lowercase()).collect()
}

/// Map an `Actuation` to `(action, args, raw_content)`. `None` = do not record
/// (v1 excludes `Move`; `Geometry` never reaches here because `window` skips it).
fn describe(act: &Actuation) -> Option<(&'static str, Value, Option<String>)> {
    Some(match act {
        Actuation::Launch { spec, backend } => {
            let tail: Vec<&String> = spec.run.iter().skip(1).collect();
            (
                "launch",
                json!({
                    "program": spec.run.first(),
                    "backend": backend,
                    "argc": spec.run.len(),
                    "has_build": spec.build.is_some()
                }),
                Some(json!({ "args": tail, "build": spec.build }).to_string()),
            )
        }
        Actuation::Stop => ("stop", json!({}), None),
        Actuation::Pointer { event } => match event {
            PointerEvent::Move { .. } => return None,
            PointerEvent::Click { x, y, button, count, modifiers } => (
                "click",
                json!({
                    "x": x,
                    "y": y,
                    "button": fmt_button(button),
                    "count": count,
                    "modifiers": fmt_mods(modifiers)
                }),
                None,
            ),
            PointerEvent::Drag { from_x, from_y, to_x, to_y, button, modifiers, duration_ms } => (
                "drag",
                json!({
                    "from_x": from_x,
                    "from_y": from_y,
                    "to_x": to_x,
                    "to_y": to_y,
                    "button": fmt_button(button),
                    "modifiers": fmt_mods(modifiers),
                    "duration_ms": duration_ms
                }),
                None,
            ),
            PointerEvent::Scroll { x, y, dx, dy, modifiers } => (
                "scroll",
                json!({
                    "x": x,
                    "y": y,
                    "dx": dx,
                    "dy": dy,
                    "modifiers": fmt_mods(modifiers)
                }),
                None,
            ),
        },
        Actuation::Key { event } => match event {
            KeyEvent::Text(s) => ("type", json!({}), Some(s.clone())),
            KeyEvent::Chord(c) => ("key", json!({ "chord": c }), None),
        },
        Actuation::ClipboardSet { text } => {
            ("clipboard_set", json!({}), Some((*text).to_string()))
        }
        Actuation::Window { op } => {
            let args = match op {
                WindowOp::Focus => json!({ "op": "focus" }),
                WindowOp::Resize { width, height } => {
                    json!({ "op": "resize", "width": width, "height": height })
                }
                WindowOp::Move { x, y } => json!({ "op": "move", "x": x, "y": y }),
                WindowOp::Geometry => return None,
            };
            ("window", args, None)
        }
        Actuation::ClickElement { .. } => ("click_element", json!({}), None),
        Actuation::SetValue { text, .. } => ("set_value", json!({}), Some((*text).to_string())),
    })
}

fn target_json(act: &Actuation, ctx: &ActuationContext) -> Value {
    match act {
        Actuation::ClickElement { element } | Actuation::SetValue { element, .. } => {
            json!({ "element": { "id": element.id, "role": element.role, "name": element.name } })
        }
        _ => match &ctx.window {
            Some(w) => json!({ "window": { "id": w.id, "title": w.title } }),
            None => Value::Null,
        },
    }
}

#[derive(Serialize)]
struct ResultRecord {
    ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    error: Option<String>,
    duration_ms: u64,
}

#[derive(Serialize)]
struct AuditRecord<'a> {
    v: u32,
    seq: u64,
    ts: String,
    session: Option<String>,
    action: &'a str,
    target: Value,
    args: Value,
    #[serde(skip_serializing_if = "Option::is_none")]
    content: Option<Value>,
    result: ResultRecord,
}

struct SinkState {
    writer: Box<dyn Write + Send>,
    seq: u64,
    session: Option<String>,
    dropped: u64,
}

/// Append-only JSONL audit sink. Only constructed when auditing is enabled.
pub struct JsonlSink {
    state: Mutex<SinkState>,
    cfg: AuditConfig,
}

impl JsonlSink {
    #[cfg(test)]
    pub fn with_writer(writer: Box<dyn Write + Send>, cfg: AuditConfig) -> Self {
        JsonlSink {
            state: Mutex::new(SinkState { writer, seq: 0, session: None, dropped: 0 }),
            cfg,
        }
    }
}

impl AuditSink for JsonlSink {
    fn record(&self, act: &Actuation, ctx: &ActuationContext, outcome: &AuditOutcome, dur: Duration) {
        let Some((action, args, raw)) = describe(act) else { return };
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        st.seq += 1;
        if action == "launch" {
            st.session = Some(mint_session());
        }
        let session = st.session.clone();
        let rec = AuditRecord {
            v: 1,
            seq: st.seq,
            ts: chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, true),
            session,
            action,
            target: target_json(act, ctx),
            args,
            content: raw.as_deref().and_then(|r| render_content(r, &self.cfg)),
            result: ResultRecord {
                ok: outcome.ok,
                error: outcome.error.clone(),
                duration_ms: u64::try_from(dur.as_millis()).unwrap_or(u64::MAX),
            },
        };
        if action == "stop" {
            st.session = None;
        }
        match serde_json::to_string(&rec) {
            Ok(mut line) => {
                line.push('\n');
                if let Err(e) = st.writer.write_all(line.as_bytes()).and_then(|_| st.writer.flush()) {
                    st.dropped += 1;
                    eprintln!("glass: AUDIT WRITE FAILED (seq {}): {e} — record dropped", st.seq);
                }
            }
            Err(e) => {
                st.dropped += 1;
                eprintln!("glass: AUDIT SERIALIZE FAILED (seq {}): {e}", st.seq);
            }
        }
    }
}

fn mint_session() -> String {
    let mut b = [0u8; 8];
    rand::rngs::OsRng.fill_bytes(&mut b);
    format!("s-{}", b.iter().map(|x| format!("{x:02x}")).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{
        Actuation, ActuationContext, AuditOutcome, ElementRef, KeyEvent, MouseButton, PointerEvent,
        WindowRef,
    };
    use std::io::Write;
    use std::sync::{Arc, Mutex};
    use std::time::Duration;

    #[derive(Clone)]
    struct Buf(Arc<Mutex<Vec<u8>>>);
    impl Write for Buf {
        fn write(&mut self, b: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(b);
            Ok(b.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }
    fn ok() -> AuditOutcome {
        AuditOutcome { ok: true, error: None }
    }
    fn lines(b: &Arc<Mutex<Vec<u8>>>) -> Vec<serde_json::Value> {
        String::from_utf8(b.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn win_ctx() -> ActuationContext {
        ActuationContext { window: Some(WindowRef { id: 7, title: Some("W".into()) }) }
    }

    #[test]
    fn redacted_content_has_len_sha256_prefix_no_text() {
        let cfg = AuditConfig { content: ContentMode::Redacted, prefix_len: 8 };
        let v = render_content("hunter2!!", &cfg).unwrap();
        assert_eq!(v["len"], 9);
        assert_eq!(v["prefix"], "hunter2!");
        assert!(v["sha256"].is_string());
        assert!(v.get("text").is_none());
    }

    #[test]
    fn prefix_utf8_safe_and_zero_len_omits() {
        let v =
            render_content("éà-x", &AuditConfig { content: ContentMode::Redacted, prefix_len: 2 })
                .unwrap();
        assert_eq!(v["prefix"], "éà");
        let v0 = render_content(
            "x",
            &AuditConfig { content: ContentMode::Redacted, prefix_len: 0 },
        )
        .unwrap();
        assert!(v0.get("prefix").is_none());
    }

    #[test]
    fn full_mode_has_text_none_mode_omits() {
        let f =
            render_content("s", &AuditConfig { content: ContentMode::Full, prefix_len: 8 }).unwrap();
        assert_eq!(f["text"], "s");
        assert!(render_content("s", &AuditConfig { content: ContentMode::None, prefix_len: 8 })
            .is_none());
    }

    #[test]
    fn type_maps_to_action_with_redacted_content_and_window_target() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::Key { event: &KeyEvent::Text("pw".into()) },
            &win_ctx(),
            &ok(),
            Duration::from_millis(3),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["action"], "type");
        assert_eq!(r["target"]["window"]["id"], 7);
        assert_eq!(r["content"]["len"], 2);
        assert!(r["content"].get("text").is_none());
        assert_eq!(r["result"]["ok"], true);
    }

    #[test]
    fn click_maps_args_key_chord_in_args() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::Pointer {
                event: &PointerEvent::Click {
                    x: 4,
                    y: 5,
                    button: MouseButton::Right,
                    count: 2,
                    modifiers: vec![],
                },
            },
            &win_ctx(),
            &ok(),
            Duration::from_millis(1),
        );
        s.record(
            &Actuation::Key { event: &KeyEvent::Chord("ctrl+s".into()) },
            &win_ctx(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = lines(&buf);
        assert_eq!(r[0]["action"], "click");
        assert_eq!(r[0]["args"]["x"], 4);
        assert_eq!(r[0]["args"]["button"], "right");
        assert_eq!(r[0]["args"]["count"], 2);
        assert_eq!(r[1]["action"], "key");
        assert_eq!(r[1]["args"]["chord"], "ctrl+s");
        assert!(r[1].get("content").is_none());
    }

    #[test]
    fn move_is_not_written() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::Pointer { event: &PointerEvent::Move { x: 1, y: 1 } },
            &win_ctx(),
            &ok(),
            Duration::from_millis(1),
        );
        assert!(buf.lock().unwrap().is_empty(), "Move is excluded in v1");
    }

    #[test]
    fn set_value_targets_element() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        let el = ElementRef { id: 5, role: Some("PasswordField".into()), name: Some("Password".into()) };
        s.record(
            &Actuation::SetValue { element: el, text: "v" },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["action"], "set_value");
        assert_eq!(r["target"]["element"]["id"], 5);
        assert_eq!(r["target"]["element"]["role"], "PasswordField");
        assert_eq!(r["content"]["len"], 1);
    }
}
