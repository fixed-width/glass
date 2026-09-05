//! Tool-argument structs. Each derives `Deserialize` (parse JSON args) and
//! `JsonSchema` (so MCP advertises a schema to the agent).

use glass_core::Region;
use schemars::JsonSchema;
use serde::{Deserialize, Deserializer, de::Error as _};

pub(crate) const MAX_CLICK_COUNT: u32 = glass_core::MAX_CLICK_COUNT;
pub(crate) const MAX_SCROLL_NOTCHES: i32 = glass_core::MAX_SCROLL_NOTCHES as i32;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RegionArgs {
    /// Left edge in window-relative pixels.
    pub x: u32,
    /// Top edge in window-relative pixels.
    pub y: u32,
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
}

impl From<&RegionArgs> for Region {
    fn from(a: &RegionArgs) -> Self {
        Region {
            x: a.x,
            y: a.y,
            width: a.width,
            height: a.height,
        }
    }
}

/// Window-relative ignore rects as core `Region`s; empty when omitted.
pub fn ignore_regions(args: Option<&[RegionArgs]>) -> Vec<Region> {
    args.map(|v| v.iter().map(Region::from).collect())
        .unwrap_or_default()
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScreenshotArgs {
    /// Optional window-relative sub-rectangle to capture; omit for the whole window.
    pub region: Option<RegionArgs>,
    /// Observe this current glass_list_windows ID without selecting it; omit for active window.
    pub window_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WindowHintArgs {
    /// Case-insensitive title substring; can locate a window handed off to an unrelated process.
    pub title: Option<String>,
    /// Exact window-class match.
    pub class: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartArgs {
    /// Optional shell command to run (in `cwd`) before launching.
    pub build: Option<String>,
    /// Desktop: [executable, args...]. iOS: [.app-or-bundle-id, args...]. Android: [apk?, package/.Activity] in either order, e.g. ["/absolute/path/app.apk", "com.example.app/.MainActivity"].
    pub run: Vec<String>,
    /// x11/wayland (Linux), windows (Windows), macos (macOS), android (any host), ios (macOS). Default: GLASS_BACKEND, else host default (x11 on Linux).
    pub backend: Option<String>,
    /// default: filesystem/process containment, network on; strict: also no network; off: uncontained. Default GLASS_SANDBOX or default. GLASS_SANDBOX_FLOOR raises omitted levels and refuses explicit lower levels.
    pub sandbox: Option<String>,
    /// Working directory for build and app; defaults to the server directory.
    pub cwd: Option<String>,
    /// Extra {KEY: VALUE} environment for build and app. Android applies it only to the host build, not the app.
    #[serde(default)]
    pub env: std::collections::BTreeMap<String, String>,
    /// Select a window by title/class, including process handoffs. Omit for the first window owned by the process or a followable descendant.
    pub window_hint: Option<WindowHintArgs>,
    /// Window-publication timeout in ms (default 10000); does not bound build.
    pub timeout_ms: Option<u64>,
    /// Enable the private accessibility bus (default true). False skips it for canvas-only apps. Linux only; other backends read accessibility ambiently.
    #[serde(default)]
    pub a11y: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WindowArgs {
    /// One of: "focus", "resize", "move", "geometry".
    pub op: String,
    /// Screen-relative left edge; required only for move.
    pub x: Option<i32>,
    /// Screen-relative top edge; required only for move.
    pub y: Option<i32>,
    /// Width in pixels; required only for resize.
    pub width: Option<u32>,
    /// Height in pixels; required only for resize.
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SelectWindowArgs {
    /// Current ID from glass_list_windows; re-list after window changes.
    pub id: u64,
}

#[derive(Clone, Copy, Debug, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
pub enum ActionModeArg {
    Auto,
    Native,
    Pointer,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionScopeArgs {
    pub query: Option<String>,
    pub role: Option<String>,
    pub states: Option<Vec<String>>,
}

#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct ActionTargetArgs {
    pub query: Option<String>,
    pub role: Option<String>,
    pub states: Option<Vec<String>>,
    pub within: Option<ActionScopeArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickElementArgs {
    /// Latest snapshot ID, exclusive with target; re-read after UI changes. Native action may focus text editors; fallback clicks the center. Popover actions restore the prior window.
    pub id: Option<u32>,
    pub target: Option<ActionTargetArgs>,
    pub mode: Option<ActionModeArg>,
    #[schemars(range(min = 0, max = 120000))]
    pub timeout_ms: Option<u64>,
    pub max_nodes: Option<u32>,
    /// none (default): no observation; settle: text-only visual stability; snapshot: settle and refresh/fold the accessibility tree.
    #[serde(rename = "return")]
    pub return_: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetValueArgs {
    /// Latest snapshot ID, exclusive with target.
    pub id: Option<u32>,
    pub target: Option<ActionTargetArgs>,
    /// Text, slider/spin number, toggle boolean (true/false/on/off/1/0), or case-insensitive combo option label. Toggle writes are idempotent; combos open and choose the option.
    pub text: String,
    #[schemars(range(min = 0, max = 120000))]
    pub timeout_ms: Option<u64>,
    pub max_nodes: Option<u32>,
    /// none (default): no observation; settle: text-only visual stability; snapshot: settle and refresh/fold the accessibility tree.
    #[serde(rename = "return")]
    pub return_: Option<String>,
}

/// Arguments for `glass_find_elements` selector fields.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct FindSelectorArgs {
    /// Case-insensitive substring searched across accessible name, description and non-secure value.
    pub query: Option<String>,
    /// Normalized accessibility role, e.g. "Button" or "Document".
    pub role: Option<String>,
    /// State predicates combined with AND, e.g. ["visible", "enabled"].
    pub states: Option<Vec<String>>,
}

/// Arguments for `glass_find_elements`.
#[derive(Clone, Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
pub struct FindElementsArgs {
    /// Approximate case-insensitive semantic text. Optional when role or states are supplied.
    pub query: Option<String>,
    /// Normalized target role.
    pub role: Option<String>,
    /// Target state predicates combined with AND.
    pub states: Option<Vec<String>>,
    /// Optional unique semantic scope resolved in the same fresh tree.
    pub within: Option<FindSelectorArgs>,
    /// Maximum ranked matches before the byte budget; default 10, range 1 through 20.
    #[schemars(range(min = 1, max = 20))]
    pub max_results: Option<u32>,
    /// Existing accessibility walk limit semantics; 0 removes the node-count limit.
    pub max_nodes: Option<u32>,
    /// Optional wait for at least one match; default 0 performs one fresh read.
    pub timeout_ms: Option<u64>,
}

/// Arguments for `glass_a11y_snapshot`.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct A11ySnapshotArgs {
    /// Node cap; omit for server default, 0 for unlimited. Changing the cap renumbers IDs; re-read them.
    pub max_nodes: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickArgs {
    /// Click x in window-relative pixels.
    pub x: i32,
    /// Click y in window-relative pixels.
    pub y: i32,
    /// "left" (default), "right", or "middle".
    pub button: Option<String>,
    /// Click count (default 1, range 1 through 10); 2 double-clicks.
    #[schemars(range(min = 1, max = 10))]
    pub count: Option<u32>,
    /// Held modifiers, e.g. ["ctrl", "shift"].
    pub modifiers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveArgs {
    /// Destination x in window-relative pixels.
    pub x: i32,
    /// Destination y in window-relative pixels.
    pub y: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DragArgs {
    /// Press x in window-relative pixels.
    pub x1: i32,
    /// Press y in window-relative pixels.
    pub y1: i32,
    /// Release x in window-relative pixels.
    pub x2: i32,
    /// Release y in window-relative pixels.
    pub y2: i32,
    /// Button held for the drag: "left" (default), "right", or "middle".
    pub button: Option<String>,
    /// Held modifiers, e.g. ["ctrl", "shift"].
    pub modifiers: Option<Vec<String>>,
    /// Motion duration in ms (default 200). Faster motion gives the app fewer sampled frames.
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PointerArgs {
    /// Window-relative start point.
    pub from: PointArg,
    /// Window-relative end point. Equal to `from` = a finger held in place.
    pub to: PointArg,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct PointArg {
    /// Window-relative x in pixels.
    pub x: i32,
    /// Window-relative y in pixels.
    pub y: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct GestureArgs {
    /// 2–10 simultaneous pointers; each a straight from→to segment. Pinch = two pointers
    /// moving toward/apart; rotate = two on an arc; two-finger swipe = two parallel segments.
    pub pointers: Vec<PointerArgs>,
    /// Span the gesture over this many ms (all pointers down at 0, up at duration). Default 250.
    pub duration_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrollArgs {
    /// Window-relative anchor x; selects the container under the pointer.
    pub x: i32,
    /// Window-relative anchor y.
    pub y: i32,
    /// Horizontal wheel notches, -100 through 100 (positive right, negative left); typically 1-5, not pixels.
    #[schemars(range(min = -100, max = 100))]
    pub dx: Option<i32>,
    /// Vertical wheel notches, -100 through 100 (positive down, negative up); typically 1-5. App determines distance/zoom.
    #[schemars(range(min = -100, max = 100))]
    pub dy: Option<i32>,
    /// Held modifiers, e.g. ["ctrl", "shift"].
    pub modifiers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeArgs {
    /// Synthetic keystrokes, not paste. When `target` is omitted: current keyboard focus. When `target` is supplied: resolves and focuses the target, confirms focus, then types. Newlines do not press Return.
    pub text: String,
    pub target: Option<ActionTargetArgs>,
    pub focus_mode: Option<ActionModeArg>,
    #[schemars(range(min = 0, max = 120000))]
    pub timeout_ms: Option<u64>,
    pub max_nodes: Option<u32>,
    /// none (default): no observation; settle: text-only visual stability; snapshot: settle and refresh/fold the accessibility tree.
    #[serde(rename = "return")]
    pub return_: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyArgs {
    /// Modifiers plus one key: ctrl+s, Return, alt+F4. Modifiers: ctrl/shift/alt/super (cmd/win/meta aliases). Key: named key, F1-F12 or printable ASCII; case-insensitive names.
    pub chord: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardSetArgs {
    /// The text to write to the clipboard.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitStableArgs {
    /// How long to wait between capture ticks (default 100ms).
    pub interval_ms: Option<u64>,
    /// Consecutive unchanged frames required (default 3).
    pub settle_frames: Option<u32>,
    /// Per-channel difference allowed (0-255, default 0).
    pub tolerance: Option<u8>,
    /// Give up after this long (default 5000ms); returns `{settled:false}` rather
    /// than erroring.
    pub timeout_ms: Option<u64>,
    /// Optional window-relative sub-rectangle for the returned frame.
    pub region: Option<RegionArgs>,
    /// Window-relative area watched for settling, independent of returned-image region.
    pub stability_region: Option<RegionArgs>,
    /// Return image (default true). False returns settled/saw_motion/observed_ms/ignored_pixels/dimensions as text; region then has no effect.
    pub include_image: Option<bool>,
    /// Observe this current glass_list_windows ID without selecting it; omit for active window.
    pub window_id: Option<u64>,
    /// Window-relative rects excluded from comparison and saw_motion, intersected with stability_region. Off-area rects clamp/drop silently; check ignored_pixels for misplaced masks.
    pub ignore: Option<Vec<RegionArgs>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForElementArgs {
    /// Substring of the element's accessible name (selector).
    pub name: Option<String>,
    /// Accessible-description substring; can select unnamed controls.
    pub description: Option<String>,
    /// Element role filter, e.g. "Button", "ProgressBar", "Document" (selector).
    pub role: Option<String>,
    /// Default appears. appears|disappears|enabled|disabled|checked|unchecked|selected|unselected|expanded|collapsed|focused|visible|hidden. checked/unchecked require a real checkable state.
    pub condition: Option<String>,
    /// Exact case-sensitive accessible value, requiring another selector and excluding `value_contains`.
    pub value: Option<String>,
    /// Value substring; requires name, description and/or role; exclusive with value.
    pub value_contains: Option<String>,
    /// Poll interval (default 200ms — an a11y snapshot per tick).
    pub interval_ms: Option<u64>,
    /// Timeout in ms (default 10000). Standalone: matched:false; batched: fails the sequence.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrollToElementArgs {
    /// Substring of the target element's accessible name (selector).
    pub name: Option<String>,
    /// Accessible-description substring; can select unnamed controls.
    pub description: Option<String>,
    /// Element role filter, e.g. "ListItem", "Button", "Document" (selector).
    pub role: Option<String>,
    /// Value substring; requires name, description and/or role.
    pub value_contains: Option<String>,
    /// up/down/left/right. Default infers off-screen direction, or down then up if absent. Reverses at the first end.
    pub direction: Option<String>,
    /// Window-relative anchor x; supply both x/y for a container. Default target row/column, or window center if absent.
    pub x: Option<i32>,
    /// Scroll anchor y (window-relative). See `x`.
    pub y: Option<i32>,
    /// Wheel notches per step (default 3); large steps can skip realized rows.
    pub step: Option<u32>,
    /// Timeout in ms (default 20000). Standalone: matched:false; batched: fails the sequence.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForRegionArgs {
    /// Saved baseline name to compare against; omit to use the frame at call start.
    pub baseline: Option<String>,
    /// Window-relative sub-rectangle to watch; omit for the whole window.
    pub region: Option<RegionArgs>,
    /// "changes" (default; diverge from reference) or "matches" (converge to baseline).
    pub until: Option<String>,
    /// "perceptual" (default) or "exact".
    pub mode: Option<String>,
    /// Perceptual sensitivity (default 0.1; smaller = stricter).
    pub threshold: Option<f32>,
    /// Exact per-channel tolerance (default 0).
    pub tolerance: Option<u8>,
    /// Poll interval (default 100ms).
    pub interval_ms: Option<u64>,
    /// Give up after this long (default 10000ms); returns `{matched:false}`.
    pub timeout_ms: Option<u64>,
    /// On match, also return the watched region as an image (default false).
    pub include_image: Option<bool>,
    /// Observe this current glass_list_windows ID without selecting it; omit for active window.
    pub window_id: Option<u64>,
    /// Window-relative excluded rects intersected with region. Off-area rects clamp/drop silently; check ignored_pixels. changed_pct uses remaining pixels.
    pub ignore: Option<Vec<RegionArgs>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForLogArgs {
    /// Substring to wait for (required, non-empty).
    pub contains: String,
    /// "stdout", "stderr", or "both" (default).
    pub stream: Option<String>,
    /// Start scanning from this cursor (from a prior glass_logs). Omit to match
    /// only lines emitted after this call.
    pub cursor: Option<u64>,
    /// Poll interval (default 100ms).
    pub interval_ms: Option<u64>,
    /// Give up after this long (default 10000ms); returns `{matched:false}`.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct BaselineSaveArgs {
    /// ASCII letters/digits/-/_ only. Replaces an existing baseline silently; used by glass_diff and glass_wait_for_region.
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DoctorArgs {
    /// Start and tear down the default display to prove it starts (default false).
    pub deep: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct CapabilitiesArgs {
    /// x11, wayland, windows, macos, android or ios. Default active/default backend. Valid but unbuilt backends report available:false.
    pub backend: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffArgs {
    /// Saved baseline name; unsaved names error.
    pub name: String,
    /// `"perceptual"` (default) or `"exact"`.
    pub mode: Option<String>,
    /// Perceptual sensitivity for `mode="perceptual"`, 0..1 (default 0.1; smaller = stricter).
    pub threshold: Option<f32>,
    /// Per-channel tolerance for `mode="exact"` (default 0).
    pub tolerance: Option<u8>,
    /// Also return the current frame cropped to the changed region (default
    /// false). No image is returned when nothing changed.
    pub include_image: Option<bool>,
    /// Window-relative comparison area; omit for whole window. Returned bbox is region-relative.
    pub region: Option<RegionArgs>,
    /// Window-relative excluded rects intersected with region. changed_pct uses remaining pixels; ignored_pixels reports the excluded count.
    pub ignore: Option<Vec<RegionArgs>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogsArgs {
    /// Resume from a prior returned cursor; omit for oldest buffered line.
    pub cursor: Option<u64>,
    /// Line cap (default 200). Returned cursor resumes at the first unread line.
    pub max_lines: Option<u32>,
    /// "stdout", "stderr", or "both" (default).
    pub stream: Option<String>,
    /// Case-sensitive substring filter applied before the line cap.
    pub contains: Option<String>,
}

/// One action in a `glass_do` sequence. Internally tagged by `action`; each
/// variant carries the same fields as the standalone tool.
#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum Action {
    Click(ClickArgs),
    Move(MoveArgs),
    Drag(DragArgs),
    Scroll(ScrollArgs),
    Type(TypeArgs),
    Key(KeyArgs),
    Settle(SettleArgs),
    ClickElement(ClickElementArgs),
    SetValue(SetValueArgs),
    WaitForElement(WaitForElementArgs),
    ScrollToElement(ScrollToElementArgs),
}

/// A mid-sequence or terminal settle — the `wait_stable` knobs, no image/return.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SettleArgs {
    /// How long to wait between capture ticks (default 100ms).
    pub interval_ms: Option<u64>,
    /// Consecutive unchanged frames required before the UI counts as settled (default 3).
    pub settle_frames: Option<u32>,
    /// Per-channel difference allowed (0-255, default 0).
    pub tolerance: Option<u8>,
    /// Timeout in ms (default 5000) completes with settled:false. The overall glass_do deadline instead fails the sequence.
    pub timeout_ms: Option<u64>,
    /// Window-relative area watched for settling.
    pub stability_region: Option<RegionArgs>,
    /// Window-relative excluded rects intersected with stability_region. Off-area rects clamp/drop silently; check placement.
    pub ignore: Option<Vec<RegionArgs>>,
}

/// Optional terminal observe after a `glass_do` sequence (run settle → diff →
/// screenshot). All text-first; only `screenshot` (or `diff` with its own
/// `include_image`) returns an image.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThenArgs {
    /// Wait for quiescence before diff/screenshot; otherwise they may observe a half-drawn frame.
    pub settle: Option<SettleArgs>,
    /// Compare against a saved baseline and return change stats as text.
    pub diff: Option<DiffArgs>,
    /// Return a screenshot. Prefer diff for change detection without image tokens.
    pub screenshot: Option<ScreenshotArgs>,
}

/// Arguments for `glass_do`: an ordered, non-empty action sequence + optional observe.
#[derive(Debug, JsonSchema)]
pub struct DoArgs {
    /// Non-empty ordered actions; failure stops remaining steps. Completed mutations may already have landed.
    pub actions: Vec<Action>,
    /// Terminal observation in order: settle, diff, screenshot.
    pub then: Option<ThenArgs>,
    /// Overall budget in ms (default 30000, range 1..120000), shared by all actions and terminal observations.
    pub timeout_ms: Option<u64>,
    #[schemars(skip)]
    pub(crate) encoded_argument_bytes: usize,
}

#[derive(Debug, Deserialize)]
struct DoArgsWire {
    actions: Vec<Action>,
    then: Option<ThenArgs>,
    timeout_ms: Option<u64>,
}

impl<'de> Deserialize<'de> for DoArgs {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        let raw = serde_json::Value::deserialize(deserializer)?;
        let encoded_argument_bytes = serde_json::to_vec(&raw).map_err(D::Error::custom)?.len();
        let wire = DoArgsWire::deserialize(raw).map_err(D::Error::custom)?;
        Ok(Self {
            actions: wire.actions,
            then: wire.then,
            timeout_ms: wire.timeout_ms,
            encoded_argument_bytes,
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn start_args_parse_minimal() {
        let a: StartArgs = serde_json::from_str(r#"{"run":["./app"]}"#).unwrap();
        assert_eq!(a.run, vec!["./app".to_string()]);
        assert!(a.env.is_empty());
        assert!(a.build.is_none());
    }

    #[test]
    fn start_env_deserializes_as_object() {
        let a: StartArgs =
            serde_json::from_str(r#"{"run":["app"],"env":{"K":"V","A":"B"}}"#).unwrap();
        assert_eq!(a.env.get("K").map(String::as_str), Some("V"));
        assert_eq!(a.env.get("A").map(String::as_str), Some("B"));
    }

    #[test]
    fn start_env_defaults_to_empty_when_omitted() {
        let a: StartArgs = serde_json::from_str(r#"{"run":["app"]}"#).unwrap();
        assert!(a.env.is_empty());
    }

    #[test]
    fn start_env_rejects_array_of_pairs() {
        // Locks the breaking change from the old `[["K","V"]]` array-of-pairs shape to the
        // `{"K":"V"}` object: the array shape must no longer deserialize.
        assert!(serde_json::from_str::<StartArgs>(r#"{"run":["app"],"env":[["K","V"]]}"#).is_err());
    }

    #[test]
    fn click_args_parse_with_optionals() {
        let a: ClickArgs =
            serde_json::from_str(r#"{"x":3,"y":4,"button":"right","count":2}"#).unwrap();
        assert_eq!((a.x, a.y), (3, 4));
        assert_eq!(a.button.as_deref(), Some("right"));
        assert_eq!(a.count, Some(2));
    }

    #[test]
    fn click_args_parse_modifiers() {
        let a: ClickArgs =
            serde_json::from_str(r#"{"x":1,"y":2,"modifiers":["ctrl","shift"]}"#).unwrap();
        assert_eq!(
            a.modifiers.as_deref(),
            Some(&["ctrl".to_string(), "shift".to_string()][..])
        );
    }

    #[test]
    fn logs_args_default_to_none() {
        let a: LogsArgs = serde_json::from_str("{}").unwrap();
        assert!(a.cursor.is_none() && a.stream.is_none());
    }

    #[test]
    fn logs_max_lines_is_u32() {
        let a: LogsArgs = serde_json::from_str(r#"{"max_lines": 50}"#).unwrap();
        let n: Option<u32> = a.max_lines; // compile-time: field is Option<u32>
        assert_eq!(n, Some(50));
    }

    #[test]
    fn screenshot_args_default_region_none() {
        let a: ScreenshotArgs = serde_json::from_str("{}").unwrap();
        assert!(a.region.is_none());
    }

    #[test]
    fn screenshot_args_parse_region() {
        let a: ScreenshotArgs =
            serde_json::from_str(r#"{"region":{"x":1,"y":2,"width":3,"height":4}}"#).unwrap();
        let r = a.region.unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (1, 2, 3, 4));
    }

    #[test]
    fn screenshot_args_window_id_defaults_none_and_parses() {
        let none: ScreenshotArgs = serde_json::from_str("{}").unwrap();
        assert!(none.window_id.is_none());
        let some: ScreenshotArgs = serde_json::from_str(r#"{"window_id":42}"#).unwrap();
        assert_eq!(some.window_id, Some(42));
    }

    #[test]
    fn diff_args_region_defaults_none_and_parses() {
        let none: DiffArgs = serde_json::from_str(r#"{"name":"m"}"#).unwrap();
        assert!(none.region.is_none());
        let some: DiffArgs =
            serde_json::from_str(r#"{"name":"m","region":{"x":1,"y":2,"width":3,"height":4}}"#)
                .unwrap();
        let r = some.region.unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (1, 2, 3, 4));
    }

    #[test]
    fn diff_args_ignore_defaults_none_and_parses() {
        let none: DiffArgs = serde_json::from_str(r#"{"name":"m"}"#).unwrap();
        assert!(none.ignore.is_none());
        let some: DiffArgs =
            serde_json::from_str(r#"{"name":"m","ignore":[{"x":1,"y":2,"width":3,"height":4}]}"#)
                .unwrap();
        let regions = some.ignore.unwrap();
        assert_eq!(regions.len(), 1);
        assert_eq!(
            (
                regions[0].x,
                regions[0].y,
                regions[0].width,
                regions[0].height
            ),
            (1, 2, 3, 4)
        );
    }

    #[test]
    fn wait_stable_args_ignore_defaults_none_and_parses() {
        let none: WaitStableArgs = serde_json::from_str("{}").unwrap();
        assert!(none.ignore.is_none());
        let some: WaitStableArgs =
            serde_json::from_str(r#"{"ignore":[{"x":0,"y":0,"width":2,"height":2}]}"#).unwrap();
        assert_eq!(some.ignore.unwrap().len(), 1);
    }

    #[test]
    fn wait_for_region_args_ignore_defaults_none_and_parses() {
        let none: WaitForRegionArgs = serde_json::from_str("{}").unwrap();
        assert!(none.ignore.is_none());
        let some: WaitForRegionArgs =
            serde_json::from_str(r#"{"ignore":[{"x":0,"y":0,"width":2,"height":2}]}"#).unwrap();
        assert_eq!(some.ignore.unwrap().len(), 1);
    }

    #[test]
    fn settle_args_ignore_defaults_none_and_parses() {
        // A `glass_do` settle action/`then.settle` carries the same `ignore`
        // knob as the standalone `glass_wait_stable` tool.
        let none: SettleArgs = serde_json::from_str("{}").unwrap();
        assert!(none.ignore.is_none());
        let some: SettleArgs =
            serde_json::from_str(r#"{"ignore":[{"x":0,"y":0,"width":2,"height":2}]}"#).unwrap();
        assert_eq!(some.ignore.unwrap().len(), 1);
    }

    #[test]
    fn ignore_regions_maps_none_to_empty_vec() {
        assert!(ignore_regions(None).is_empty());
    }

    #[test]
    fn ignore_regions_maps_some_to_core_regions() {
        let rects = vec![
            RegionArgs {
                x: 1,
                y: 2,
                width: 3,
                height: 4,
            },
            RegionArgs {
                x: 5,
                y: 6,
                width: 7,
                height: 8,
            },
        ];
        let regions = ignore_regions(Some(rects.as_slice()));
        assert_eq!(regions.len(), 2);
        assert_eq!(
            (
                regions[0].x,
                regions[0].y,
                regions[0].width,
                regions[0].height
            ),
            (1, 2, 3, 4)
        );
        assert_eq!(
            (
                regions[1].x,
                regions[1].y,
                regions[1].width,
                regions[1].height
            ),
            (5, 6, 7, 8)
        );
    }

    #[test]
    fn wait_stable_args_parse_region() {
        let a: WaitStableArgs =
            serde_json::from_str(r#"{"region":{"x":0,"y":0,"width":5,"height":5}}"#).unwrap();
        assert!(a.region.is_some());
    }

    #[test]
    fn wait_stable_args_parse_stability_region() {
        let a: WaitStableArgs =
            serde_json::from_str(r#"{"stability_region":{"x":0,"y":0,"width":2,"height":2}}"#)
                .unwrap();
        let r = a.stability_region.unwrap();
        assert_eq!((r.x, r.y, r.width, r.height), (0, 0, 2, 2));
    }

    #[test]
    fn wait_stable_args_window_id_defaults_none_and_parses() {
        let none: WaitStableArgs = serde_json::from_str("{}").unwrap();
        assert!(none.window_id.is_none());
        let some: WaitStableArgs = serde_json::from_str(r#"{"window_id":7}"#).unwrap();
        assert_eq!(some.window_id, Some(7));
    }

    #[test]
    fn find_elements_args_parse_flat_target_and_nested_scope() {
        let args: FindElementsArgs = serde_json::from_str(
            r#"{
            "query":"save",
            "role":"Button",
            "states":["visible","enabled"],
            "within":{"role":"Document","states":["visible"]},
            "max_results":10,
            "max_nodes":500,
            "timeout_ms":3000
        }"#,
        )
        .unwrap();
        assert_eq!(args.query.as_deref(), Some("save"));
        assert_eq!(
            args.within.as_ref().and_then(|scope| scope.role.as_deref()),
            Some("Document")
        );
        assert_eq!(args.max_results, Some(10));
    }

    #[test]
    fn find_elements_args_allow_filter_only_queries() {
        let args: FindElementsArgs = serde_json::from_str(r#"{"role":"Button"}"#).unwrap();
        assert!(args.query.is_none());
        assert_eq!(args.role.as_deref(), Some("Button"));
    }

    #[test]
    fn region_args_map_to_core_region() {
        let a = RegionArgs {
            x: 1,
            y: 2,
            width: 3,
            height: 4,
        };
        let r: glass_core::Region = (&a).into();
        assert_eq!((r.x, r.y, r.width, r.height), (1, 2, 3, 4));
    }

    #[test]
    fn click_element_args_parse() {
        let a: ClickElementArgs = serde_json::from_str(r#"{"id":5}"#).unwrap();
        assert_eq!(a.id, Some(5));
    }

    #[test]
    fn set_value_args_parse() {
        let a: SetValueArgs = serde_json::from_str(r#"{"id":5,"text":"hi"}"#).unwrap();
        assert_eq!(a.id, Some(5));
        assert_eq!(a.text, "hi");
    }

    #[test]
    fn click_element_args_parse_return() {
        let a: ClickElementArgs = serde_json::from_str(r#"{"id":5,"return":"snapshot"}"#).unwrap();
        assert_eq!(a.id, Some(5));
        assert_eq!(a.return_.as_deref(), Some("snapshot"));
        let b: ClickElementArgs = serde_json::from_str(r#"{"id":1}"#).unwrap();
        assert!(b.return_.is_none());
    }

    #[test]
    fn set_value_args_parse_return() {
        let a: SetValueArgs =
            serde_json::from_str(r#"{"id":2,"text":"hi","return":"settle"}"#).unwrap();
        assert_eq!(a.id, Some(2));
        assert_eq!(a.return_.as_deref(), Some("settle"));
    }

    #[test]
    fn click_element_args_parse_id_with_typed_mode() {
        let a: ClickElementArgs = serde_json::from_str(r#"{"id":42,"mode":"pointer"}"#).unwrap();
        assert_eq!(a.id, Some(42));
        assert!(matches!(a.mode, Some(ActionModeArg::Pointer)));
        assert!(a.target.is_none());
    }

    #[test]
    fn click_element_args_parse_semantic_target_controls() {
        let a: ClickElementArgs = serde_json::from_str(
            r#"{
              "target":{"query":"Save account","role":"Button","states":["enabled"],
                        "within":{"query":"Account","role":"Document"}},
              "timeout_ms":10000,"max_nodes":0,"mode":"auto","return":"snapshot"
            }"#,
        )
        .unwrap();
        let target = a.target.as_ref().unwrap();
        assert_eq!(target.query.as_deref(), Some("Save account"));
        assert_eq!(target.role.as_deref(), Some("Button"));
        assert_eq!(
            target.states.as_deref(),
            Some(["enabled".to_string()].as_slice())
        );
        assert_eq!(
            target
                .within
                .as_ref()
                .and_then(|scope| scope.query.as_deref()),
            Some("Account")
        );
        assert_eq!(a.timeout_ms, Some(10_000));
        assert_eq!(a.max_nodes, Some(0));
        assert!(matches!(a.mode, Some(ActionModeArg::Auto)));
        assert_eq!(a.return_.as_deref(), Some("snapshot"));
    }

    #[test]
    fn set_value_args_parse_semantic_target() {
        let a: SetValueArgs = serde_json::from_str(
            r#"{"target":{"query":"Account name","role":"TextField"},"text":"Ada"}"#,
        )
        .unwrap();
        assert!(a.id.is_none());
        assert_eq!(
            a.target.as_ref().and_then(|t| t.query.as_deref()),
            Some("Account name")
        );
        assert_eq!(a.text, "Ada");
    }

    #[test]
    fn type_args_parse_semantic_target_controls() {
        let a: TypeArgs = serde_json::from_str(
            r#"{
              "target":{"query":"Account name","role":"TextField"},
              "text":"Ada","focus_mode":"native","timeout_ms":5000
            }"#,
        )
        .unwrap();
        assert_eq!(
            a.target.as_ref().and_then(|t| t.role.as_deref()),
            Some("TextField")
        );
        assert!(matches!(a.focus_mode, Some(ActionModeArg::Native)));
        assert_eq!(a.timeout_ms, Some(5_000));
    }

    #[test]
    fn type_args_text_schema_distinguishes_targeted_and_untargeted_focus() {
        let schema = serde_json::to_value(schemars::schema_for!(TypeArgs)).unwrap();
        let description = schema["properties"]["text"]["description"]
            .as_str()
            .expect("text has a public schema description");
        assert!(
            description.contains("When `target` is omitted"),
            "{description}"
        );
        assert!(
            description.contains("current keyboard focus"),
            "{description}"
        );
        assert!(
            description.contains("When `target` is supplied"),
            "{description}"
        );
        assert!(
            description.contains("resolves and focuses"),
            "{description}"
        );
        assert!(
            !description.contains("does not focus a field"),
            "{description}"
        );
    }

    fn json_contains_number(value: &serde_json::Value, key: &str, expected: u64) -> bool {
        match value {
            serde_json::Value::Object(object) => {
                object.get(key).and_then(serde_json::Value::as_u64) == Some(expected)
                    || object
                        .values()
                        .any(|child| json_contains_number(child, key, expected))
            }
            serde_json::Value::Array(array) => array
                .iter()
                .any(|child| json_contains_number(child, key, expected)),
            _ => false,
        }
    }

    #[test]
    fn click_element_args_schema_adds_optional_target_mode_and_bounded_timeout() {
        let click = serde_json::to_value(schemars::schema_for!(ClickElementArgs)).unwrap();
        let click_properties = click["properties"].as_object().unwrap();
        for field in ["id", "target", "mode", "timeout_ms", "max_nodes", "return"] {
            assert!(
                click_properties.contains_key(field),
                "missing click {field}"
            );
        }
        assert!(
            !click["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "id"))
        );

        let set_value = serde_json::to_value(schemars::schema_for!(SetValueArgs)).unwrap();
        let set_properties = set_value["properties"].as_object().unwrap();
        for field in ["id", "target", "text", "timeout_ms", "max_nodes", "return"] {
            assert!(
                set_properties.contains_key(field),
                "missing set_value {field}"
            );
        }
        assert!(
            !set_value["required"]
                .as_array()
                .is_some_and(|required| required.iter().any(|name| name == "id"))
        );

        let type_args = serde_json::to_value(schemars::schema_for!(TypeArgs)).unwrap();
        let type_properties = type_args["properties"].as_object().unwrap();
        for field in [
            "text",
            "target",
            "focus_mode",
            "timeout_ms",
            "max_nodes",
            "return",
        ] {
            assert!(type_properties.contains_key(field), "missing type {field}");
        }

        for timeout in [
            &click_properties["timeout_ms"],
            &set_properties["timeout_ms"],
            &type_properties["timeout_ms"],
        ] {
            assert!(json_contains_number(timeout, "maximum", 120_000));
        }

        let mode_schema = serde_json::to_value(schemars::schema_for!(ActionModeArg)).unwrap();
        assert_eq!(
            mode_schema["enum"],
            serde_json::json!(["auto", "native", "pointer"])
        );
    }

    #[test]
    fn action_schema_keeps_semantic_structs_embedded_in_existing_variants() {
        let schema = serde_json::to_value(schemars::schema_for!(Action)).unwrap();
        let encoded = schema.to_string();
        for definition in ["ClickElementArgs", "SetValueArgs", "TypeArgs"] {
            assert!(
                encoded.contains(definition),
                "missing {definition}: {encoded}"
            );
        }
        assert!(
            encoded.contains("target"),
            "missing semantic target fields: {encoded}"
        );
        assert!(
            encoded.contains("focus_mode"),
            "missing targeted type mode: {encoded}"
        );
    }

    #[test]
    fn type_args_parse_return() {
        let a: TypeArgs = serde_json::from_str(r#"{"text":"hi","return":"settle"}"#).unwrap();
        assert_eq!(a.text, "hi");
        assert_eq!(a.return_.as_deref(), Some("settle"));
        let b: TypeArgs = serde_json::from_str(r#"{"text":"hi"}"#).unwrap();
        assert!(b.return_.is_none());
    }

    #[test]
    fn wait_for_element_args_parse() {
        let a: WaitForElementArgs =
            serde_json::from_str(r#"{"role":"Button","condition":"enabled"}"#).unwrap();
        assert_eq!(a.role.as_deref(), Some("Button"));
        assert_eq!(a.condition.as_deref(), Some("enabled"));
        assert!(a.name.is_none());
    }

    #[test]
    fn wait_for_region_args_parse() {
        let a: WaitForRegionArgs =
            serde_json::from_str(r#"{"until":"matches","baseline":"login","mode":"exact"}"#)
                .unwrap();
        assert_eq!(a.until.as_deref(), Some("matches"));
        assert_eq!(a.baseline.as_deref(), Some("login"));
        assert_eq!(a.mode.as_deref(), Some("exact"));
        assert!(a.region.is_none());
    }

    #[test]
    fn wait_for_region_args_window_id_defaults_none_and_parses() {
        let none: WaitForRegionArgs = serde_json::from_str(r#"{}"#).unwrap();
        assert!(none.window_id.is_none());
        let some: WaitForRegionArgs = serde_json::from_str(r#"{"window_id":13}"#).unwrap();
        assert_eq!(some.window_id, Some(13));
    }

    #[test]
    fn wait_for_log_args_parse() {
        let a: WaitForLogArgs =
            serde_json::from_str(r#"{"contains":"ready","stream":"stderr"}"#).unwrap();
        assert_eq!(a.contains, "ready");
        assert_eq!(a.stream.as_deref(), Some("stderr"));
        assert!(a.cursor.is_none());
    }

    #[test]
    fn do_args_parse_mixed_actions() {
        let a: DoArgs = serde_json::from_str(
            r#"{"actions":[
                {"action":"click","x":10,"y":20},
                {"action":"type","text":"hi"},
                {"action":"key","chord":"Return"},
                {"action":"settle","timeout_ms":500}
            ]}"#,
        )
        .unwrap();
        assert_eq!(a.actions.len(), 4);
        assert!(matches!(a.actions[0], Action::Click(_)));
        assert!(matches!(a.actions[1], Action::Type(_)));
        assert!(matches!(a.actions[2], Action::Key(_)));
        assert!(matches!(a.actions[3], Action::Settle(_)));
        assert!(a.then.is_none());
        assert!(a.timeout_ms.is_none());
    }

    #[test]
    fn do_args_accepts_every_documented_type_return_mode() {
        for return_mode in ["none", "settle", "snapshot"] {
            let raw = serde_json::json!({
                "actions": [{
                    "action": "type",
                    "text": "hé🙂",
                    "return": return_mode
                }]
            });
            let args: DoArgs = serde_json::from_value(raw).unwrap();
            let Action::Type(type_args) = &args.actions[0] else {
                panic!("the action must remain a type action");
            };
            assert_eq!(type_args.return_.as_deref(), Some(return_mode));
        }
    }

    #[test]
    fn do_args_rejects_unknown_action() {
        let r: Result<DoArgs, _> =
            serde_json::from_str(r#"{"actions":[{"action":"teleport","x":1}]}"#);
        assert!(r.is_err());
    }

    #[test]
    fn do_args_parse_then() {
        let a: DoArgs = serde_json::from_str(
            r#"{"actions":[{"action":"key","chord":"a"}],"then":{"screenshot":{}}}"#,
        )
        .unwrap();
        assert_eq!(a.actions.len(), 1);
        assert!(a.then.is_some());
        assert!(a.then.unwrap().screenshot.is_some());
        assert!(a.timeout_ms.is_none());
    }

    #[test]
    fn do_args_parse_semantic_actions_and_timeout() {
        let a: DoArgs = serde_json::from_str(
            r#"{
            "actions":[
              {"action":"click_element","id":3,"return":"snapshot"},
              {"action":"set_value","id":4,"text":"Alice","return":"settle"},
              {"action":"wait_for_element","description":"Name","value":"Alice"},
              {"action":"scroll_to_element","name":"Row 250","direction":"down"}
            ],
            "timeout_ms":10000
        }"#,
        )
        .unwrap();
        assert!(matches!(a.actions[0], Action::ClickElement(_)));
        assert!(matches!(a.actions[1], Action::SetValue(_)));
        assert!(matches!(a.actions[2], Action::WaitForElement(_)));
        assert!(matches!(a.actions[3], Action::ScrollToElement(_)));
        assert_eq!(a.timeout_ms, Some(10_000));
    }

    #[test]
    fn do_args_measure_compact_json_before_unknown_fields_are_ignored() {
        let raw = r#"{"actions":[{"action":"key","chord":"a","ignored":"xxxxxxxx"}]}"#;
        let value: serde_json::Value = serde_json::from_str(raw).unwrap();
        let a: DoArgs = serde_json::from_str(raw).unwrap();
        assert_eq!(
            a.encoded_argument_bytes,
            serde_json::to_vec(&value).unwrap().len()
        );
    }

    fn mixed_utf8_do_args_with_compact_len(target: usize) -> Vec<u8> {
        let mut value = serde_json::json!({
            "actions": [{
                "action": "key",
                "chord": "é",
                "ignored_action_bytes": "🙂"
            }],
            "ignored_top_level_bytes": "漢"
        });
        let base = serde_json::to_vec(&value).unwrap().len();
        assert!(base <= target);
        value["ignored_top_level_bytes"] =
            serde_json::Value::String(format!("漢{}", "x".repeat(target - base)));
        let compact = serde_json::to_vec(&value).unwrap();
        assert_eq!(compact.len(), target);
        compact
    }

    #[test]
    fn do_args_measure_exact_mixed_utf8_boundaries_before_ignoring_unknown_fields() {
        for target in [65_536, 65_537] {
            let compact = mixed_utf8_do_args_with_compact_len(target);
            let args: DoArgs = serde_json::from_slice(&compact).unwrap();
            assert_eq!(args.encoded_argument_bytes, target);
        }
    }

    #[test]
    fn do_args_schema_hides_internal_measurement() {
        let schema = serde_json::to_value(schemars::schema_for!(DoArgs)).unwrap();
        let text = schema.to_string();
        assert!(text.contains("timeout_ms"));
        assert!(!text.contains("encoded_argument_bytes"));
    }
}
