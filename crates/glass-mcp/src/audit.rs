//! Concrete audit sink: implements `glass_core::AuditSink` by appending JSONL to a
//! file, redacting content by default. The seam (when/what/completeness) lives in
//! glass-core; this owns the wire format + redaction policy.

use std::fs::OpenOptions;
use std::io::Write;
use std::sync::Mutex;
use std::time::Duration;

use glass_core::{
    platform::Segment, Actuation, ActuationContext, AuditOutcome, AuditSink, KeyEvent, Modifier,
    MouseButton, PointerEvent, WindowOp,
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
                // `error` is recorded verbatim and is NOT run through content redaction.
                // Invariant: backend error messages must never embed actuation content
                // (e.g. the typed text), or it would leak here regardless of content mode.
                error: outcome.error.clone(),
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
    // `try_fill_bytes`, not `fill_bytes`: `record` must never panic, and `OsRng` can
    // fail. A session id only distinguishes start→stop cycles (it need not be
    // unpredictable), so on the astronomically-rare RNG error use a fixed fallback tag.
    if rand::rngs::OsRng.try_fill_bytes(&mut b).is_err() {
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
        assert!(render_content(
            "s",
            &AuditConfig {
                content: ContentMode::None,
                prefix_len: 8
            }
        )
        .is_none());
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
            },
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
        use crate::tools::testutil::{glass_with, FakePlatform};
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
                text: "secret".into(),
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
