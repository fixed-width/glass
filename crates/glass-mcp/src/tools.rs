//! Pure tool logic: parse already-deserialized args, call `Glass`, format
//! results. No rmcp, no X11 — unit-tested against a fake `Platform`. Every
//! failure returns `Err(String)` (an agent-readable message); the server shell
//! turns that into an MCP error result. Never a silent success.
//!
use std::path::PathBuf;

use glass_core::{
    frame_to_webp, AppSpec, AxNodeId, Glass, MouseButton, WindowGeometry, WindowHint, WindowId,
    WindowOp,
};
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
    /// Wrap a tool's trusted result payload in the uniform 1.0 success envelope
    /// as the sole leading content block.
    pub fn result(tool: &str, result: serde_json::Value) -> Self {
        ToolOutput(vec![OutContent::Text(envelope(tool, result))])
    }

    /// Envelope block first, then app-controlled/image sibling blocks unchanged.
    pub fn result_with(tool: &str, result: serde_json::Value, mut extra: Vec<OutContent>) -> Self {
        let mut v = vec![OutContent::Text(envelope(tool, result))];
        v.append(&mut extra);
        ToolOutput(v)
    }

    /// Capture-style result: the image block (when present) leads, then the envelope,
    /// then any extra sibling blocks, then the trailing IMAGE_NOTE — emitted only when an
    /// image was attached.
    pub fn image_result(
        tool: &str,
        image: Option<Vec<u8>>,
        result: serde_json::Value,
        mut siblings: Vec<OutContent>,
    ) -> Self {
        let has_image = image.is_some();
        let mut v = Vec::new();
        if let Some(img) = image {
            v.push(OutContent::Image(img));
        }
        v.push(OutContent::Text(envelope(tool, result)));
        v.append(&mut siblings);
        if has_image {
            v.push(OutContent::Text(crate::untrusted::IMAGE_NOTE.to_string()));
        }
        ToolOutput(v)
    }
}

/// Serialize the success envelope. `ok` is always true — errors take the `Err` path.
fn envelope(tool: &str, result: serde_json::Value) -> String {
    serde_json::json!({ "ok": true, "tool": tool, "result": result }).to_string()
}

/// Tool result: Ok(content) or Err(agent-readable message).
pub type ToolResult = Result<ToolOutput, String>;

fn geometry_value(g: &WindowGeometry) -> serde_json::Value {
    json!({ "x": g.x, "y": g.y, "width": g.width, "height": g.height })
}

/// Resolve the effective sandbox level from the agent's request, the operator's omit-default
/// (`GLASS_SANDBOX`), and the operator's enforced floor (`GLASS_SANDBOX_FLOOR`).
///
/// - `floor` = `GLASS_SANDBOX_FLOOR` else `Off` (no floor = today's behavior).
/// - Agent OMITS `sandbox`: `requested` = `GLASS_SANDBOX` else `Default`, then clamped UP to the
///   floor (the agent stated no preference, so policy simply applies). Never an error.
/// - Agent passes `sandbox` EXPLICITLY: honored iff at or above the floor; a request *below* the
///   floor is REFUSED, naming the policy — the operator, not the agent, decides to weaken it.
fn resolve_sandbox(
    arg: Option<&str>,
    env_default: Option<&str>,
    env_floor: Option<&str>,
) -> Result<glass_core::SandboxLevel, String> {
    use glass_core::SandboxLevel;
    let floor = match env_floor {
        Some(s) => s.parse::<SandboxLevel>()?,
        None => SandboxLevel::Off,
    };
    match arg {
        Some(s) => {
            let requested = s.parse::<SandboxLevel>()?;
            if requested.strength() < floor.strength() {
                return Err(format!(
                    "sandbox:\"{requested}\" is below the operator's containment floor \
                     (GLASS_SANDBOX_FLOOR={floor}); request a level at or above \"{floor}\", or ask \
                     the operator to lower the floor"
                ));
            }
            Ok(requested)
        }
        None => {
            let omit_default = match env_default {
                Some(s) => s.parse::<SandboxLevel>()?,
                None => SandboxLevel::Default,
            };
            Ok(if omit_default.strength() >= floor.strength() {
                omit_default
            } else {
                floor
            })
        }
    }
}

/// Resolve the `a11y` launch flag. On by default: the accessibility path (semantic
/// addressing, text-only verification) is glass's cheap, low-token default, so omitting
/// the flag enables it rather than leaving it off. Pass `a11y: false` to skip spawning
/// the accessibility bus for canvas/pixel-only work. (The flag only has effect on Linux,
/// which spawns a private AT-SPI bus; other backends read accessibility ambiently.)
fn resolve_a11y(arg: Option<bool>) -> bool {
    arg.unwrap_or(true)
}

/// Non-spawning preflight for the accessibility bus, so a best-effort (default-on) a11y
/// launch can degrade to pixel-only on a host that can't provide it (e.g. AT-SPI not
/// installed) instead of failing. Only the Linux backends spawn a private AT-SPI bus and
/// read `spec.a11y`; on every other target the flag is a no-op, so the preflight is a
/// no-op too. An explicit `a11y: true` skips this and still fails loudly if the bus can't
/// start (no silent fallback).
#[cfg(target_os = "linux")]
fn a11y_bus_preflight() -> Result<(), String> {
    glass_dbus_linux::available()
}
#[cfg(not(target_os = "linux"))]
fn a11y_bus_preflight() -> Result<(), String> {
    Ok(())
}

/// Read the operator floor env, distinguishing "unset" from "set-but-unreadable". A floor whose
/// bytes are not valid UTF-8 must NOT be silently treated as unset — that would drop the operator's
/// floor (fail-OPEN). It is an error, so the launch is refused until it's fixed (fail-closed),
/// exactly as an unrecognized floor value is. (`std::env::var(..).ok()` would collapse both the
/// absent and the non-UTF-8 cases to `None`, which is the fail-open we avoid here.)
fn floor_from_var(v: Result<String, std::env::VarError>) -> Result<Option<String>, String> {
    match v {
        Ok(s) => Ok(Some(s)),
        Err(std::env::VarError::NotPresent) => Ok(None),
        Err(std::env::VarError::NotUnicode(_)) => Err(
            "GLASS_SANDBOX_FLOOR is set but is not valid UTF-8; set it to off/default/strict or \
             unset it"
                .to_string(),
        ),
    }
}

pub fn start(glass: &mut Glass, a: &StartArgs) -> ToolResult {
    if a.run.is_empty() {
        return Err("`run` must contain at least the program to launch".into());
    }
    // Read the two operator vars into named bindings so the wiring is eyeball-obvious (a swap
    // between the omit-default and the floor would otherwise compile silently).
    let sandbox_env = std::env::var("GLASS_SANDBOX").ok();
    let floor_env = floor_from_var(std::env::var("GLASS_SANDBOX_FLOOR"))?;
    let sandbox = resolve_sandbox(
        a.sandbox.as_deref(),
        sandbox_env.as_deref(),
        floor_env.as_deref(),
    )?;
    let mut spec = AppSpec {
        build: a.build.clone(),
        run: a.run.clone(),
        cwd: a.cwd.clone().map(PathBuf::from),
        env: a.env.iter().map(|(k, v)| (k.clone(), v.clone())).collect(),
        window_hint: a.window_hint.as_ref().map(|h| WindowHint {
            title: h.title.clone(),
            class: h.class.clone(),
        }),
        timeout_ms: a.timeout_ms.unwrap_or(10_000),
        sandbox,
        a11y: resolve_a11y(a.a11y),
    };
    // Best-effort default: a11y is on unless the caller opts out, but a host that can't bring
    // up the accessibility bus must still launch and pixel-drive. When a11y is on only because
    // it defaults on (not explicitly requested) and the bus can't start here, launch without
    // it. An explicit a11y:true is left to fail loudly at the backend (no silent fallback).
    if spec.a11y && a.a11y.is_none() {
        if let Err(why) = a11y_bus_preflight() {
            eprintln!(
                "glass: accessibility is on by default but this host can't start its bus \
                 ({why}); launching without it — pass a11y:true to require it."
            );
            spec.a11y = false;
        }
    }
    let geo = match a.backend.as_deref() {
        Some(b) => glass.start_on(b, &spec),
        None => glass.start(&spec),
    }
    .map_err(|e| e.to_string())?;
    Ok(ToolOutput::result("glass_start", geometry_value(&geo)))
}

pub fn stop(glass: &mut Glass) -> ToolResult {
    glass.stop().map_err(|e| e.to_string())?;
    Ok(ToolOutput::result("glass_stop", serde_json::json!({})))
}

pub fn window(glass: &mut Glass, a: &WindowArgs) -> ToolResult {
    let op = match a.op.as_str() {
        "focus" => WindowOp::Focus,
        "geometry" => WindowOp::Geometry,
        "resize" => WindowOp::Resize {
            width: a
                .width
                .ok_or_else(|| "resize requires `width`".to_string())?,
            height: a
                .height
                .ok_or_else(|| "resize requires `height`".to_string())?,
        },
        "move" => WindowOp::Move {
            x: a.x.ok_or_else(|| "move requires `x`".to_string())?,
            y: a.y.ok_or_else(|| "move requires `y`".to_string())?,
        },
        other => return Err(format!("unknown window op '{other}'")),
    };
    let geo = glass.window(&op).map_err(|e| e.to_string())?;
    Ok(ToolOutput::result("glass_window", geometry_value(&geo)))
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
    Ok(ToolOutput::result_with(
        "glass_list_windows",
        serde_json::json!({ "count": windows.len() }),
        vec![OutContent::Text(crate::untrusted::wrap_untrusted(&body))],
    ))
}

pub fn select_window(glass: &mut Glass, a: &SelectWindowArgs) -> ToolResult {
    let geo = glass
        .select_window(WindowId(a.id))
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::result(
        "glass_select_window",
        geometry_value(&geo),
    ))
}

pub fn a11y_snapshot(glass: &mut Glass) -> ToolResult {
    let tree = glass.a11y_snapshot().map_err(|e| e.to_string())?;
    let body = tree.to_outline();
    // The outline is app-derived → untrusted-wrapped. When the tree has nothing to
    // address, add glass's own (trusted, unwrapped) hint steering to the pixel loop.
    let mut contents = vec![OutContent::Text(crate::untrusted::wrap_untrusted(&body))];
    if let Some(hint) = tree.empty_guidance() {
        contents.push(OutContent::Text(hint.to_string()));
    }
    Ok(ToolOutput::result_with(
        "glass_a11y_snapshot",
        serde_json::json!({}),
        contents,
    ))
}

/// Defaults matching the text-only settle the observe used before.
fn settle_params() -> glass_core::WaitStableParams {
    glass_core::WaitStableParams {
        interval_ms: 100,
        settle_frames: 3,
        tolerance: 0,
        timeout_ms: 5000,
        stability_region: None,
        // the return:"settle" observe has no arg surface to carry ignore rects — always masks nothing
        ignore: Vec::new(),
        window: None,
    }
}

/// Apply the optional `return` observe. `settle` → `Some(metadata)` to merge under
/// `result.observed`; `snapshot` → an untrusted outline sibling to append; none/absent
/// → neither. Calls the `Glass` methods directly (not the `a11y_snapshot`/`wait_stable`
/// tool functions) so a composed observe never nests another envelope. Unknown value
/// → `Err` (unchanged rejection).
fn resolve_return(
    glass: &mut Glass,
    ret: Option<&str>,
) -> Result<(Option<serde_json::Value>, Vec<OutContent>), String> {
    match ret {
        None | Some("none") => Ok((None, vec![])),
        Some("settle") => {
            let o = glass
                .wait_stable(&settle_params())
                .map_err(|e| e.to_string())?;
            Ok((
                Some(serde_json::json!({
                    "settled": o.settled,
                    "saw_motion": o.saw_motion,
                    "observed_ms": o.observed_ms,
                })),
                vec![],
            ))
        }
        Some("snapshot") => {
            // Let the UI settle before folding the tree so a screen-changing action (a
            // navigating click) doesn't fold a mid-transition tree. Best-effort: `wait_stable`
            // soft-times-out (`settled:false`, not an error) on a non-settling UI, and a rare
            // real capture failure is swallowed because `a11y_snapshot` reads the accessibility
            // tree (not pixels) and still returns the freshest tree — the caller asked for it.
            let _ = glass.wait_stable(&settle_params());
            let tree = glass.a11y_snapshot().map_err(|e| e.to_string())?;
            Ok((
                None,
                vec![OutContent::Text(crate::untrusted::wrap_untrusted(
                    &tree.to_outline(),
                ))],
            ))
        }
        Some(o) => Err(format!("unknown return '{o}' (use none/settle/snapshot)")),
    }
}

pub fn click_element(glass: &mut Glass, a: &ClickElementArgs) -> ToolResult {
    glass
        .click_element(AxNodeId(a.id))
        .map_err(|e| e.to_string())?;
    let (observed, extra) = resolve_return(glass, a.return_.as_deref())?;
    let mut result = serde_json::json!({ "id": a.id });
    if let Some(o) = observed {
        result["observed"] = o;
    }
    Ok(ToolOutput::result_with(
        "glass_click_element",
        result,
        extra,
    ))
}

pub fn set_value(glass: &mut Glass, a: &SetValueArgs) -> ToolResult {
    glass
        .set_value(AxNodeId(a.id), &a.text)
        .map_err(|e| e.to_string())?;
    let (observed, extra) = resolve_return(glass, a.return_.as_deref())?;
    let mut result = serde_json::json!({ "id": a.id });
    if let Some(o) = observed {
        result["observed"] = o;
    }
    Ok(ToolOutput::result_with("glass_set_value", result, extra))
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
    Ok(ToolOutput::image_result(
        "glass_a11y_marks",
        Some(img),
        serde_json::json!({ "count": marks.len() }),
        vec![OutContent::Text(crate::untrusted::wrap_untrusted(&legend))],
    ))
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
        /// Count of `capture_frame` calls — lets a test assert a settle actually captured
        /// frames (e.g. `return:"snapshot"` settling before it folds the tree).
        pub captures: Arc<Mutex<usize>>,
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
        fn start_app(&mut self, _spec: &AppSpec) -> Result<WindowGeometry> {
            self.started = true;
            Ok(self.geometry.clone())
        }
        fn stop_app(&mut self) -> Result<()> {
            self.started = false;
            Ok(())
        }
        fn capture_frame(&mut self, region: Option<&Region>) -> Result<Frame> {
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
        fn send_pointer(&mut self, e: &PointerEvent) -> Result<()> {
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
                SetOutcome::NotEditable => {
                    return Err(GlassError::AxElementNotEditable(target.id.0))
                }
                SetOutcome::Changed => return Err(GlassError::AxElementChanged(target.id.0)),
                SetOutcome::Ok => {}
            }
            self.set_log
                .lock()
                .unwrap()
                .push((target.clone(), text.to_string()));
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
        AxTree { root, count: 0 }
    }

    /// A window root with no child elements — the "app publishes no usable tree" shape.
    pub fn empty_tree() -> AxTree {
        let root = AxNode {
            id: AxNodeId(0),
            role: AxRole::Window,
            raw_role: "frame".into(),
            name: Some("Win".into()),
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
        AxTree { root, count: 0 }
    }

    pub fn glass_with_a11y(platform: FakePlatform, tree: AxTree) -> Glass {
        glass_with_a11y_outcome(platform, tree, SetOutcome::Ok)
    }

    /// Like [`glass_with_a11y`] but with a chosen `set_value` outcome, so a test can
    /// drive the not-editable / changed-since-snapshot rejection paths.
    pub fn glass_with_a11y_outcome(
        platform: FakePlatform,
        tree: AxTree,
        set_outcome: SetOutcome,
    ) -> Glass {
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
            held.take()
                .ok_or_else(|| GlassError::Backend("test factory called twice".into()))
        });
        Glass::new(factory, "x11".into(), BaselineStore::new(root), 100)
    }

    /// Parse content block `i` as the `{ok,tool,result}` envelope.
    pub(crate) fn envelope_at(out: &ToolOutput, i: usize) -> serde_json::Value {
        let OutContent::Text(t) = &out.0[i] else {
            panic!("expected envelope text at block {i}")
        };
        serde_json::from_str(t).expect("envelope must be valid JSON")
    }

    /// Assert block 0 is the success envelope for `tool` — and that `tool` is a REGISTERED
    /// `#[tool]` name, so a co-typo shared between the tool impl's envelope literal and the
    /// test's expected string (both say `"glass_stopp"`) still fails loudly. Returns `result`.
    pub(crate) fn assert_envelope(out: &ToolOutput, tool: &str) -> serde_json::Value {
        let v = envelope_at(out, 0);
        assert_eq!(v["ok"], serde_json::json!(true), "envelope: {v}");
        assert_eq!(v["tool"], serde_json::json!(tool), "envelope: {v}");
        assert!(
            crate::server::registered_tools().iter().any(|t| t == tool),
            "envelope tool {tool:?} is not a registered #[tool]"
        );
        v["result"].clone()
    }
}

#[cfg(test)]
mod tests {
    use super::testutil::*;
    use super::*;
    use glass_core::{AppSpec, SandboxLevel};

    fn start_args() -> StartArgs {
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

    #[test]
    fn a11y_defaults_on_when_omitted() {
        // The a11y-first path is the low-token default, so an omitted flag enables it.
        assert!(resolve_a11y(None), "omitted a11y must default on");
        assert!(resolve_a11y(Some(true)));
        assert!(!resolve_a11y(Some(false)), "explicit false opts out");
    }

    #[test]
    fn floor_unset_preserves_current_behavior() {
        // arg wins over env; omit → GLASS_SANDBOX else default. Floor off = no enforcement.
        assert_eq!(
            resolve_sandbox(Some("off"), Some("strict"), None).unwrap(),
            SandboxLevel::Off
        );
        assert_eq!(
            resolve_sandbox(None, Some("strict"), None).unwrap(),
            SandboxLevel::Strict
        );
        assert_eq!(
            resolve_sandbox(None, None, None).unwrap(),
            SandboxLevel::Default
        );
    }

    #[test]
    fn floor_clamps_an_omitted_request_up() {
        // omit-default default, floor strict → effective strict (policy applies, no error).
        assert_eq!(
            resolve_sandbox(None, None, Some("strict")).unwrap(),
            SandboxLevel::Strict
        );
        assert_eq!(
            resolve_sandbox(None, Some("off"), Some("default")).unwrap(),
            SandboxLevel::Default
        );
    }

    #[test]
    fn floor_honors_an_explicit_request_at_or_above_it() {
        assert_eq!(
            resolve_sandbox(Some("strict"), None, Some("default")).unwrap(),
            SandboxLevel::Strict
        );
        assert_eq!(
            resolve_sandbox(Some("default"), None, Some("default")).unwrap(),
            SandboxLevel::Default
        );
    }

    #[test]
    fn floor_refuses_an_explicit_request_below_it() {
        let err = resolve_sandbox(Some("off"), None, Some("strict")).unwrap_err();
        assert!(err.contains("GLASS_SANDBOX_FLOOR=strict"), "{err}");
        assert!(err.contains("off"), "{err}");
        assert!(resolve_sandbox(Some("default"), None, Some("strict")).is_err());
    }

    #[test]
    fn invalid_floor_or_level_is_an_error() {
        assert!(resolve_sandbox(None, None, Some("bogus")).is_err());
        assert!(resolve_sandbox(Some("bogus"), None, None).is_err());
    }

    #[test]
    fn floor_from_var_maps_present_absent_and_non_utf8() {
        // Present + valid → Some; absent → None (no floor).
        assert_eq!(
            floor_from_var(Ok("strict".to_string())).unwrap(),
            Some("strict".to_string())
        );
        assert_eq!(
            floor_from_var(Err(std::env::VarError::NotPresent)).unwrap(),
            None
        );
        // Set-but-non-UTF-8 must be an ERROR (fail-closed), never silently unset (fail-open) —
        // otherwise a garbled operator floor would silently disable the policy.
        #[cfg(unix)]
        {
            use std::os::unix::ffi::OsStringExt;
            let bad = std::ffi::OsString::from_vec(vec![0x73, 0x80, 0x74]); // invalid UTF-8
            assert!(floor_from_var(Err(std::env::VarError::NotUnicode(bad))).is_err());
        }
    }

    #[test]
    fn start_returns_geometry_json() {
        let mut g = glass_with(FakePlatform::new(80, 60));
        let out = start(&mut g, &start_args()).unwrap();
        let v = assert_envelope(&out, "glass_start");
        assert_eq!(v["width"], json!(80));
        assert_eq!(v["height"], json!(60));
    }

    #[test]
    fn start_rejects_empty_run() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        let mut a = start_args();
        a.run.clear();
        assert!(start(&mut g, &a).is_err());
    }

    #[test]
    fn start_rejects_unknown_sandbox() {
        // Locks rejection at the `glass_start` tool boundary (not just the
        // `resolve_sandbox`/`SandboxLevel::FromStr` units below it) — an unknown
        // `sandbox` must not be silently coerced to the default level.
        let mut g = glass_with(FakePlatform::new(10, 10));
        let mut a = start_args();
        a.sandbox = Some("bogus".into());
        let err = start(&mut g, &a).unwrap_err();
        assert!(err.contains("unknown sandbox level"), "got: {err}");
    }

    #[test]
    fn stop_without_session_errors_with_message() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        let err = stop(&mut g).unwrap_err();
        assert!(err.contains("no active session"));
    }

    #[test]
    fn stop_running_session_returns_empty_envelope() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        start(&mut g, &start_args()).unwrap();
        let out = stop(&mut g).unwrap();
        let v = assert_envelope(&out, "glass_stop");
        assert_eq!(v, json!({}), "envelope: {v}");
    }

    #[test]
    fn window_resize_requires_dimensions() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        start(&mut g, &start_args()).unwrap();
        let a = WindowArgs {
            op: "resize".into(),
            x: None,
            y: None,
            width: None,
            height: None,
        };
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
        let v = assert_envelope(&out, "glass_window");
        assert_eq!(v["width"], json!(33));
        assert_eq!(v["height"], json!(44));
    }

    #[test]
    fn window_rejects_unknown_op() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        start(&mut g, &start_args()).unwrap();
        let a = WindowArgs {
            op: "levitate".into(),
            x: None,
            y: None,
            width: None,
            height: None,
        };
        let err = window(&mut g, &a).unwrap_err();
        assert!(err.contains("unknown window op"), "got: {err}");
    }

    #[test]
    fn parse_button_maps_and_rejects() {
        assert!(matches!(
            parse_button(Some("middle")),
            Ok(MouseButton::Middle)
        ));
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
            a11y: false,
        })
        .unwrap();
        let out = a11y_snapshot(&mut g).unwrap();
        assert_envelope(&out, "glass_a11y_snapshot");
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(
                    t.starts_with(crate::untrusted::NOTE),
                    "must be marked untrusted: {t}"
                );
                assert!(
                    t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"),
                    "enveloped: {t}"
                );
                assert!(t.contains("#0 Window"), "outline: {t}");
                assert!(
                    t.contains("#1 Button \"Save\" (10,10 20x20)"),
                    "outline: {t}"
                );
            }
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn a11y_snapshot_appends_pixel_hint_when_treeless() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), empty_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
        let out = a11y_snapshot(&mut g).unwrap();
        assert_envelope(&out, "glass_a11y_snapshot");
        // [0]=envelope, [1]=untrusted root-only outline, [2]=glass's trusted pixel hint.
        match &out.0[2] {
            OutContent::Text(t) => {
                assert!(t.contains("glass_screenshot"), "pixel hint: {t}");
                assert!(
                    !t.starts_with(crate::untrusted::NOTE),
                    "the hint is glass's own guidance, not untrusted app content: {t}"
                );
            }
            _ => panic!("expected the pixel-hint text"),
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
            a11y: false,
        })
        .unwrap();
        let err = a11y_snapshot(&mut g).unwrap_err();
        assert!(err.contains("not supported"), "msg: {err}");
    }

    #[test]
    fn set_value_tool_ok_and_errors() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
        a11y_snapshot(&mut g).unwrap();
        let out = set_value(
            &mut g,
            &SetValueArgs {
                id: 1,
                text: "hello".into(),
                return_: None,
            },
        )
        .unwrap();
        let v = assert_envelope(&out, "glass_set_value");
        assert_eq!(v["id"], json!(1), "envelope: {v}");
        // unknown id surfaces the actionable message
        let err = set_value(
            &mut g,
            &SetValueArgs {
                id: 99,
                text: "x".into(),
                return_: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("not in the current snapshot"), "msg: {err}");
    }

    #[test]
    fn set_value_tool_rejects_uneditable_and_stale() {
        let spec = AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        };
        // Backend says the element isn't editable: the tool must surface an error,
        // never the "set value" confirmation (a silent successful-looking no-op is
        // the worst failure for an agent that then asserts "value set").
        let mut g = glass_with_a11y_outcome(
            FakePlatform::new(100, 100),
            fake_tree(),
            SetOutcome::NotEditable,
        );
        g.start(&spec).unwrap();
        a11y_snapshot(&mut g).unwrap();
        let err = set_value(
            &mut g,
            &SetValueArgs {
                id: 1,
                text: "x".into(),
                return_: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("not editable"), "msg: {err}");

        // Element changed since the snapshot: same contract — error, not success.
        let mut g = glass_with_a11y_outcome(
            FakePlatform::new(100, 100),
            fake_tree(),
            SetOutcome::Changed,
        );
        g.start(&spec).unwrap();
        a11y_snapshot(&mut g).unwrap();
        let err = set_value(
            &mut g,
            &SetValueArgs {
                id: 1,
                text: "x".into(),
                return_: None,
            },
        )
        .unwrap_err();
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
            a11y: false,
        })
        .unwrap();
        a11y_snapshot(&mut g).unwrap();
        assert!(click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: None
            }
        )
        .is_ok());
        let err = click_element(
            &mut g,
            &ClickElementArgs {
                id: 99,
                return_: None,
            },
        )
        .unwrap_err();
        assert!(err.contains("not in the current snapshot"), "msg: {err}");
    }

    #[test]
    fn a11y_marks_returns_image_and_legend() {
        use glass_core::Frame;
        let platform =
            FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
        let out = a11y_marks(&mut g).unwrap();
        assert!(
            matches!(out.0[0], OutContent::Image(_)),
            "first item is the image"
        );
        let OutContent::Text(t) = &out.0[1] else {
            panic!("expected envelope text as the second item")
        };
        let v: serde_json::Value = serde_json::from_str(t).expect("envelope must be valid JSON");
        assert_eq!(v["ok"], json!(true), "envelope: {v}");
        assert_eq!(v["tool"], json!("glass_a11y_marks"), "envelope: {v}");
        assert_eq!(v["result"]["count"], json!(1), "envelope: {v}");
        match &out.0[2] {
            OutContent::Text(t) => assert!(t.contains("#1 Button \"Save\""), "legend: {t}"),
            _ => panic!("expected legend text"),
        }
    }

    #[test]
    fn a11y_marks_legend_untrusted_wrapped_and_image_note_present() {
        use glass_core::Frame;
        let platform =
            FakePlatform::new(100, 100).with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
        let out = a11y_marks(&mut g).unwrap();
        // [Image, envelope-Text, legend-Text (untrusted-wrapped), IMAGE_NOTE-Text]
        assert!(
            out.0.len() >= 4,
            "expected [Image, envelope, legend, IMAGE_NOTE], got {} items",
            out.0.len()
        );
        assert!(
            matches!(out.0[0], OutContent::Image(_)),
            "image leads: {:?}",
            out.0
        );
        // the trusted envelope comes right after the image
        match &out.0[1] {
            OutContent::Text(t) => {
                let v: serde_json::Value =
                    serde_json::from_str(t).expect("envelope must be valid JSON");
                assert_eq!(v["ok"], json!(true), "envelope: {v}");
                assert_eq!(v["tool"], json!("glass_a11y_marks"), "envelope: {v}");
            }
            _ => panic!("expected envelope text as second item"),
        }
        // legend must be untrusted-wrapped
        match &out.0[2] {
            OutContent::Text(t) => {
                assert!(
                    t.starts_with(crate::untrusted::NOTE),
                    "legend must start with NOTE: {t}"
                );
                assert!(
                    t.contains("⟦untrusted:") && t.contains("⟦/untrusted:"),
                    "legend must be untrusted-wrapped: {t}"
                );
                assert!(
                    t.contains("#1 Button"),
                    "legend must still contain element: {t}"
                );
            }
            _ => panic!("expected legend text as third item"),
        }
        // IMAGE_NOTE must be present
        let has_note = out
            .0
            .iter()
            .any(|c| matches!(c, OutContent::Text(t) if t == crate::untrusted::IMAGE_NOTE));
        assert!(has_note, "IMAGE_NOTE must be present in a11y_marks output");
    }

    pub(crate) fn started_a11y_frames(frames: Vec<glass_core::Frame>) -> Glass {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100).with_frames(frames), fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
        a11y_snapshot(&mut g).unwrap(); // populate last_ax for click_element/set_value
        g
    }

    #[test]
    fn return_none_is_confirmation_only() {
        let mut g = started_a11y_frames(vec![]);
        let out = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: None,
            },
        )
        .unwrap();
        assert_eq!(out.0.len(), 1, "just the envelope, no siblings");
        let v = assert_envelope(&out, "glass_click_element");
        assert_eq!(v["id"], json!(1), "envelope: {v}");
        assert!(v["observed"].is_null(), "envelope: {v}");

        let out2 = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: Some("none".into()),
            },
        )
        .unwrap();
        assert_eq!(out2.0.len(), 1);
    }

    #[test]
    fn return_unknown_errors() {
        let mut g = started_a11y_frames(vec![]);
        let err = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: Some("wat".into()),
            },
        )
        .unwrap_err();
        assert!(err.contains("unknown return"), "msg: {err}");
    }

    #[test]
    fn return_snapshot_appends_tree_and_refreshes_cache() {
        // vec![] → the snapshot arm's best-effort settle can't capture and is swallowed; the
        // tree still folds (the assertions below are unaffected by the settle).
        let mut g = started_a11y_frames(vec![]);
        let out = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: Some("snapshot".into()),
            },
        )
        .unwrap();
        assert_eq!(
            out.0.len(),
            2,
            "envelope + exactly one sibling (the a11y outline)"
        );
        let v = assert_envelope(&out, "glass_click_element");
        assert_eq!(v["id"], json!(1), "envelope: {v}");
        assert!(
            v["observed"].is_null(),
            "snapshot doesn't populate `observed`: {v}"
        );
        match &out.0[1] {
            OutContent::Text(t) => {
                assert!(
                    t.starts_with(crate::untrusted::NOTE),
                    "must be marked untrusted: {t}"
                );
                assert!(
                    t.contains("#1 Button \"Save\""),
                    "a11y outline appended: {t}"
                );
            }
            _ => panic!("expected a11y outline text"),
        }
        // the snapshot refreshed last_ax -> a follow-up id-based action still resolves
        assert!(click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: None
            }
        )
        .is_ok());
    }

    #[test]
    fn return_snapshot_settles_before_folding() {
        use glass_core::Frame;
        use std::sync::{Arc, Mutex};
        // A settleable frame + a capture counter, wired inline (started_a11y_frames doesn't
        // expose a capture log).
        let captures = Arc::new(Mutex::new(0usize));
        let platform = FakePlatform::new(100, 100)
            .with_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])])
            .with_capture_log(captures.clone());
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&AppSpec {
            build: None,
            run: vec!["x".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 1,
            sandbox: SandboxLevel::Off,
            a11y: false,
        })
        .unwrap();
        a11y_snapshot(&mut g).unwrap(); // seed last_ax for click_element
        let before = *captures.lock().unwrap();
        let out = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: Some("snapshot".into()),
            },
        )
        .unwrap();
        // The a11y outline is still folded (envelope + one untrusted sibling) ...
        assert_eq!(out.0.len(), 2, "envelope + a11y outline sibling");
        // ... AND the settle captured frames before the fold. This guards the `wait_stable`
        // line: remove it and `captures` stays at `before`.
        assert!(
            *captures.lock().unwrap() > before,
            "return:snapshot must settle (capture frames) before folding"
        );
    }

    #[test]
    fn return_snapshot_without_frames_still_folds() {
        // No frames → the settle's `wait_stable` errors (no scripted frames); the `let _ =`
        // swallows it and the tree is still folded. A `?` there would deny the tree.
        let mut g = started_a11y_frames(vec![]);
        let out = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: Some("snapshot".into()),
            },
        )
        .unwrap();
        assert_eq!(
            out.0.len(),
            2,
            "tree still folds even when the settle can't run"
        );
    }

    #[test]
    fn return_settle_appends_settled_text() {
        use glass_core::Frame;
        // wait_stable needs frames; one solid frame (repeated by the fake) settles.
        let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let out = click_element(
            &mut g,
            &ClickElementArgs {
                id: 1,
                return_: Some("settle".into()),
            },
        )
        .unwrap();
        assert_eq!(
            out.0.len(),
            1,
            "settle folds into `result.observed`, no extra sibling"
        );
        let v = assert_envelope(&out, "glass_click_element");
        assert_eq!(v["id"], json!(1), "envelope: {v}");
        assert_eq!(v["observed"]["settled"], json!(true), "envelope: {v}");
    }

    #[test]
    fn set_value_return_snapshot() {
        let mut g = started_a11y_frames(vec![]);
        let out = set_value(
            &mut g,
            &SetValueArgs {
                id: 1,
                text: "x".into(),
                return_: Some("snapshot".into()),
            },
        )
        .unwrap();
        let v = assert_envelope(&out, "glass_set_value");
        assert_eq!(v["id"], json!(1), "envelope: {v}");
        assert!(
            matches!(&out.0[1], OutContent::Text(t) if t.starts_with(crate::untrusted::NOTE) && t.contains("#1 Button")),
            "outline appended"
        );
    }

    #[test]
    fn set_value_return_settle_folds_into_observed() {
        use glass_core::Frame;
        // Mirrors `return_settle_appends_settled_text` for `click_element`: wait_stable
        // needs frames; one solid frame (repeated by the fake) settles.
        let mut g = started_a11y_frames(vec![Frame::solid(100, 100, [0, 0, 0, 255])]);
        let out = set_value(
            &mut g,
            &SetValueArgs {
                id: 1,
                text: "x".into(),
                return_: Some("settle".into()),
            },
        )
        .unwrap();
        assert_eq!(
            out.0.len(),
            1,
            "settle folds into `result.observed`, no extra sibling"
        );
        let v = assert_envelope(&out, "glass_set_value");
        assert_eq!(v["id"], json!(1), "envelope: {v}");
        assert_eq!(v["observed"]["settled"], json!(true), "envelope: {v}");
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
            a11y: false,
        })
        .unwrap();

        let out = list_windows(&mut g).unwrap();
        let v = assert_envelope(&out, "glass_list_windows");
        assert_eq!(v["count"], json!(1), "envelope: {v}");
        let text = match &out.0[1] {
            OutContent::Text(t) => t.clone(),
            _ => panic!("expected text"),
        };
        assert!(
            text.starts_with(crate::untrusted::NOTE),
            "must be marked untrusted: {text}"
        );
        assert!(
            text.contains("⟦untrusted:") && text.contains("⟦/untrusted:"),
            "enveloped: {text}"
        );
        assert!(
            text.contains("\"id\":0"),
            "json should list window id 0: {text}"
        );
        assert!(
            text.contains("\"active\":true"),
            "json should mark active: {text}"
        );
        assert!(
            text.contains("\"width\":320"),
            "json should include geometry width: {text}"
        );

        let out = select_window(&mut g, &SelectWindowArgs { id: 0 }).unwrap();
        let v = assert_envelope(&out, "glass_select_window");
        assert_eq!(v["width"], json!(320), "envelope: {v}");
        assert_eq!(v["height"], json!(240), "envelope: {v}");
        assert!(select_window(&mut g, &SelectWindowArgs { id: 42 }).is_err());
    }

    #[test]
    fn result_envelope_is_leading_and_shaped() {
        let out = ToolOutput::result("glass_stop", serde_json::json!({}));
        let OutContent::Text(t) = &out.0[0] else {
            panic!("expected text")
        };
        let v: serde_json::Value = serde_json::from_str(t).unwrap();
        assert_eq!(v["ok"], serde_json::json!(true));
        assert_eq!(v["tool"], serde_json::json!("glass_stop"));
        assert_eq!(v["result"], serde_json::json!({}));
    }

    #[test]
    fn result_with_puts_envelope_first_then_extra() {
        let out = ToolOutput::result_with(
            "glass_screenshot",
            serde_json::json!({ "width": 4, "height": 4 }),
            vec![OutContent::Image(vec![1, 2, 3])],
        );
        assert!(matches!(out.0[0], OutContent::Text(_)), "envelope leads");
        assert!(matches!(out.0[1], OutContent::Image(_)), "extra follows");
    }
}
