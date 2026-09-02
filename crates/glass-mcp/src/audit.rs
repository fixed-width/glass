//! Concrete audit sink: implements `glass_core::AuditSink` by appending JSONL to a
//! file, redacting content by default. The seam (when/what/completeness) lives in
//! glass-core; this owns the wire format + redaction policy.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use glass_core::{
    Actuation, ActuationContext, AuditOutcome, AuditSink, KeyEvent, Modifier, MouseButton,
    PointerEvent, WindowOp, platform::Segment,
};
use rand::TryRng;
use serde::Serialize;
use serde_json::{Value, json};
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
        AuditConfig {
            content: ContentMode::Redacted,
            prefix_len: 8,
        }
    }
}

fn sha256_hex(raw: &str) -> String {
    Sha256::digest(raw.as_bytes())
        .iter()
        .map(|b| format!("{b:02x}"))
        .collect()
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
            // Deliberately omit spec.env and spec.cwd: env vars commonly carry secrets
            // (tokens, keys) and must not land in the log. Keep them out if extended.
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
            PointerEvent::Click {
                x,
                y,
                button,
                count,
                modifiers,
            } => (
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
            PointerEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                button,
                modifiers,
                duration_ms,
            } => (
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
            PointerEvent::Scroll {
                x,
                y,
                dx,
                dy,
                modifiers,
            } => (
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
            PointerEvent::Gesture {
                pointers,
                duration_ms,
            } => (
                "gesture",
                json!({
                    "pointers": pointers.iter().map(|s: &Segment| json!({
                        "from_x": s.from_x, "from_y": s.from_y, "to_x": s.to_x, "to_y": s.to_y
                    })).collect::<Vec<_>>(),
                    "duration_ms": duration_ms
                }),
                None,
            ),
        },
        Actuation::Key { event } => match event {
            KeyEvent::Text(s) => ("type", json!({}), Some(s.clone())),
            KeyEvent::Chord(c) => ("key", json!({ "chord": c }), None),
        },
        Actuation::ClipboardSet { text } => ("clipboard_set", json!({}), Some((*text).to_string())),
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
        Actuation::ClickElement {
            mode,
            method,
            native_fallback,
            actuated_id,
            dispatch,
            confirmation,
            ..
        } => {
            let mut args = json!({
                "mode": mode,
                "dispatch": dispatch,
                "confirmation": confirmation,
            });
            if let Some(method) = method {
                args["method"] = json!(method);
            }
            if let Some(reason) = native_fallback {
                args["native_fallback"] = json!(reason);
            }
            if let Some(actuated) = actuated_id {
                args["actuated_id"] = json!(actuated);
            }
            ("click_element", args, None)
        }
        Actuation::SetValue {
            text,
            dispatch,
            confirmation,
            ..
        } => (
            "set_value",
            json!({ "dispatch": dispatch, "confirmation": confirmation }),
            Some((*text).to_string()),
        ),
        Actuation::TypeTarget {
            text,
            focus_mode,
            focus_method,
            focus_dispatch,
            focus_confirmation,
            type_dispatch,
            ..
        } => {
            let mut args = json!({
                "focus_mode": focus_mode,
                "focus_dispatch": focus_dispatch,
                "focus_confirmation": focus_confirmation,
                "type_dispatch": type_dispatch,
            });
            if let Some(method) = focus_method {
                args["focus_method"] = json!(method);
            }
            ("type", args, Some((*text).to_string()))
        }
    })
}

fn target_json(act: &Actuation, ctx: &ActuationContext) -> Value {
    match act {
        Actuation::ClickElement { element, .. }
        | Actuation::SetValue { element, .. }
        | Actuation::TypeTarget { element, .. } => {
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
            state: Mutex::new(SinkState {
                writer,
                seq: 0,
                session: None,
                dropped: 0,
            }),
            cfg,
        }
    }

    /// Open (create-or-append). Fail-closed: an I/O error is returned to the caller.
    pub fn open(path: &str, cfg: AuditConfig) -> std::io::Result<Self> {
        let file = OpenOptions::new().create(true).append(true).open(path)?;
        Ok(JsonlSink {
            state: Mutex::new(SinkState {
                writer: Box::new(file),
                seq: 0,
                session: None,
                dropped: 0,
            }),
            cfg,
        })
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.state.lock().unwrap().dropped
    }
}

impl AuditSink for JsonlSink {
    fn record(
        &self,
        act: &Actuation,
        ctx: &ActuationContext,
        outcome: &AuditOutcome,
        dur: Duration,
    ) {
        let Some((action, args, raw)) = describe(act) else {
            return;
        };
        let mut st = self.state.lock().unwrap_or_else(|p| p.into_inner());
        // Monotonic event counter. `saturating_add` so an (unreachable) overflow can't
        // panic while the lock is held — `record` must never panic (trait contract).
        st.seq = st.seq.saturating_add(1);
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
                error: outcome.error.as_ref().map(|error| {
                    if raw.is_some() {
                        "action failed".into()
                    } else {
                        error.clone()
                    }
                }),
                duration_ms: u64::try_from(dur.as_millis()).unwrap_or(u64::MAX),
            },
        };
        if action == "stop" {
            st.session = None;
        }
        // On write/serialize failure: count it, emit loudly, and continue — `seq` has
        // already advanced, so a GAP in the persisted seq sequence is the intended
        // signal that a record was lost. Do NOT renumber to hide a drop.
        match serde_json::to_string(&rec) {
            Ok(mut line) => {
                line.push('\n');
                if let Err(e) = st
                    .writer
                    .write_all(line.as_bytes())
                    .and_then(|_| st.writer.flush())
                {
                    st.dropped += 1;
                    eprintln!(
                        "glass: AUDIT WRITE FAILED (seq {}): {e} — record dropped",
                        st.seq
                    );
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
    // The fallible `SysRng` path, not the panicking `rand::rng()` one: `record` must never
    // panic, and the OS source can fail. A session id only distinguishes start→stop cycles
    // (it need not be unpredictable), so on the astronomically-rare RNG error use a fixed
    // fallback tag.
    if rand::rngs::SysRng.try_fill_bytes(&mut b).is_err() {
        return "s-norand".to_string();
    }
    format!(
        "s-{}",
        b.iter().map(|x| format!("{x:02x}")).collect::<String>()
    )
}

/// Audit posture (for `doctor`/`env`).
#[derive(Debug, Clone)]
pub struct AuditReport {
    pub enabled: bool,
    pub path: Option<String>,
    pub content: ContentMode,
    pub prefix_len: usize,
}

fn config_from(
    cli_path: Option<&str>,
    env: &dyn Fn(&str) -> Option<String>,
) -> (Option<String>, AuditConfig) {
    let path = cli_path
        .map(String::from)
        .or_else(|| env("GLASS_AUDIT_LOG").filter(|p| !p.is_empty()));
    let content = env("GLASS_AUDIT_CONTENT")
        .map(|s| ContentMode::parse(&s))
        .unwrap_or(ContentMode::Redacted);
    let prefix_len = env("GLASS_AUDIT_PREFIX_LEN")
        .and_then(|s| s.trim().parse().ok())
        .unwrap_or(8);
    (
        path,
        AuditConfig {
            content,
            prefix_len,
        },
    )
}

/// Posture only — used by the `doctor` subcommand (does NOT open the file).
pub fn report_from_config(
    cli_path: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> AuditReport {
    let (path, cfg) = config_from(cli_path, &env);
    AuditReport {
        enabled: path.is_some(),
        path,
        content: cfg.content,
        prefix_len: cfg.prefix_len,
    }
}

/// Resolve the sink (opening the file, fail-closed) and the report. `None` sink when
/// no path is configured.
pub fn resolve(
    cli_path: Option<&str>,
    env: impl Fn(&str) -> Option<String>,
) -> anyhow::Result<(Option<Box<dyn AuditSink>>, AuditReport)> {
    let (path, cfg) = config_from(cli_path, &env);
    let report = AuditReport {
        enabled: path.is_some(),
        path: path.clone(),
        content: cfg.content,
        prefix_len: cfg.prefix_len,
    };
    let sink: Option<Box<dyn AuditSink>> = match path {
        None => None,
        Some(p) => {
            Some(Box::new(JsonlSink::open(&p, cfg).map_err(|e| {
                anyhow::anyhow!("cannot open audit log {p:?}: {e}")
            })?))
        }
    };
    Ok((sink, report))
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

    use crate::artifacts::{ArtifactStore, FaultStage};
    use crate::output::{OutContent, TargetAccess, ToolEffect, ToolOutput};
    use crate::output_policy::{OutputPolicy, ToolCallOutcome};

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
        AuditOutcome {
            ok: true,
            error: None,
        }
    }
    fn lines(b: &Arc<Mutex<Vec<u8>>>) -> Vec<serde_json::Value> {
        String::from_utf8(b.lock().unwrap().clone())
            .unwrap()
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect()
    }
    fn win_ctx() -> ActuationContext {
        ActuationContext {
            window: Some(WindowRef {
                id: 7,
                title: Some("W".into()),
            }),
        }
    }

    #[test]
    fn externalized_body_is_readable_but_absent_from_audit_and_diagnostics() {
        let marker = "artifact-body-secret-marker".repeat(512);
        let successful_root = tempfile::tempdir().expect("successful store root");
        let successful_store =
            ArtifactStore::for_test(successful_root.path(), 1 << 30).expect("successful store");
        let outcome = || ToolCallOutcome {
            tool: "glass_logs",
            effect: ToolEffect::ReadOnly,
            is_error: false,
            target_access: TargetAccess::NoActiveTarget,
            output: ToolOutput::result_with(
                "glass_logs",
                serde_json::json!({}),
                vec![OutContent::untrusted_observation(&marker)],
            ),
        };
        let applied = OutputPolicy::new(successful_store.clone()).apply(outcome());
        let descriptor = applied
            .output
            .0
            .iter()
            .find_map(|content| match content {
                OutContent::ResourceLink(descriptor) => Some(descriptor),
                _ => None,
            })
            .expect("externalized descriptor");
        let artifact = successful_store
            .read(descriptor.uri())
            .expect("read-only artifact path");

        let audit_buf = Arc::new(Mutex::new(Vec::new()));
        let sink = JsonlSink::with_writer(Box::new(Buf(audit_buf.clone())), AuditConfig::default());
        sink.record(
            &Actuation::Pointer {
                event: &PointerEvent::Click {
                    x: 4,
                    y: 5,
                    button: MouseButton::Left,
                    count: 1,
                    modifiers: vec![],
                },
            },
            &win_ctx(),
            &ok(),
            Duration::from_millis(1),
        );
        let audit_jsonl = String::from_utf8(audit_buf.lock().unwrap().clone()).unwrap();
        let audit_record: serde_json::Value = serde_json::from_str(&audit_jsonl).unwrap();
        let audit_keys = audit_record
            .as_object()
            .unwrap()
            .keys()
            .map(String::as_str)
            .collect::<std::collections::BTreeSet<_>>();

        let failing_root = tempfile::tempdir().expect("failing store root");
        let failing_store = ArtifactStore::for_test_with_fault(
            failing_root.path(),
            1 << 30,
            FaultStage::TempWritten(0),
        )
        .expect("failing store");
        let diagnostics = Arc::new(Mutex::new(Vec::new()));
        let failing_policy =
            OutputPolicy::with_diagnostic_for_test(failing_store, diagnostics.clone());
        let _ = failing_policy.apply(outcome());
        let diagnostic = diagnostics.lock().unwrap().join("\n");

        assert!(artifact.text.contains(&marker));
        assert_eq!(audit_record["v"], 1);
        assert_eq!(
            audit_keys,
            [
                "action", "args", "result", "seq", "session", "target", "ts", "v"
            ]
            .into_iter()
            .collect()
        );
        assert!(audit_jsonl.contains("\"action\":\"click\""));
        assert!(!audit_jsonl.contains("artifact_id"));
        assert!(!audit_jsonl.contains("externalization"));
        assert!(!audit_jsonl.contains(&marker));
        assert!(!diagnostic.contains(&marker));
    }

    #[test]
    fn redacted_content_has_len_sha256_prefix_no_text() {
        let cfg = AuditConfig {
            content: ContentMode::Redacted,
            prefix_len: 8,
        };
        let v = render_content("hunter2!!", &cfg).unwrap();
        assert_eq!(v["len"], 9);
        assert_eq!(v["prefix"], "hunter2!");
        assert!(v["sha256"].is_string());
        assert!(v.get("text").is_none());
    }

    #[test]
    fn prefix_utf8_safe_and_zero_len_omits() {
        let v = render_content(
            "éà-x",
            &AuditConfig {
                content: ContentMode::Redacted,
                prefix_len: 2,
            },
        )
        .unwrap();
        assert_eq!(v["prefix"], "éà");
        let v0 = render_content(
            "x",
            &AuditConfig {
                content: ContentMode::Redacted,
                prefix_len: 0,
            },
        )
        .unwrap();
        assert!(v0.get("prefix").is_none());
    }

    #[test]
    fn full_mode_has_text_none_mode_omits() {
        let f = render_content(
            "s",
            &AuditConfig {
                content: ContentMode::Full,
                prefix_len: 8,
            },
        )
        .unwrap();
        assert_eq!(f["text"], "s");
        assert!(
            render_content(
                "s",
                &AuditConfig {
                    content: ContentMode::None,
                    prefix_len: 8
                }
            )
            .is_none()
        );
    }

    #[test]
    fn type_maps_to_action_with_redacted_content_and_window_target() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::Key {
                event: &KeyEvent::Text("pw".into()),
            },
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
            &Actuation::Key {
                event: &KeyEvent::Chord("ctrl+s".into()),
            },
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
            &Actuation::Pointer {
                event: &PointerEvent::Move { x: 1, y: 1 },
            },
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
        let el = ElementRef {
            id: 5,
            role: Some("PasswordField".into()),
            name: Some("Password".into()),
        };
        s.record(
            &Actuation::SetValue {
                element: el,
                text: "v",
                dispatch: "dispatched",
                confirmation: "value_confirmed",
            },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["action"], "set_value");
        assert_eq!(r["target"]["element"]["id"], 5);
        assert_eq!(r["target"]["element"]["role"], "PasswordField");
        assert_eq!(
            r["args"],
            json!({
                "dispatch": "dispatched",
                "confirmation": "value_confirmed",
            })
        );
        assert_eq!(r["content"]["len"], 1);
    }

    #[test]
    fn targeted_type_targets_element_and_uses_redacted_content_policy() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::TypeTarget {
                element: ElementRef {
                    id: 9,
                    role: Some("TextField".into()),
                    name: Some("Account name".into()),
                },
                text: "Ada",
                focus_mode: "auto",
                focus_method: Some("native-action"),
                focus_dispatch: "dispatched",
                focus_confirmation: "focus_confirmed",
                type_dispatch: "dispatched",
            },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["action"], "type");
        assert_eq!(r["target"]["element"]["id"], 9);
        assert_eq!(r["target"]["element"]["role"], "TextField");
        assert_eq!(r["args"]["focus_mode"], "auto");
        assert_eq!(r["args"]["focus_method"], "native-action");
        assert_eq!(r["args"]["focus_dispatch"], "dispatched");
        assert_eq!(r["args"]["focus_confirmation"], "focus_confirmed");
        assert_eq!(r["args"]["type_dispatch"], "dispatched");
        assert_eq!(r["content"]["len"], 3);
        assert!(r["content"].get("text").is_none());
    }

    #[test]
    fn targeted_type_honors_full_and_none_content_modes() {
        let element = ElementRef {
            id: 9,
            role: Some("TextField".into()),
            name: Some("Account name".into()),
        };
        let record = |content| {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let s = JsonlSink::with_writer(
                Box::new(Buf(buf.clone())),
                AuditConfig {
                    content,
                    prefix_len: 8,
                },
            );
            s.record(
                &Actuation::TypeTarget {
                    element: element.clone(),
                    text: "Ada",
                    focus_mode: "auto",
                    focus_method: Some("native-action"),
                    focus_dispatch: "dispatched",
                    focus_confirmation: "focus_confirmed",
                    type_dispatch: "dispatched",
                },
                &ActuationContext::default(),
                &ok(),
                Duration::from_millis(1),
            );
            lines(&buf).remove(0)
        };

        let full = record(ContentMode::Full);
        assert_eq!(full["content"]["text"], "Ada");
        let none = record(ContentMode::None);
        assert!(none.get("content").is_none());
    }

    fn click_el() -> ElementRef {
        ElementRef {
            id: 1,
            role: Some("Button".into()),
            name: Some("Save".into()),
        }
    }

    #[test]
    fn click_element_carries_the_actuating_method_and_its_fallback_reason() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::ClickElement {
                element: click_el(),
                mode: "auto",
                method: Some("pointer"),
                native_fallback: Some("element exposes no activation action"),
                actuated_id: None,
                dispatch: "dispatched",
                confirmation: "dispatch_confirmed",
            },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["action"], "click_element");
        assert_eq!(r["args"]["mode"], "auto");
        assert_eq!(r["args"]["method"], "pointer");
        assert_eq!(r["args"]["dispatch"], "dispatched");
        assert_eq!(r["args"]["confirmation"], "dispatch_confirmed");
        assert_eq!(
            r["args"]["native_fallback"], "element exposes no activation action",
            "the pointer path records WHY the native action wasn't used: {r}"
        );
    }

    #[test]
    fn click_element_native_action_records_no_fallback_reason() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::ClickElement {
                element: click_el(),
                mode: "native",
                method: Some("native-action"),
                native_fallback: None,
                actuated_id: None,
                dispatch: "dispatched",
                confirmation: "dispatch_confirmed",
            },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["args"]["mode"], "native");
        assert_eq!(r["args"]["method"], "native-action");
        assert_eq!(r["args"]["dispatch"], "dispatched");
        assert_eq!(r["args"]["confirmation"], "dispatch_confirmed");
        assert!(
            r["args"].get("native_fallback").is_none(),
            "nothing fell back: {r}"
        );
    }

    #[test]
    fn click_element_records_the_element_actuated_in_the_targets_place() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::ClickElement {
                element: click_el(),
                mode: "native",
                method: Some("native-action"),
                native_fallback: None,
                actuated_id: Some(7),
                dispatch: "dispatched",
                confirmation: "dispatch_confirmed",
            },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["args"]["actuated_id"], 7, "{r}");
        assert_eq!(r["target"]["element"]["id"], click_el().id, "{r}");
    }

    #[test]
    fn click_element_failure_omits_method() {
        // Neither path actuated, so there is no method — the key must be ABSENT, not null.
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        s.record(
            &Actuation::ClickElement {
                element: click_el(),
                mode: "auto",
                method: None,
                native_fallback: None,
                actuated_id: None,
                dispatch: "not_dispatched",
                confirmation: "unconfirmed",
            },
            &ActuationContext::default(),
            &AuditOutcome {
                ok: false,
                error: Some("element #1 changed since the snapshot; re-snapshot".into()),
            },
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["action"], "click_element");
        assert_eq!(r["result"]["ok"], false);
        assert_eq!(r["args"]["mode"], "auto");
        assert_eq!(r["args"]["dispatch"], "not_dispatched");
        assert_eq!(r["args"]["confirmation"], "unconfirmed");
        assert!(r["args"].get("method").is_none(), "no null method: {r}");
        assert!(r["args"].get("native_fallback").is_none(), "{r}");
    }

    #[test]
    fn semantic_content_modes_never_let_typed_or_set_text_bypass_result_error_policy() {
        const SENTINEL: &str = "AUDIT_PAYLOAD_SENTINEL_4f937e";
        let record = |content: ContentMode, act: Actuation<'_>| {
            let buf = Arc::new(Mutex::new(Vec::new()));
            let sink = JsonlSink::with_writer(
                Box::new(Buf(buf.clone())),
                AuditConfig {
                    content,
                    prefix_len: 5,
                },
            );
            sink.record(
                &act,
                &ActuationContext::default(),
                &AuditOutcome {
                    ok: false,
                    error: Some(format!("backend echoed {SENTINEL}")),
                },
                Duration::from_millis(1),
            );
            lines(&buf).remove(0)
        };
        let element = ElementRef {
            id: 9,
            role: Some("TextField".into()),
            name: Some("Account".into()),
        };
        let make = |targeted_type: bool| {
            if targeted_type {
                Actuation::TypeTarget {
                    element: element.clone(),
                    text: SENTINEL,
                    focus_mode: "auto",
                    focus_method: Some("native-action"),
                    focus_dispatch: "dispatched",
                    focus_confirmation: "focus_confirmed",
                    type_dispatch: "may_have_dispatched",
                }
            } else {
                Actuation::SetValue {
                    element: element.clone(),
                    text: SENTINEL,
                    dispatch: "may_have_dispatched",
                    confirmation: "unconfirmed",
                }
            }
        };

        for targeted_type in [false, true] {
            let none = record(ContentMode::None, make(targeted_type));
            assert!(none.get("content").is_none());
            assert!(!none["result"]["error"].as_str().unwrap().contains(SENTINEL));

            let redacted = record(ContentMode::Redacted, make(targeted_type));
            assert_eq!(redacted["content"]["len"], SENTINEL.len());
            assert!(redacted["content"]["sha256"].as_str().is_some());
            assert_eq!(redacted["content"]["prefix"], &SENTINEL[..5]);
            assert!(
                !redacted["result"]["error"]
                    .as_str()
                    .unwrap()
                    .contains(SENTINEL)
            );

            let full = record(ContentMode::Full, make(targeted_type));
            assert_eq!(full["content"]["text"], SENTINEL);
            assert!(!full["result"]["error"].as_str().unwrap().contains(SENTINEL));
            assert_eq!(full.to_string().matches(SENTINEL).count(), 1);
        }
    }

    #[test]
    fn seq_monotonic_session_minted_on_launch_cleared_on_stop() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        let spec = glass_core::AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        };
        s.record(
            &Actuation::Launch {
                spec: &spec,
                backend: "x11",
            },
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        s.record(
            &Actuation::Stop,
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        s.record(
            &Actuation::Stop,
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        ); // after stop
        let r = lines(&buf);
        assert_eq!(
            r.iter()
                .map(|x| x["seq"].as_u64().unwrap())
                .collect::<Vec<_>>(),
            vec![1, 2, 3]
        );
        let sess = r[0]["session"].as_str().unwrap().to_string();
        assert!(sess.starts_with("s-"));
        assert_eq!(r[1]["session"], sess, "stop stamps the ending session");
        assert!(r[2]["session"].is_null(), "no session after stop");
    }

    #[test]
    fn errored_actuation_records_ok_false_with_message() {
        let buf = Arc::new(Mutex::new(Vec::new()));
        let s = JsonlSink::with_writer(Box::new(Buf(buf.clone())), AuditConfig::default());
        let out = AuditOutcome {
            ok: false,
            error: Some("coords out of bounds".into()),
        };
        s.record(
            &Actuation::Pointer {
                event: &PointerEvent::Click {
                    x: 9,
                    y: 9,
                    button: MouseButton::Left,
                    count: 1,
                    modifiers: vec![],
                },
            },
            &ActuationContext::default(),
            &out,
            Duration::from_millis(1),
        );
        let r = &lines(&buf)[0];
        assert_eq!(r["result"]["ok"], false);
        assert_eq!(r["result"]["error"], "coords out of bounds");
    }

    #[test]
    fn write_failure_counts_not_panics() {
        struct Fail;
        impl Write for Fail {
            fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
                Err(std::io::Error::other("full"))
            }
            fn flush(&mut self) -> std::io::Result<()> {
                Ok(())
            }
        }
        let s = JsonlSink::with_writer(Box::new(Fail), AuditConfig::default());
        s.record(
            &Actuation::Stop,
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        assert_eq!(s.dropped(), 1);
    }

    #[test]
    fn open_fail_closed_and_append_semantics() {
        assert!(JsonlSink::open("/nonexistent-xyz/a.jsonl", AuditConfig::default()).is_err());
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("a.jsonl");
        std::fs::write(&p, "PRE\n").unwrap();
        let s = JsonlSink::open(p.to_str().unwrap(), AuditConfig::default()).unwrap();
        s.record(
            &Actuation::Stop,
            &ActuationContext::default(),
            &ok(),
            Duration::from_millis(1),
        );
        let body = std::fs::read_to_string(&p).unwrap();
        assert!(body.starts_with("PRE\n") && body.lines().count() == 2);
    }

    #[test]
    fn end_to_end_actuations_logged_reads_not() {
        use crate::params::*;
        use crate::tools;
        use crate::tools::testutil::{FakePlatform, glass_with};
        use glass_core::Frame;

        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("audit.jsonl");
        let (sink, report) = resolve(Some(path.to_str().unwrap()), |k| {
            // Disable the prefix so the redacted record never contains the plaintext.
            if k == "GLASS_AUDIT_PREFIX_LEN" {
                Some("0".into())
            } else {
                None
            }
        })
        .unwrap();
        assert!(report.enabled);

        // A FakePlatform with several frames so screenshot / settle work.
        let frame = Frame::solid(100, 100, [0, 0, 0, 255]);
        let mut g = glass_with(FakePlatform::new(100, 100).with_frames(vec![
            frame.clone(),
            frame.clone(),
            frame.clone(),
            frame,
        ]));
        g.set_audit_sink(sink.unwrap());

        tools::start(
            &mut g,
            &StartArgs {
                build: None,
                run: vec!["app".into()],
                backend: None,
                sandbox: Some("off".into()),
                cwd: None,
                env: std::collections::BTreeMap::new(),
                window_hint: None,
                timeout_ms: None,
                a11y: None,
            },
        )
        .unwrap();
        tools::screenshot(
            &mut g,
            &ScreenshotArgs {
                region: None,
                window_id: None,
            },
        )
        .unwrap(); // read — not logged
        tools::type_text(
            &mut g,
            &TypeArgs {
                target: None,
                focus_mode: None,
                timeout_ms: None,
                max_nodes: None,
                text: "secret".into(),
                return_: None,
            },
        )
        .unwrap(); // "type"
        tools::do_actions(
            &mut g,
            &DoArgs {
                actions: vec![
                    Action::Click(ClickArgs {
                        x: 1,
                        y: 2,
                        button: None,
                        count: None,
                        modifiers: None,
                    }),
                    Action::Settle(SettleArgs {
                        interval_ms: Some(0),
                        settle_frames: Some(1),
                        tolerance: None,
                        timeout_ms: Some(500),
                        stability_region: None,
                        ignore: None,
                    }),
                ],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap(); // click (logged) + settle (read — not logged)
        tools::window(
            &mut g,
            &WindowArgs {
                op: "geometry".into(),
                x: None,
                y: None,
                width: None,
                height: None,
            },
        )
        .unwrap(); // read — not logged
        tools::window(
            &mut g,
            &WindowArgs {
                op: "focus".into(),
                x: None,
                y: None,
                width: None,
                height: None,
            },
        )
        .unwrap(); // "window"
        tools::stop(&mut g).unwrap();

        let body = std::fs::read_to_string(&path).unwrap();
        let recs: Vec<serde_json::Value> = body
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        let actions: Vec<&str> = recs.iter().map(|r| r["action"].as_str().unwrap()).collect();
        assert_eq!(
            actions,
            vec!["launch", "type", "click", "window", "stop"],
            "reads (screenshot, settle, window-geometry) are not logged; glass_do click IS"
        );
        let typ = recs.iter().find(|r| r["action"] == "type").unwrap();
        assert!(
            typ["content"].get("text").is_none(),
            "redacted: no plaintext"
        );
        assert_eq!(typ["content"]["len"], 6);
        assert!(
            !body.contains("secret"),
            "plaintext must not appear in redacted mode"
        );
        // launch program is recorded verbatim (structural, not content)
        let launch = recs.iter().find(|r| r["action"] == "launch").unwrap();
        assert_eq!(launch["args"]["program"], "app");
    }

    #[test]
    fn batched_semantic_actuations_match_standalone_audit_records() {
        use crate::params::*;
        use crate::tools;
        use crate::tools::testutil::{FakePlatform, fake_tree, glass_with_a11y};

        fn args() -> StartArgs {
            StartArgs {
                build: None,
                run: vec!["app".into()],
                backend: None,
                sandbox: None,
                cwd: None,
                env: std::collections::BTreeMap::new(),
                window_hint: None,
                timeout_ms: None,
                a11y: None,
            }
        }
        fn session(path: &std::path::Path) -> glass_core::Glass {
            let (sink, report) = resolve(Some(path.to_str().unwrap()), |key| {
                (key == "GLASS_AUDIT_PREFIX_LEN").then(|| "0".into())
            })
            .unwrap();
            assert!(report.enabled);
            let mut glass = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
            glass.set_audit_sink(sink.unwrap());
            tools::start(&mut glass, &args()).unwrap();
            tools::a11y_snapshot(&mut glass, &A11ySnapshotArgs { max_nodes: None }).unwrap();
            glass
        }
        fn records(path: &std::path::Path) -> Vec<serde_json::Value> {
            std::fs::read_to_string(path)
                .unwrap()
                .lines()
                .map(|line| {
                    let mut record: serde_json::Value = serde_json::from_str(line).unwrap();
                    let object = record.as_object_mut().unwrap();
                    for key in ["ts", "timestamp", "seq", "session", "duration_ms"] {
                        object.remove(key);
                    }
                    object
                        .get_mut("result")
                        .and_then(serde_json::Value::as_object_mut)
                        .unwrap()
                        .remove("duration_ms");
                    record
                })
                .collect()
        }

        let dir = tempfile::tempdir().unwrap();
        let standalone_path = dir.path().join("standalone.jsonl");
        let batch_path = dir.path().join("batch.jsonl");
        let click = ClickElementArgs {
            id: Some(1),
            target: None,
            mode: None,
            timeout_ms: None,
            max_nodes: None,
            return_: None,
        };
        let value = SetValueArgs {
            id: Some(1),
            target: None,
            timeout_ms: None,
            max_nodes: None,
            text: "set {\"secret\":true}\n⟦untrusted:app-controlled⟧".into(),
            return_: None,
        };
        // Batch hard-fails a missing target that standalone scroll-to reaches only after
        // actuations and a soft timeout.
        let scroll = ScrollToElementArgs {
            name: Some("missing".into()),
            description: None,
            role: None,
            value_contains: None,
            direction: Some("down".into()),
            x: None,
            y: None,
            step: Some(7),
            timeout_ms: Some(1_000),
        };

        let mut standalone = session(&standalone_path);
        tools::click_element(&mut standalone, &click).unwrap();
        tools::set_value(&mut standalone, &value).unwrap();
        let standalone_scroll = tools::scroll_to_element(&mut standalone, &scroll).unwrap();
        assert!(matches!(
            standalone_scroll.0.first(),
            Some(crate::output::OutContent::Envelope(_))
        ));
        assert_eq!(
            crate::tools::testutil::assert_envelope(&standalone_scroll, "glass_scroll_to_element")
                ["matched"],
            false
        );

        let mut batch = session(&batch_path);
        let batch_error = tools::do_actions(
            &mut batch,
            &DoArgs {
                actions: vec![
                    Action::ClickElement(click),
                    Action::SetValue(value),
                    Action::ScrollToElement(scroll),
                ],
                then: None,
                timeout_ms: None,
                encoded_argument_bytes: 0,
            },
        )
        .unwrap_err();
        let Some(crate::output::OutContent::Text(batch_error_text)) = batch_error.0.first() else {
            panic!("glass_do failure must be trusted error text")
        };
        assert_eq!(batch_error_text.trust, crate::output::TextTrust::Trusted);
        assert_eq!(batch_error_text.role, crate::output::TextRole::ErrorDetail);
        let batch_error_value: serde_json::Value =
            serde_json::from_str(&batch_error_text.body).unwrap();
        assert_eq!(batch_error_value["error"]["code"], "predicate_not_matched");

        let standalone_records = records(&standalone_path);
        let batch_records = records(&batch_path);
        assert_eq!(batch_records, standalone_records);
        assert_eq!(standalone_records.len(), 5);
        assert_eq!(
            standalone_records
                .iter()
                .map(|record| record["action"].as_str().unwrap())
                .collect::<Vec<_>>(),
            vec!["launch", "click_element", "set_value", "scroll", "scroll"]
        );
        for record in &standalone_records {
            assert_eq!(record["result"]["ok"], true, "{record}");
            assert!(record["result"].get("error").is_none(), "{record}");
        }
        assert_eq!(standalone_records[1]["action"], "click_element");
        assert_eq!(standalone_records[1]["target"]["element"]["id"], 1);
        assert_eq!(standalone_records[1]["args"]["method"], "pointer");
        assert_eq!(
            standalone_records[1]["args"]["native_fallback"],
            "backend has no native action path"
        );
        assert_eq!(standalone_records[2]["action"], "set_value");
        assert_eq!(standalone_records[2]["target"]["element"]["id"], 1);
        assert!(standalone_records[2]["content"].get("text").is_none());
        assert_eq!(standalone_records[2]["content"]["len"], 50);
        assert_eq!(
            standalone_records[2]["content"]["sha256"],
            "c4cf05a87b3248c89a874c5f6e97c66efd2ff53549d062cc8323f6ec330c44e5"
        );
        assert_eq!(standalone_records[3]["action"], "scroll");
        assert_eq!(
            standalone_records[3]["args"],
            json!({ "x": 50, "y": 50, "dx": 0, "dy": 7, "modifiers": [] })
        );
        assert_eq!(standalone_records[4]["action"], "scroll");
        assert_eq!(
            standalone_records[4]["args"],
            json!({ "x": 50, "y": 50, "dx": 0, "dy": -7, "modifiers": [] })
        );
        for records in [&standalone_records, &batch_records] {
            assert!(!records.iter().any(|record| record["action"] == "glass_do"));
            assert!(
                !serde_json::to_string(records)
                    .unwrap()
                    .contains("set {\"secret\":true}")
            );
        }
    }

    #[test]
    fn resolve_cli_over_env_disabled_when_unset_modes_from_env() {
        let (sink, rep) = resolve(None, |_| None).unwrap();
        assert!(sink.is_none() && !rep.enabled);

        let dir = tempfile::tempdir().unwrap();
        let envp = dir.path().join("e.jsonl");
        let clip = dir.path().join("c.jsonl");
        let env = |k: &str| match k {
            "GLASS_AUDIT_LOG" => Some(envp.to_str().unwrap().to_string()),
            "GLASS_AUDIT_CONTENT" => Some("full".into()),
            "GLASS_AUDIT_PREFIX_LEN" => Some("4".into()),
            _ => None,
        };
        let (sink, rep) = resolve(Some(clip.to_str().unwrap()), env).unwrap();
        assert!(sink.is_some());
        assert_eq!(
            rep.path.as_deref(),
            Some(clip.to_str().unwrap()),
            "CLI path wins"
        );
        assert_eq!(rep.content, ContentMode::Full);
        assert_eq!(rep.prefix_len, 4);
    }
}
