//! A scriptable in-memory `Platform` so tool logic can be tested with no X
//! server. Mirrors the one in glass-core's own tests.
use std::collections::VecDeque;
use std::sync::{Arc, Mutex};

use glass_core::{
    Accessibility, AppSpec, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget,
    AxTree, Backend, BaselineStore, Deadline, Frame, Glass, GlassError, KeyEvent, Platform,
    PlatformFactory, PointerEvent, Region, Result, Stream, Truncation, TruncationLimit,
    WindowGeometry, WindowId, WindowInfo, WindowOp,
};

use super::{OutContent, ToolOutput};

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
    pub window_frame: Option<(WindowId, Frame)>,
    /// Count of `capture_frame` calls — lets a test assert a settle actually captured
    /// frames (e.g. `return:"snapshot"` settling before it folds the tree).
    pub captures: Arc<Mutex<usize>>,
    /// Specs `start_app` was handed, in order — the only observer of what the tool layer
    /// built from `glass_start`'s arguments.
    pub specs: Arc<Mutex<Vec<AppSpec>>>,
    pub fail_text_dispatch_after_receiving: bool,
    pub trailing_toggle: bool,
    pub protected_paths: Option<Arc<Mutex<Vec<glass_core::ProtectedHostPath>>>>,
}

impl FakePlatform {
    pub fn new(width: u32, height: u32) -> Self {
        Self {
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width,
                height,
            },
            ..Default::default()
        }
    }
    pub fn with_frames(mut self, frames: Vec<Frame>) -> Self {
        self.frames = frames.into();
        self
    }
    pub fn with_window_frame(mut self, id: WindowId, frame: Frame) -> Self {
        self.window_frame = Some((id, frame));
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
    pub fn with_capture_log(mut self, log: Arc<Mutex<usize>>) -> Self {
        self.captures = log;
        self
    }
    pub fn with_spec_log(mut self, log: Arc<Mutex<Vec<AppSpec>>>) -> Self {
        self.specs = log;
        self
    }
    pub fn fail_text_dispatch_after_receiving(mut self) -> Self {
        self.fail_text_dispatch_after_receiving = true;
        self
    }
    pub fn with_trailing_toggle(mut self) -> Self {
        self.trailing_toggle = true;
        self
    }
}

/// A 4x4 opaque frame, constant everywhere except pixel (3,3), set to `corner` —
/// a stand-in for a perpetually animating rect (a blinking caret, a clock) in
/// `ignore`-masking tests. Mirrors glass-core's own test helper of the same name.
pub fn frame_4x4_corner(corner: [u8; 4]) -> Frame {
    let mut px = vec![0u8; 4 * 4 * 4];
    for i in 0..16 {
        px[i * 4 + 3] = 255; // alpha
    }
    let idx = (3 * 4 + 3) * 4;
    px[idx..idx + 4].copy_from_slice(&corner);
    Frame::new(4, 4, px).expect("4x4 frame is well-formed")
}

impl Platform for FakePlatform {
    fn configure_protected_host_paths(
        &mut self,
        paths: &[glass_core::ProtectedHostPath],
    ) -> Result<glass_core::HostPathProtectionMode> {
        if let Some(recorded) = &self.protected_paths {
            *recorded.lock().unwrap() = paths.to_vec();
        } else if !paths.is_empty() {
            return Err(GlassError::SandboxUnavailable(
                "fake backend has no protected-path support".into(),
            ));
        }
        Ok(glass_core::HostPathProtectionMode::SandboxRules)
    }
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        self.specs.lock().unwrap().push(spec.clone());
        self.started = true;
        Ok(self.geometry.clone())
    }
    fn stop_app_by(&mut self, _deadline: glass_core::Deadline) -> Result<()> {
        self.started = false;
        Ok(())
    }
    fn capture_frame_by(&mut self, region: Option<&Region>, deadline: Deadline) -> Result<Frame> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("capture"));
        }
        *self.captures.lock().unwrap() += 1;
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

    fn capture_window_by(
        &mut self,
        id: WindowId,
        region: Option<&Region>,
        deadline: Deadline,
    ) -> Result<Frame> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("window capture"));
        }
        let Some((scripted_id, frame)) = &self.window_frame else {
            return Err(GlassError::Unsupported(
                "capture_window is not supported by this backend".into(),
            ));
        };
        if *scripted_id != id {
            return Err(GlassError::WindowNotFound);
        }
        let frame = frame.clone();
        match region {
            Some(region) => frame.crop(region),
            None => Ok(frame),
        }
    }

    fn send_pointer_by(&mut self, e: &PointerEvent, deadline: Deadline) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("pointer input"));
        }
        self.events.lock().unwrap().push(match e {
            PointerEvent::Click { x, y, .. } => format!("click({x},{y})"),
            PointerEvent::Move { x, y } => format!("move({x},{y})"),
            PointerEvent::Drag {
                from_x,
                from_y,
                to_x,
                to_y,
                ..
            } => {
                format!("drag({from_x},{from_y}->{to_x},{to_y})")
            }
            PointerEvent::Scroll { x, y, dx, dy, .. } => format!("scroll({x},{y},{dx},{dy})"),
            PointerEvent::Gesture { pointers, .. } => format!("gesture({})", pointers.len()),
        });
        self.pointer_events.push(e.clone());
        Ok(())
    }
    fn send_key_by(&mut self, e: &KeyEvent, deadline: Deadline) -> Result<()> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("key input"));
        }
        self.events.lock().unwrap().push(match e {
            KeyEvent::Text(t) => format!("type({t})"),
            KeyEvent::Chord(c) => format!("key({c})"),
        });
        self.key_events.push(e.clone());
        if self.fail_text_dispatch_after_receiving
            && let KeyEvent::Text(text) = e
        {
            return Err(GlassError::Backend(format!(
                "text dispatch rejected debug={text:?} hex={}",
                text.as_bytes()
                    .iter()
                    .map(|byte| format!("{byte:02x}"))
                    .collect::<String>()
            )));
        }
        Ok(())
    }
    fn window_by(&mut self, op: &WindowOp, deadline: Deadline) -> Result<WindowGeometry> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("window operation"));
        }
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
    fn list_windows_by(&mut self, deadline: Deadline) -> Result<Vec<WindowInfo>> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("window list"));
        }
        Ok(vec![WindowInfo {
            id: WindowId(0),
            title: Some("fake".into()),
            class: None,
            geometry: self.geometry.clone(),
            active: true,
        }])
    }
    fn select_window_by(&mut self, id: WindowId, deadline: Deadline) -> Result<WindowGeometry> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("window selection"));
        }
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
    fn a11y_toggle_control_at_trailing_edge(&self) -> bool {
        self.trailing_toggle
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
        let platform = held
            .take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))?;
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
    EchoText,
}

/// What `FakeAccessibility::invoke` should do. Default mirrors the trait's own
/// default (unsupported) — a backend that never implemented the native action,
/// so `click_element` falls back to the pointer path unless a test opts into
/// [`InvokeOutcome::Ok`].
#[derive(Clone, Copy, Default, PartialEq)]
pub enum InvokeOutcome {
    #[default]
    Unsupported,
    Ok,
    /// The native action fired on a different element than the one named.
    OkOnAnother(u32),
    ErrorWithDetail(&'static str),
}

pub struct FakeAccessibility {
    pub tree: AxTree,
    pub reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    pub set_log: std::sync::Arc<std::sync::Mutex<Vec<(AxTarget, String)>>>,
    pub set_outcome: SetOutcome,
    pub invoke_outcome: InvokeOutcome,
}

impl Accessibility for FakeAccessibility {
    fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
        self.reads
            .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        Ok(self.tree.clone())
    }
    fn set_value(&mut self, _ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        match self.set_outcome {
            SetOutcome::NotEditable => {
                return Err(GlassError::AxElementNotEditable(target.id.0));
            }
            SetOutcome::Changed => return Err(GlassError::AxElementChanged(target.id.0)),
            SetOutcome::EchoText => {
                self.set_log
                    .lock()
                    .unwrap()
                    .push((target.clone(), text.to_string()));
                return Err(GlassError::Backend(format!(
                    "set-value rejected debug={text:?} hex={}",
                    text.as_bytes()
                        .iter()
                        .map(|byte| format!("{byte:02x}"))
                        .collect::<String>()
                )));
            }
            SetOutcome::Ok => {}
        }
        self.set_log
            .lock()
            .unwrap()
            .push((target.clone(), text.to_string()));
        Ok(())
    }
    fn invoke(&mut self, _ctx: &AxContext, _target: &AxTarget) -> Result<Option<AxNodeId>> {
        match self.invoke_outcome {
            InvokeOutcome::Unsupported => Err(GlassError::AxUnsupported),
            InvokeOutcome::Ok => Ok(None),
            InvokeOutcome::OkOnAnother(id) => Ok(Some(AxNodeId(id))),
            InvokeOutcome::ErrorWithDetail(detail) => Err(GlassError::Backend(detail.into())),
        }
    }
}

/// A Window #0 with a Button "Save" child at (10,10 20x20).
pub fn fake_tree() -> AxTree {
    let button = AxNode {
        id: AxNodeId(0),
        role: AxRole::Button,
        raw_role: "push button".into(),
        name: Some("Save".into()),
        description: None,
        value: None,
        states: AxStates {
            focusable: true,
            enabled: true,
            ..Default::default()
        },
        bounds: Some(AxRect {
            x: 10,
            y: 10,
            width: 20,
            height: 20,
        }),
        children: vec![],
    };
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "frame".into(),
        name: Some("Win".into()),
        description: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }),
        children: vec![button],
    };
    AxTree::new(root)
}

/// A window root with no child elements — the "app publishes no usable tree" shape.
pub fn empty_tree() -> AxTree {
    let root = AxNode {
        id: AxNodeId(0),
        role: AxRole::Window,
        raw_role: "frame".into(),
        name: Some("Win".into()),
        description: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 0,
            width: 100,
            height: 100,
        }),
        children: vec![],
    };
    AxTree::new(root)
}

/// `fake_tree` with `truncated` set — the "walk stopped early" shape, for testing that
/// the truncation steer surfaces as its own trusted block rather than being baked into
/// the untrusted-wrapped outline.
pub fn truncated_tree() -> AxTree {
    let mut t = fake_tree();
    t.truncated = Some(Truncation {
        limit: TruncationLimit::Nodes,
        limit_value: 1500,
        nodes_walked: 1500,
    });
    t
}

/// `fake_tree` with a childless `Document` child — the unpublished-web-content shape.
pub fn unpublished_document_tree() -> AxTree {
    let mut t = fake_tree();
    t.root.children.push(AxNode {
        id: AxNodeId(0),
        role: AxRole::Document,
        raw_role: "document web".into(),
        name: Some("page".into()),
        description: None,
        value: None,
        states: AxStates::default(),
        bounds: Some(AxRect {
            x: 0,
            y: 40,
            width: 100,
            height: 60,
        }),
        children: vec![],
    });
    t.assign_ids();
    t
}

pub fn glass_with_a11y(platform: FakePlatform, tree: AxTree) -> Glass {
    glass_with_a11y_outcome(platform, tree, SetOutcome::Ok)
}

/// Like [`glass_with_a11y`] but with a chosen `set_value` outcome, so a test can
/// drive the not-editable / changed-since-snapshot rejection paths. `invoke` stays
/// at its default (unsupported) — use [`glass_with_a11y_invoke_ok`] for the
/// native-action path.
pub fn glass_with_a11y_outcome(
    platform: FakePlatform,
    tree: AxTree,
    set_outcome: SetOutcome,
) -> Glass {
    glass_with_a11y_full(platform, tree, set_outcome, InvokeOutcome::Unsupported)
}

/// Like [`glass_with_a11y`] but with `invoke` wired to succeed, so a test can drive
/// `click_element`'s native-action path (no pointer event, no fallback disclosed).
pub fn glass_with_a11y_invoke_ok(platform: FakePlatform, tree: AxTree) -> Glass {
    glass_with_a11y_full(platform, tree, SetOutcome::Ok, InvokeOutcome::Ok)
}

/// [`glass_with_a11y_invoke_ok`] for a backend that actuates element `actuated` when
/// asked for another one.
pub fn glass_with_a11y_invoke_on_another(
    platform: FakePlatform,
    tree: AxTree,
    actuated: u32,
) -> Glass {
    glass_with_a11y_full(
        platform,
        tree,
        SetOutcome::Ok,
        InvokeOutcome::OkOnAnother(actuated),
    )
}

/// A backend whose native semantic click path returns the supplied raw detail.
pub fn glass_with_a11y_invoke_error(
    platform: FakePlatform,
    tree: AxTree,
    detail: &'static str,
) -> Glass {
    glass_with_a11y_full(
        platform,
        tree,
        SetOutcome::Ok,
        InvokeOutcome::ErrorWithDetail(detail),
    )
}

fn glass_with_a11y_full(
    platform: FakePlatform,
    tree: AxTree,
    set_outcome: SetOutcome,
    invoke_outcome: InvokeOutcome,
) -> Glass {
    glass_with_a11y_full_and_reads(
        platform,
        tree,
        set_outcome,
        invoke_outcome,
        std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(0)),
    )
}

fn glass_with_a11y_full_and_reads(
    platform: FakePlatform,
    tree: AxTree,
    set_outcome: SetOutcome,
    invoke_outcome: InvokeOutcome,
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
) -> Glass {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut held: Option<Backend> = Some(Backend {
        platform: Box::new(platform),
        accessibility: Some(Box::new(FakeAccessibility {
            tree,
            reads,
            set_log: std::sync::Arc::new(std::sync::Mutex::new(Vec::new())),
            set_outcome,
            invoke_outcome,
        })),
    });
    let factory: PlatformFactory = Box::new(move |_backend| {
        held.take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
    });
    Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
}

pub fn started_a11y_with(tree: AxTree) -> Glass {
    let mut glass = glass_with_a11y(FakePlatform::new(100, 100), tree);
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: Vec::new(),
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .unwrap();
    glass
}

pub fn started_counted_a11y(
    reads: std::sync::Arc<std::sync::atomic::AtomicUsize>,
    tree: AxTree,
) -> Glass {
    let mut glass = glass_with_a11y_full_and_reads(
        FakePlatform::new(100, 100),
        tree,
        SetOutcome::Ok,
        InvokeOutcome::Unsupported,
        reads,
    );
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: Vec::new(),
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .unwrap();
    glass
}

pub fn started_without_a11y() -> Glass {
    let mut glass = glass_with(FakePlatform::new(100, 100));
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: Vec::new(),
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
    glass
}

struct FailingAccessibility {
    error: Option<GlassError>,
}

impl Accessibility for FailingAccessibility {
    fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
        Err(self.error.take().expect("scripted accessibility error"))
    }
}

pub fn started_failing_a11y(error: GlassError) -> Glass {
    let dir = tempfile::tempdir().unwrap();
    let root = dir.path().join("baselines");
    std::mem::forget(dir);
    let mut held = Some(Backend {
        platform: Box::new(FakePlatform::new(100, 100)),
        accessibility: Some(Box::new(FailingAccessibility { error: Some(error) })),
    });
    let factory: PlatformFactory = Box::new(move |_backend| {
        held.take()
            .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
    });
    let mut glass = Glass::new(factory, "x11".into(), BaselineStore::new(root), 100);
    glass
        .start(&AppSpec {
            build: None,
            run: vec!["app".into()],
            cwd: None,
            env: Vec::new(),
            window_hint: None,
            timeout_ms: 1,
            sandbox: glass_core::SandboxLevel::Off,
            a11y: true,
        })
        .unwrap();
    glass
}

/// Parse content block `i` as the `{ok,tool,result}` envelope.
pub(crate) fn envelope_at(out: &ToolOutput, i: usize) -> serde_json::Value {
    let Some(OutContent::Envelope(envelope)) = out.0.get(i) else {
        panic!("expected envelope text at block {i}")
    };
    serde_json::json!({ "ok": true, "tool": envelope.tool, "result": envelope.result })
}

/// Assert block 0 is the success envelope for `tool` — and that `tool` is a REGISTERED
/// `#[tool]` name, so a co-typo shared between the tool impl's envelope literal and the
/// test's expected string (both say `"glass_stopp"`) still fails loudly. Returns `result`.
pub(crate) fn assert_envelope(out: &ToolOutput, tool: &str) -> serde_json::Value {
    let v = envelope_at(out, 0);
    assert!(
        crate::server::registered_tools().iter().any(|t| t == tool),
        "envelope tool {tool:?} is not a registered #[tool]"
    );
    v["result"].clone()
}
