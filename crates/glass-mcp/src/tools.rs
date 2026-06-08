//! Pure tool logic: parse already-deserialized args, call `Glass`, format
//! results. No rmcp, no X11 — unit-tested against a fake `Platform`. Every
//! failure returns `Err(String)` (an agent-readable message); the server shell
//! turns that into an MCP error result. Never a silent success.
//!
use std::path::PathBuf;

use glass_core::{AppSpec, AxNodeId, Glass, MouseButton, WindowGeometry, WindowHint, WindowId, WindowOp, frame_to_webp};
use serde_json::json;

use crate::params::*;

/// A single piece of MCP content the server will emit.
#[derive(Debug)]
pub enum OutContent {
    Text(String),
    /// Encoded image bytes (lossless WebP); the server base64s and tags these
    /// as `image/webp` MCP image content.
    Image(Vec<u8>),
}

/// What a tool produced. The server converts this into MCP `Content`.
#[derive(Debug)]
pub struct ToolOutput(pub Vec<OutContent>);

impl ToolOutput {
    pub fn text(s: impl Into<String>) -> Self {
        ToolOutput(vec![OutContent::Text(s.into())])
    }
}

/// Tool result: Ok(content) or Err(agent-readable message).
pub type ToolResult = Result<ToolOutput, String>;

fn geometry_json(g: &WindowGeometry) -> String {
    json!({ "x": g.x, "y": g.y, "width": g.width, "height": g.height }).to_string()
}

/// Resolve the sandbox level: explicit arg → `GLASS_SANDBOX` env → `Default`.
fn resolve_sandbox(arg: Option<&str>, env: Option<&str>) -> Result<glass_core::SandboxLevel, String> {
    match arg.or(env) {
        Some(s) => s.parse(),
        None => Ok(glass_core::SandboxLevel::Default),
    }
}

pub fn start(glass: &mut Glass, a: &StartArgs) -> ToolResult {
    if a.run.is_empty() {
        return Err("`run` must contain at least the program to launch".into());
    }
    let sandbox = resolve_sandbox(
        a.sandbox.as_deref(),
        std::env::var("GLASS_SANDBOX").ok().as_deref(),
    )?;
    let spec = AppSpec {
        build: a.build.clone(),
        run: a.run.clone(),
        cwd: a.cwd.clone().map(PathBuf::from),
        env: a.env.clone(),
        window_hint: a.window_hint.as_ref().map(|h| WindowHint {
            title: h.title.clone(),
            class: h.class.clone(),
        }),
        timeout_ms: a.timeout_ms.unwrap_or(10_000),
        sandbox,
    };
    let geo = match a.backend.as_deref() {
        Some(b) => glass.start_on(b, &spec),
        None => glass.start(&spec),
    }
    .map_err(|e| e.to_string())?;
    Ok(ToolOutput::text(geometry_json(&geo)))
}

pub fn stop(glass: &mut Glass) -> ToolResult {
    glass.stop().map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("stopped"))
}

pub fn window(glass: &mut Glass, a: &WindowArgs) -> ToolResult {
    let op = match a.op.as_str() {
        "focus" => WindowOp::Focus,
        "geometry" => WindowOp::Geometry,
        "resize" => WindowOp::Resize {
            width: a.width.ok_or_else(|| "resize requires `width`".to_string())?,
            height: a.height.ok_or_else(|| "resize requires `height`".to_string())?,
        },
        "move" => WindowOp::Move {
            x: a.x.ok_or_else(|| "move requires `x`".to_string())?,
            y: a.y.ok_or_else(|| "move requires `y`".to_string())?,
        },
        other => return Err(format!("unknown window op '{other}'")),
    };
    let geo = glass.window(&op).map_err(|e| e.to_string())?;
    Ok(ToolOutput::text(geometry_json(&geo)))
}

pub fn list_windows(glass: &mut Glass) -> ToolResult {
    let windows = glass.list_windows().map_err(|e| e.to_string())?;
    let arr: Vec<_> = windows
        .iter()
        .map(|w| {
            json!({
                "id": w.id.0,
                "title": w.title,
                "class": w.class,
                "x": w.geometry.x,
                "y": w.geometry.y,
                "width": w.geometry.width,
                "height": w.geometry.height,
                "active": w.active,
            })
        })
        .collect();
    let body = serde_json::Value::Array(arr).to_string();
    Ok(ToolOutput::text(crate::untrusted::wrap_untrusted(&body)))
}

pub fn select_window(glass: &mut Glass, a: &SelectWindowArgs) -> ToolResult {
    let geo = glass.select_window(WindowId(a.id)).map_err(|e| e.to_string())?;
    Ok(ToolOutput::text(geometry_json(&geo)))
}

pub fn a11y_snapshot(glass: &mut Glass) -> ToolResult {
    let tree = glass.a11y_snapshot().map_err(|e| e.to_string())?;
    let body = tree.to_outline();
    Ok(ToolOutput::text(crate::untrusted::wrap_untrusted(&body)))
}

/// A text-only `wait_stable` (default knobs, no image) for the `return:"settle"` observe.
fn settle_text_only_args() -> WaitStableArgs {
    WaitStableArgs {
        interval_ms: None,
        settle_frames: None,
        tolerance: None,
        timeout_ms: None,
        region: None,
        stability_region: None,
        include_image: Some(false),
    }
}

/// Append the optional post-action observe (`return`) to a tool's output.
fn append_return(glass: &mut Glass, ret: Option<&str>, out: &mut Vec<OutContent>) -> Result<(), String> {
    match ret {
        None | Some("none") => {}
        Some("settle") => out.extend(wait_stable(glass, &settle_text_only_args())?.0),
        Some("snapshot") => out.extend(a11y_snapshot(glass)?.0),
        Some(o) => return Err(format!("unknown return '{o}' (use none/settle/snapshot)")),
    }
    Ok(())
}

pub fn click_element(glass: &mut Glass, a: &ClickElementArgs) -> ToolResult {
    glass.click_element(AxNodeId(a.id)).map_err(|e| e.to_string())?;
    let mut out = vec![OutContent::Text(format!("clicked element #{}", a.id))];
    append_return(glass, a.return_.as_deref(), &mut out)?;
    Ok(ToolOutput(out))
}

pub fn set_value(glass: &mut Glass, a: &SetValueArgs) -> ToolResult {
    glass.set_value(AxNodeId(a.id), &a.text).map_err(|e| e.to_string())?;
    let mut out = vec![OutContent::Text(format!("set value of element #{}", a.id))];
    append_return(glass, a.return_.as_deref(), &mut out)?;
    Ok(ToolOutput(out))
}

pub fn a11y_marks(glass: &mut Glass) -> ToolResult {
    let (frame, marks) = glass.a11y_marks().map_err(|e| e.to_string())?;
    let img = frame_to_webp(&frame).map_err(|e| e.to_string())?;
    let legend = if marks.is_empty() {
        "0 interactable elements".to_string()
    } else {
        marks
            .iter()
            .map(|m| match &m.name {
                Some(name) => format!("#{} {:?} {name:?}", m.id.0, m.role),
                None => format!("#{} {:?}", m.id.0, m.role),
            })
            .collect::<Vec<_>>()
            .join("\n")
    };
    Ok(ToolOutput(vec![
        OutContent::Image(img),
        OutContent::Text(crate::untrusted::wrap_untrusted(&legend)),
        OutContent::Text(crate::untrusted::IMAGE_NOTE.to_string()),
    ]))
}

pub(crate) fn parse_button(s: Option<&str>) -> Result<MouseButton, String> {
    match s.unwrap_or("left") {
        "left" => Ok(MouseButton::Left),
        "right" => Ok(MouseButton::Right),
        "middle" => Ok(MouseButton::Middle),
        other => Err(format!("unknown button '{other}' (use left/right/middle)")),
    }
}

// Re-export the symbols later tasks (input, capture) add to this file.
pub use self::batch::*;
pub use self::capture::*;
pub use self::clipboard::*;
pub use self::input::*;
pub use self::wait::*;

mod batch;
mod capture; // filled in Task 6
mod clipboard;
mod input; // filled in Task 5
mod wait;

#[cfg(test)]
pub(crate) mod testutil {
    //! A scriptable in-memory `Platform` so tool logic can be tested with no X
    //! server. Mirrors the one in glass-core's own tests.
    use std::collections::VecDeque;
    use std::sync::{Arc, Mutex};

    use glass_core::{
        Accessibility, AppSpec, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget,
        AxTree, Backend, BaselineStore, Frame, Glass, GlassError, KeyEvent, Platform,
        PlatformFactory, PointerEvent, Region, Result, Stream, WindowGeometry, WindowId,
        WindowInfo, WindowOp,
    };

    #[derive(Default)]
    pub struct FakePlatform {
        pub geometry: WindowGeometry,
        pub frames: VecDeque<Frame>,
        pub pending_logs: Vec<(Stream, String)>,
        pub pointer_events: Vec<PointerEvent>,
        pub key_events: Vec<KeyEvent>,
        pub started: bool,
        pub events: Arc<Mutex<Vec<String>>>,
        pub clipboard: String,
    }

    impl FakePlatform {
        pub fn new(width: u32, height: u32) -> Self {
            Self {
                geometry: WindowGeometry { x: 0, y: 0, width, height },
                ..Default::default()
            }
        }
        pub fn with_frames(mut self, frames: Vec<Frame>) -> Self {
            self.frames = frames.into();
            self
        }
        pub fn with_logs(mut self, logs: Vec<(Stream, &str)>) -> Self {
            self.pending_logs = logs.into_iter().map(|(s, t)| (s, t.to_string())).collect();
            self
        }
        pub fn with_event_log(mut self, log: Arc<Mutex<Vec<String>>>) -> Self {
            self.events = log;
            self
        }
    }

    impl Platform for FakePlatform {
        fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
            self.started = true;
            Ok(self.geometry.clone())
        }
        fn stop_app(&mut self) -> Result<()> {
            self.started = false;
            Ok(())
        }
        fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
            let frame = match self.frames.pop_front() {
                Some(f) => {
                    if self.frames.is_empty() {
                        self.frames.push_back(f.clone());
                    }
                    f
                }
                None => return Err(GlassError::CaptureFailed("no scripted frames".into())),
            };
            match region {
                Some(r) => frame.crop(r),
                None => Ok(frame),
            }
        }
        fn send_pointer(&mut self, e: &PointerEvent) -> Result<()> {
            self.events.lock().unwrap().push(match e {
                PointerEvent::Click { x, y, .. } => format!("click({x},{y})"),
                PointerEvent::Move { x, y } => format!("move({x},{y})"),
                PointerEvent::Drag { from_x, from_y, to_x, to_y, .. } => {
                    format!("drag({from_x},{from_y}->{to_x},{to_y})")
                }
                PointerEvent::Scroll { x, y, dx, dy, .. } => format!("scroll({x},{y},{dx},{dy})"),
            });
            self.pointer_events.push(e.clone());
            Ok(())
        }
        fn send_key(&mut self, e: &KeyEvent) -> Result<()> {
            self.events.lock().unwrap().push(match e {
                KeyEvent::Text(t) => format!("type({t})"),
                KeyEvent::Chord(c) => format!("key({c})"),
            });
            self.key_events.push(e.clone());
            Ok(())
        }
        fn window(&mut self, op: &WindowOp) -> Result<WindowGeometry> {
            match *op {
                WindowOp::Resize { width, height } => {
                    self.geometry.width = width;
                    self.geometry.height = height;
                }
                WindowOp::Move { x, y } => {
                    self.geometry.x = x;
                    self.geometry.y = y;
                }
                WindowOp::Focus | WindowOp::Geometry => {}
            }
            Ok(self.geometry.clone())
        }
        fn list_windows(&mut self) -> Result<Vec<WindowInfo>> {
            Ok(vec![WindowInfo {
                id: WindowId(0),
                title: Some("fake".into()),
                class: None,
                geometry: self.geometry.clone(),
                active: true,
            }])
        }
        fn select_window(&mut self, id: WindowId) -> Result<WindowGeometry> {
            if id == WindowId(0) {
                Ok(self.geometry.clone())
            } else {
                Err(GlassError::WindowNotFound)
            }
        }
        fn drain_logs(&mut self) -> Vec<(Stream, String)> {
            std::mem::take(&mut self.pending_logs)
        }
        fn get_clipboard(&mut self) -> Result<String> {
            Ok(self.clipboard.clone())
        }
        fn set_clipboard(&mut self, text: &str) -> Result<()> {
            self.clipboard = text.to_string();
            Ok(())
        }
    }

    /// Build a `Glass` over a `FakePlatform` with a throwaway baseline dir.
    pub fn glass_with(platform: FakePlatform) -> Glass {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir); // keep the dir alive for the test
        // Factory yields the pre-scripted platform once.
        let mut held: Option<Box<dyn Platform + Send>> = Some(Box::new(platform));
        let factory: PlatformFactory = Box::new(move |_backend| {
            let platform =
                held.take().ok_or_else(|| GlassError::Backend("test factory called twice".into()))?;
            Ok(Backend::display_only(platform))
        });
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
    }

    /// What `FakeAccessibility::set_value` should do — lets a test model the
    /// backend rejecting a write (element not editable, or changed since the
    /// snapshot) so the tool layer's error propagation can be exercised.
    #[derive(Clone, Copy, Default, PartialEq)]
    pub enum SetOutcome {
        #[default]
        Ok,
        NotEditable,
        Changed,
    }

    pub struct FakeAccessibility {
        pub tree: AxTree,
        pub set_log: std::sync::Arc<std::sync::Mutex<Vec<(AxTarget, String)>>>,
        pub set_outcome: SetOutcome,
    }

    impl Accessibility for FakeAccessibility {
        fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
            Ok(self.tree.clone())
        }
        fn set_value(&mut self, _ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
            match self.set_outcome {
                SetOutcome::NotEditable => return Err(GlassError::AxElementNotEditable(target.id.0)),
                SetOutcome::Changed => return Err(GlassError::AxElementChanged(target.id.0)),
                SetOutcome::Ok => {}
            }
            self.set_log.lock().unwrap().push((target.clone(), text.to_string()));
            Ok(())
        }
    }

    /// A Window #0 with a Button "Save" child at (10,10 20x20).
    pub fn fake_tree() -> AxTree {
        let button = AxNode {
            id: AxNodeId(0),
            role: AxRole::Button,
            raw_role: "push button".into(),
            name: Some("Save".into()),
            value: None,
            states: AxStates { focusable: true, enabled: true, ..Default::default() },
            bounds: Some(AxRect { x: 10, y: 10, width: 20, height: 20 }),
            children: vec![],
        };
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Win".into()),
            value: None,
            states: AxStates::default(),
            bounds: Some(AxRect { x: 0, y: 0, width: 100, height: 100 }),
            children: vec![button],
        };
        AxTree { root, count: 0 }
    }

    pub fn glass_with_a11y(platform: FakePlatform, tree: AxTree) -> Glass {
        glass_with_a11y_outcome(platform, tree, SetOutcome::Ok)
    }

    /// Like [`glass_with_a11y`] but with a chosen `set_value` outcome, so a test can
    /// drive the not-editable / changed-since-snapshot rejection paths.
    pub fn glass_with_a11y_outcome(platform: FakePlatform, tree: AxTree, set_outcome: SetOutcome) -> Glass {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path().join("baselines");
        std::mem::forget(dir);
        let mut held: Option<Backend> = Some(Backend {
            platform: Box::new(platform),
            accessibility: Some(Box::new(FakeAccessibility {
                tree,
                set_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
                set_outcome,
            })),
        });
        let factory: PlatformFactory = Box::new(move |_backend| {
            held.take().ok_or_else(|| GlassError::Backend("test factory called twice".into()))
        });
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
    }
}

#[cfg(test)]
mod tests {
    use glass_core::{AppSpec, SandboxLevel};
    use super::testutil::*;
    use super::*;

    fn start_args() -> StartArgs {
        StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: None,
        }
    }

    #[test]
    fn sandbox_precedence_arg_over_env_over_default() {
        // arg wins
        assert_eq!(resolve_sandbox(Some("strict"), Some("off")).unwrap(), SandboxLevel::Strict);
        // env used when no arg
        assert_eq!(resolve_sandbox(None, Some("off")).unwrap(), SandboxLevel::Off);
        // default when neither
        assert_eq!(resolve_sandbox(None, None).unwrap(), SandboxLevel::Default);
        // bad value is a clear error
        assert!(resolve_sandbox(Some("nope"), None).is_err());
    }

    #[test]
    fn start_returns_geometry_json() {
        let mut g = glass_with(FakePlatform::new(80, 60));
        let out = start(&mut g, &start_args()).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.contains("\"width\":80"));
                assert!(t.contains("\"height\":60"));
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn start_rejects_empty_run() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        let mut a = start_args();
        a.run.clear();
        assert!(start(&mut g, &a).is_err());
    }

    #[test]
    fn stop_without_session_errors_with_message() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        let err = stop(&mut g).unwrap_err();
        assert!(err.contains("no active session"));
    }

    #[test]
    fn window_resize_requires_dimensions() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        start(&mut g, &start_args()).unwrap();
        let a = WindowArgs { op: "resize".into(), x: None, y: None, width: None, height: None };
        assert!(window(&mut g, &a).unwrap_err().contains("width"));
    }

    #[test]
    fn window_resize_updates_and_returns_geometry() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        start(&mut g, &start_args()).unwrap();
        let a = WindowArgs {
            op: "resize".into(),
            x: None,
            y: None,
            width: Some(33),
            height: Some(44),
        };
        let out = window(&mut g, &a).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("\"width\":33") && t.contains("\"height\":44")),
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn parse_button_maps_and_rejects() {
        assert!(matches!(parse_button(Some("middle")), Ok(MouseButton::Middle)));
        assert!(matches!(parse_button(None), Ok(MouseButton::Left)));
        assert!(parse_button(Some("nope")).is_err());
    }

    #[test]
    fn a11y_snapshot_returns_outline_text() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        })
        .unwrap();
        let out = a11y_snapshot(&mut g).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {t}");
                assert!(t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"), "enveloped: {t}");
                assert!(t.contains("#0 Window"), "outline: {t}");
                assert!(t.contains("#1 Button \"Save\" (10,10 20x20)"), "outline: {t}");
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn a11y_snapshot_unsupported_message() {
        let mut g = glass_with(FakePlatform::new(40, 30));
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        })
        .unwrap();
        let err = a11y_snapshot(&mut g).unwrap_err();
        assert!(err.contains("not supported"), "msg: {err}");
    }

    #[test]
    fn set_value_tool_ok_and_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&AppSpec {
            build: None, run: vec!["x".into()], cwd: None, env: vec![],
            window_hint: None, timeout_ms: 1, sandbox: SandboxLevel::Off,
        }).unwrap();
        a11y_snapshot(&mut g).unwrap();
        let out = set_value(&mut g, &SetValueArgs { id: 1, text: "hello".into(), return_: None }).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("set value of element #1"), "msg: {t}"),
            _ => panic!("expected text"),
        }
        // unknown id surfaces the actionable message
        let err = set_value(&mut g, &SetValueArgs { id: 99, text: "x".into(), return_: None }).unwrap_err();
        assert!(err.contains("not in the current snapshot"), "msg: {err}");
    }

    #[test]
    fn set_value_tool_rejects_uneditable_and_stale() {
        let spec = AppSpec {
            build: None, run: vec!["x".into()], cwd: None, env: vec![],
            window_hint: None, timeout_ms: 1, sandbox: SandboxLevel::Off,
        };
        // Backend says the element isn't editable: the tool must surface an error,
        // never the "set value" confirmation (a silent successful-looking no-op is
        // the worst failure for an agent that then asserts "value set").
        let mut g = glass_with_a11y_outcome(FakePlatform::new(100, 100), fake_tree(), SetOutcome::NotEditable);
        g.start(&spec).unwrap();
        a11y_snapshot(&mut g).unwrap();
        let err = set_value(&mut g, &SetValueArgs { id: 1, text: "x".into(), return_: None }).unwrap_err();
        assert!(err.contains("not editable"), "msg: {err}");

        // Element changed since the snapshot: same contract — error, not success.
        let mut g = glass_with_a11y_outcome(FakePlatform::new(100, 100), fake_tree(), SetOutcome::Changed);
        g.start(&spec).unwrap();
        a11y_snapshot(&mut g).unwrap();
        let err = set_value(&mut g, &SetValueArgs { id: 1, text: "x".into(), return_: None }).unwrap_err();
        assert!(err.contains("changed since the snapshot"), "msg: {err}");
    }

    #[test]
    fn click_element_tool_ok_and_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        })
        .unwrap();
        a11y_snapshot(&mut g).unwrap();
        assert!(click_element(&mut g, &ClickElementArgs { id: 1, return_: None }).is_ok());
        let err = click_element(&mut g, &ClickElementArgs { id: 99, return_: None }).unwrap_err();
        assert!(err.contains("not in the current snapshot"), "msg: {err}");
    }

    #[test]
    fn a11y_marks_returns_image_and_legend() {
        use glass_core::Frame;
        let platform = FakePlatform::new(100, 100)
            .with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        })
        .unwrap();
        let out = a11y_marks(&mut g).unwrap();
        assert!(matches!(out.0[0], OutContent::Image(_)), "first item is the image");
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("#1 Button \"Save\""), "legend: {t}"),
            _ => panic!("expected legend text"),
        }
    }

    #[test]
    fn a11y_marks_legend_enveloped_and_image_note_present() {
        use glass_core::Frame;
        let platform = FakePlatform::new(100, 100)
            .with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        })
        .unwrap();
        let out = a11y_marks(&mut g).unwrap();
        // [Image, legend-Text (enveloped), IMAGE_NOTE-Text]
        assert!(out.0.len() >= 3, "expected [Image, legend, IMAGE_NOTE], got {} items", out.0.len());
        // legend must be enveloped
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "legend must start with NOTE: {t}");
                assert!(t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"), "legend must be in envelope: {t}");
                assert!(t.contains("#1 Button"), "legend must still contain element: {t}");
            }
            _ => panic!("expected legend text as second item"),
        }
        // IMAGE_NOTE must be present
        let has_note = out.0.iter().any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(has_note, "IMAGE_NOTE must be present in a11y_marks output");
    }

    fn started_a11y_frames(frames: Vec<glass_core::Frame>) -> Glass {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100).with_frames(frames), fake_tree());
        g.start(&AppSpec {
            build: None, run: vec!["x".into()], cwd: None, env: vec![],
            window_hint: None, timeout_ms: 1, sandbox: SandboxLevel::Off,
        })
        .unwrap();
        a11y_snapshot(&mut g).unwrap(); // populate last_ax for click_element/set_value
        g
    }

    #[test]
    fn return_none_is_confirmation_only() {
        let mut g = started_a11y_frames(vec![]);
        let out = click_element(&mut g, &ClickElementArgs { id: 1, return_: None }).unwrap();
        assert_eq!(out.0.len(), 1);
        let out2 = click_element(&mut g, &ClickElementArgs { id: 1, return_: Some("none".into()) }).unwrap();
        assert_eq!(out2.0.len(), 1);
    }

    #[test]
    fn return_unknown_errors() {
        let mut g = started_a11y_frames(vec![]);
        let err = click_element(&mut g, &ClickElementArgs { id: 1, return_: Some("wat".into()) }).unwrap_err();
        assert!(err.contains("unknown return"), "msg: {err}");
    }

    #[test]
    fn return_snapshot_appends_tree_and_refreshes_cache() {
        let mut g = started_a11y_frames(vec![]); // click + a11y_snapshot don't capture frames
        let out = click_element(&mut g, &ClickElementArgs { id: 1, return_: Some("snapshot".into()) }).unwrap();
        assert_eq!(out.0.len(), 2, "snapshot return appends exactly one item (the a11y outline)");
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("clicked element #1"), "got: {t}"),
            _ => panic!("expected confirmation text"),
        }
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(t.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {t}");
                assert!(t.contains("#1 Button \"Save\""), "a11y outline appended: {t}");
            }
            _ => panic!("expected a11y outline text"),
        }
        // the snapshot refreshed last_ax -> a follow-up id-based action still resolves
        assert!(click_element(&mut g, &ClickElementArgs { id: 1, return_: None }).is_ok());
    }

    #[test]
    fn return_settle_appends_settled_text() {
        use glass_core::Frame;
        // wait_stable needs frames; one solid frame (repeated by the fake) settles.
        let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let out = click_element(&mut g, &ClickElementArgs { id: 1, return_: Some("settle".into()) }).unwrap();
        match &out.0[1] {
            OutContent::Text(t) => assert!(t.contains("\"settled\":true"), "got: {t}"),
            _ => panic!("expected settle text"),
        }
    }

    #[test]
    fn set_value_return_snapshot() {
        let mut g = started_a11y_frames(vec![]);
        let out = set_value(&mut g, &SetValueArgs { id: 1, text: "x".into(), return_: Some("snapshot".into()) }).unwrap();
        match &out.0[0] {
            OutContent::Text(t) => assert!(t.contains("set value of element #1"), "got: {t}"),
            _ => panic!("expected confirmation"),
        }
        assert!(matches!(&out.0[1], OutContent::Text(t) if t.starts_with(crate::untrusted::NOTE) && t.contains("#1 Button")), "outline appended");
    }

    #[test]
    fn list_and_select_window_tools() {
        let mut g = glass_with(FakePlatform::new(320, 240));
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
        })
        .unwrap();

        let out = list_windows(&mut g).unwrap();
        let text = match &out.0[0] {
            OutContent::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        assert!(text.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {text}");
        assert!(text.contains("⟦untrusted:") && text.contains("⟦/untrusted:"), "enveloped: {text}");
        assert!(text.contains("\"id\":0"), "json should list window id 0: {text}");
        assert!(text.contains("\"active\":true"), "json should mark active: {text}");
        assert!(text.contains("\"width\":320"), "json should include geometry width: {text}");

        assert!(select_window(&mut g, &SelectWindowArgs { id: 0 }).is_ok());
        assert!(select_window(&mut g, &SelectWindowArgs { id: 42 }).is_err());
    }
}
