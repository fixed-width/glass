//! Pure tool logic: parse already-deserialized args, call `Glass`, format
//! results. No rmcp, no X11 — unit-tested against a fake `Platform`. Every
//! failure returns `Err(String)` (an agent-readable message); the server shell
//! turns that into an MCP error result. Never a silent success.
//!
use std::path::PathBuf;
use std::time::{Duration, Instant};

use glass_core::{
    AppSpec, AxNodeId, BoundDispatch, Glass, MarkLabel, MouseButton, WindowGeometry, WindowHint,
    WindowId, WindowOp, frame_to_webp,
};
use serde::Serialize;
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

#[derive(Clone, Copy, Debug)]
pub(crate) struct ToolContext {
    pub deadline: glass_core::Deadline,
}

impl ToolContext {
    pub const UNBOUNDED: Self = Self {
        deadline: glass_core::Deadline::UNBOUNDED,
    };
}

#[derive(Debug)]
pub(crate) struct ContextualOutput {
    pub output: ToolOutput,
    pub timed_out_by: Option<glass_core::Whose>,
}

impl ContextualOutput {
    pub fn immediate(output: ToolOutput) -> Self {
        Self {
            output,
            timed_out_by: None,
        }
    }

    pub fn with_timeout(output: ToolOutput, timed_out_by: Option<glass_core::Whose>) -> Self {
        Self {
            output,
            timed_out_by,
        }
    }
}

#[derive(Debug)]
pub(crate) struct ContextualError {
    pub message: String,
    pub category: SafeErrorCategory,
    pub safe_summary: &'static str,
    pub sequence_deadline_exceeded: bool,
    pub bound_dispatch: Option<BoundDispatch>,
    post_write: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum SafeErrorCategory {
    NoActiveSession,
    StaleElement,
    NotEditable,
    InvalidValue,
    OptionNotFound,
    UnsupportedAccessibility,
    PermissionDenied,
    TransportFailure,
    ActionDeadlineExceeded,
    SequenceDeadlineExceeded,
    Other,
}

impl SafeErrorCategory {
    fn from_error(error: &glass_core::GlassError) -> Self {
        if error.bound_owner() == Some(glass_core::Whose::Caller) {
            return Self::SequenceDeadlineExceeded;
        }

        match error.cause() {
            glass_core::GlassError::NoActiveSession => Self::NoActiveSession,
            glass_core::GlassError::NoAxSnapshot
            | glass_core::GlassError::AxElementNotFound(_)
            | glass_core::GlassError::AxElementChanged(_)
            | glass_core::GlassError::AxElementGone(_) => Self::StaleElement,
            glass_core::GlassError::AxElementNotEditable(_) => Self::NotEditable,
            glass_core::GlassError::AxValueNotBoolean(..) => Self::InvalidValue,
            glass_core::GlassError::AxOptionNotFound(..) => Self::OptionNotFound,
            glass_core::GlassError::AxUnsupported
            | glass_core::GlassError::AxActionUnavailable(_) => Self::UnsupportedAccessibility,
            glass_core::GlassError::PermissionDenied { .. } => Self::PermissionDenied,
            glass_core::GlassError::CaptureFailed(_)
            | glass_core::GlassError::AccessibilityUnavailable(_)
            | glass_core::GlassError::Backend(_)
            | glass_core::GlassError::ToolFailed { .. }
            | glass_core::GlassError::Bounded { .. }
            | glass_core::GlassError::Io(_) => Self::TransportFailure,
            _ => Self::Other,
        }
    }

    fn summary(self) -> &'static str {
        match self {
            Self::NoActiveSession => "no active session",
            Self::StaleElement => "element is stale or missing",
            Self::NotEditable => "element is not editable",
            Self::InvalidValue => "element expects a boolean value",
            Self::OptionNotFound => "requested option was not found",
            Self::UnsupportedAccessibility => "accessibility operation is unsupported",
            Self::PermissionDenied => "permission denied",
            Self::TransportFailure => "backend transport failed",
            Self::ActionDeadlineExceeded => "action deadline exceeded",
            Self::SequenceDeadlineExceeded => "sequence deadline exceeded",
            Self::Other => "action execution failed",
        }
    }

    fn post_write_summary(self) -> &'static str {
        match self {
            Self::TransportFailure => {
                "backend transport failed after the value write went out; re-snapshot to see where it landed rather than writing it again"
            }
            Self::SequenceDeadlineExceeded => {
                "sequence deadline exceeded after the value write went out; re-snapshot to see where it landed rather than writing it again"
            }
            Self::ActionDeadlineExceeded => {
                "action deadline exceeded after the value write went out; re-snapshot to see where it landed rather than writing it again"
            }
            _ => {
                "the value write went out but could not be confirmed; re-snapshot to see where it landed rather than writing it again"
            }
        }
    }
}

impl ContextualError {
    pub fn validation(message: String) -> Self {
        Self {
            message,
            category: SafeErrorCategory::Other,
            safe_summary: "action validation failed",
            sequence_deadline_exceeded: false,
            bound_dispatch: Some(BoundDispatch::NotDispatched),
            post_write: false,
        }
    }

    fn from_error(error: glass_core::GlassError) -> Self {
        let post_write = error.set_value_failed_after_writing();
        let category = SafeErrorCategory::from_error(&error);
        // These variants are constructed only by core preflight checks before the requested
        // mutation can dispatch. Keep the allowlist narrow so a new ordinary error remains
        // conservatively ambiguous.
        let bound_dispatch = error.bound_dispatch().or_else(|| {
            matches!(
                &error,
                glass_core::GlassError::CoordOutOfBounds { .. }
                    | glass_core::GlassError::AxValueNotBoolean(..)
            )
            .then_some(BoundDispatch::NotDispatched)
        });
        Self {
            message: error.to_string(),
            category,
            safe_summary: if post_write {
                category.post_write_summary()
            } else {
                category.summary()
            },
            sequence_deadline_exceeded: error.bound_owner() == Some(glass_core::Whose::Caller),
            bound_dispatch,
            post_write,
        }
    }

    pub fn from_core(error: glass_core::GlassError, _context: ToolContext) -> Self {
        Self::from_error(error)
    }

    pub fn from_caller_bound(error: glass_core::GlassError, _context: ToolContext) -> Self {
        Self::from_error(error)
    }

    pub fn from_resolved_bound(
        error: glass_core::GlassError,
        context: ToolContext,
        whose: glass_core::Whose,
    ) -> Self {
        let bounded = error.bound().is_some();
        let mut out = Self::from_error(error);
        if context.deadline.has_passed() {
            return out.after_sequence_deadline();
        }
        if whose == glass_core::Whose::Callee && bounded {
            out.category = SafeErrorCategory::ActionDeadlineExceeded;
            out.safe_summary = if out.post_write {
                SafeErrorCategory::ActionDeadlineExceeded.post_write_summary()
            } else {
                SafeErrorCategory::ActionDeadlineExceeded.summary()
            };
            out.sequence_deadline_exceeded = false;
        }
        out
    }

    pub fn after_dispatch(mut self) -> Self {
        self.bound_dispatch = Some(BoundDispatch::MayHaveDispatched);
        self
    }

    /// Reclassify an error observed after the enclosing batch deadline without discarding the
    /// operation's safe detail or its dispatch verdict.
    pub fn after_sequence_deadline(mut self) -> Self {
        self.category = SafeErrorCategory::SequenceDeadlineExceeded;
        self.safe_summary = if self.post_write {
            SafeErrorCategory::SequenceDeadlineExceeded.post_write_summary()
        } else {
            SafeErrorCategory::SequenceDeadlineExceeded.summary()
        };
        self.sequence_deadline_exceeded = true;
        self
    }

    pub fn annotate(mut self, prefix: &str) -> Self {
        self.message = format!("{prefix}: {}", self.message);
        self
    }

    pub fn sequence_deadline(message: String) -> Self {
        Self {
            message,
            category: SafeErrorCategory::SequenceDeadlineExceeded,
            safe_summary: SafeErrorCategory::SequenceDeadlineExceeded.summary(),
            sequence_deadline_exceeded: true,
            bound_dispatch: None,
            post_write: false,
        }
    }
}

pub(crate) type ContextualToolResult = Result<ContextualOutput, ContextualError>;
type ReturnObservation = (
    Option<serde_json::Value>,
    Vec<OutContent>,
    Option<glass_core::Whose>,
);

fn erase_context(result: ContextualToolResult) -> ToolResult {
    result.map(|o| o.output).map_err(|e| e.message)
}

/// Batch tool result: either outcome may carry structured MCP content.
pub(crate) type BatchToolResult = Result<ToolOutput, ToolOutput>;

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
    if spec.a11y
        && a.a11y.is_none()
        && let Err(why) = a11y_bus_preflight()
    {
        eprintln!(
            "glass: accessibility is on by default but this host can't start its bus \
                 ({why}); launching without it — pass a11y:true to require it."
        );
        spec.a11y = false;
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

/// The truncation steer for a snapshot outline: the core notice plus the MCP-level recourse
/// (how to widen the cap). Kept here, not in `glass-core`, so core stays tool-agnostic; used by
/// both `a11y_snapshot` and the `return:"snapshot"` fold so both disclose the recourse identically.
fn a11y_truncation_steer(tree: &glass_core::AxTree) -> Option<String> {
    let notice = tree.truncation_notice()?;
    // Only a Nodes truncation is raisable via `max_nodes`. A Depth/Siblings hit is structural
    // (max_nodes doesn't touch those rails), and the core notice already steers to narrowing the
    // UI / driving by pixels — so don't dangle a `max_nodes` recourse that wouldn't help.
    let raisable = matches!(
        tree.truncated.map(|t| t.limit),
        Some(glass_core::TruncationLimit::Nodes)
    );
    Some(if raisable {
        format!("{notice} Pass max_nodes to raise the limit, or max_nodes: 0 for the full tree.")
    } else {
        notice
    })
}

/// Every disclosure a snapshot owes the agent: the elements it does not show, the content the app
/// withheld, the web content it could not enter, and what it turned out to describe. One function,
/// not four call-site lists, so `a11y_snapshot` and the `return:"snapshot"` fold disclose
/// identically.
///
/// Order: truncation, unreadable, unexposed, document, subject.
fn a11y_steers(tree: &glass_core::AxTree) -> Vec<String> {
    [
        a11y_truncation_steer(tree),
        tree.unreadable_notice(),
        tree.unexposed_notice(),
        tree.document_guidance(),
        tree.subject_notice(),
    ]
    .into_iter()
    .flatten()
    .collect()
}

pub fn a11y_snapshot(glass: &mut Glass, a: &A11ySnapshotArgs) -> ToolResult {
    let tree = glass
        .a11y_snapshot(a.max_nodes.map(|n| n as usize))
        .map_err(|e| e.to_string())?;
    // The agent-facing render: wrapper chains collapsed. The session cache keeps the full
    // tree, so every elided node is still addressable by id. Truncation is disclosed
    // separately below, not baked into this text — see the comment there.
    let body = glass_core::outline::render_compact(&tree);
    // The outline is app-derived → untrusted-wrapped. glass's own steers (the empty-tree
    // hint, the truncation notice) are trusted, separate, unwrapped blocks — never baked
    // into the untrusted-wrapped body, or an instruction of glass's own ("drive by
    // pixels…") would end up under a directive telling the agent to ignore instructions
    // in that block.
    let mut contents = vec![OutContent::Text(crate::untrusted::wrap_untrusted(&body))];
    if let Some(hint) = tree.empty_guidance() {
        contents.push(OutContent::Text(hint.to_string()));
    }
    contents.extend(a11y_steers(&tree).into_iter().map(OutContent::Text));
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

/// Reject an unknown `return` value without touching the session. For a tool whose
/// action mutates the app (typing, clicking), call this BEFORE acting — a bad
/// argument must not leave the action applied, or an agent retrying the errored
/// call applies it twice.
pub(crate) fn validate_return(ret: Option<&str>) -> Result<(), String> {
    match ret {
        None | Some("none" | "settle" | "snapshot") => Ok(()),
        Some(o) => Err(format!("unknown return '{o}' (use none/settle/snapshot)")),
    }
}

/// Apply the optional `return` observe. `settle` → `Some(metadata)` to merge under
/// `result.observed`; `snapshot` → an untrusted outline sibling to append; none/absent
/// → neither. Calls the `Glass` methods directly (not the `a11y_snapshot`/`wait_stable`
/// tool functions) so a composed observe never nests another envelope. Unknown value
/// → `Err` (unchanged rejection).
fn resolve_return_with(
    glass: &mut Glass,
    ret: Option<&str>,
    context: ToolContext,
) -> Result<ReturnObservation, ContextualError> {
    match ret {
        None | Some("none") => Ok((None, vec![], None)),
        Some("settle") => {
            let (_, whose) = context.deadline.budget(
                Duration::from_millis(settle_params().timeout_ms),
                Instant::now(),
            );
            let o = glass
                .wait_stable_by(&settle_params(), context.deadline)
                .map_err(|e| ContextualError::from_resolved_bound(e, context, whose))?;
            let timed_out_by = (!o.settled).then_some(whose);
            Ok((
                Some(serde_json::json!({
                    "settled": o.settled,
                    "saw_motion": o.saw_motion,
                    "observed_ms": o.observed_ms,
                })),
                vec![],
                timed_out_by,
            ))
        }
        Some("snapshot") => {
            // Let the UI settle before folding the tree so a screen-changing action (a
            // navigating click) doesn't fold a mid-transition tree. `wait_stable` soft-times-out
            // (`settled:false`, not an error) on a non-settling UI; real capture/backend failures
            // propagate because the requested observation did not complete reliably.
            let (_, whose) = context.deadline.budget(
                Duration::from_millis(settle_params().timeout_ms),
                Instant::now(),
            );
            match glass.wait_stable_by(&settle_params(), context.deadline) {
                Ok(o) if !o.settled && whose == glass_core::Whose::Caller => {
                    return Err(ContextualError::sequence_deadline(
                        "return snapshot settle reached the caller deadline".into(),
                    ));
                }
                Err(error) => {
                    return Err(ContextualError::from_resolved_bound(error, context, whose));
                }
                Ok(_) => {}
            }
            // Reuse the session's current limits so a fold after a raised/unbounded snapshot
            // isn't silently re-truncated to the default cap.
            let tree = glass
                .a11y_resnapshot(context.deadline)
                .map_err(|e| ContextualError::from_core(e, context))?;
            // Same shape as `a11y_snapshot`: the app-derived outline stays untrusted-wrapped;
            // glass's own steers are separate trusted blocks, not baked into that body.
            let mut extra = vec![OutContent::Text(crate::untrusted::wrap_untrusted(
                &glass_core::outline::render_compact(&tree),
            ))];
            if let Some(hint) = tree.empty_guidance() {
                extra.push(OutContent::Text(hint.to_string()));
            }
            extra.extend(a11y_steers(&tree).into_iter().map(OutContent::Text));
            Ok((None, extra, None))
        }
        Some(o) => Err(ContextualError::validation(format!(
            "unknown return '{o}' (use none/settle/snapshot)"
        ))),
    }
}

pub fn click_element(glass: &mut Glass, a: &ClickElementArgs) -> ToolResult {
    erase_context(click_element_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn click_element_with(
    glass: &mut Glass,
    a: &ClickElementArgs,
    context: ToolContext,
) -> ContextualToolResult {
    // Bad `return` value → reject before the click lands (see `validate_return`).
    validate_return(a.return_.as_deref()).map_err(ContextualError::validation)?;
    let method = glass
        .click_element_by(AxNodeId(a.id), context.deadline)
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    let (observed, extra, timed_out_by) = resolve_return_with(glass, a.return_.as_deref(), context)
        .map_err(ContextualError::after_dispatch)?;
    let mut result = serde_json::json!({ "id": a.id, "method": method.label() });
    if let Some(reason) = method.native_fallback() {
        result["native_fallback"] = serde_json::json!(reason);
    }
    if let Some(actuated) = method.actuated() {
        result["actuated_id"] = serde_json::json!(actuated.0);
    }
    if let Some(o) = observed {
        result["observed"] = o;
    }
    Ok(ContextualOutput::with_timeout(
        ToolOutput::result_with("glass_click_element", result, extra),
        timed_out_by,
    ))
}

pub fn set_value(glass: &mut Glass, a: &SetValueArgs) -> ToolResult {
    erase_context(set_value_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn set_value_with(
    glass: &mut Glass,
    a: &SetValueArgs,
    context: ToolContext,
) -> ContextualToolResult {
    // Bad `return` value → reject before the value is written (see `validate_return`).
    validate_return(a.return_.as_deref()).map_err(ContextualError::validation)?;
    glass
        .set_value_by(AxNodeId(a.id), &a.text, context.deadline)
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    let (observed, extra, timed_out_by) = resolve_return_with(glass, a.return_.as_deref(), context)
        .map_err(ContextualError::after_dispatch)?;
    let mut result = serde_json::json!({ "id": a.id });
    if let Some(o) = observed {
        result["observed"] = o;
    }
    Ok(ContextualOutput::with_timeout(
        ToolOutput::result_with("glass_set_value", result, extra),
        timed_out_by,
    ))
}

pub fn a11y_marks(glass: &mut Glass) -> ToolResult {
    let (frame, marks) = glass.a11y_marks().map_err(|e| e.to_string())?;
    let img = frame_to_webp(&frame).map_err(|e| e.to_string())?;
    let legend = if marks.is_empty() {
        "0 interactable elements".to_string()
    } else {
        marks
            .iter()
            // Same spelling as the outline's line: a quoted bare label is the accessible
            // name a `name:` selector matches, `desc="…"` is a description and matches
            // nothing.
            .map(|m| match &m.label {
                Some(MarkLabel::Name(name)) => format!("#{} {:?} {name:?}", m.id.0, m.role),
                Some(MarkLabel::Description(d)) => format!("#{} {:?} desc={d:?}", m.id.0, m.role),
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
#[allow(unused_imports)]
pub use self::find::*;
pub use self::input::*;
pub use self::wait::*;

mod batch;
mod capture; // filled in Task 6
mod clipboard;
#[allow(dead_code)]
mod find;
mod input; // filled in Task 5
mod wait;

#[cfg(test)]
pub(crate) mod testutil;

#[cfg(test)]
mod tests;
