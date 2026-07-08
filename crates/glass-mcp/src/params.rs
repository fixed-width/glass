//! Tool-argument structs. Each derives `Deserialize` (parse JSON args) and
//! `JsonSchema` (so MCP advertises a schema to the agent).

use glass_core::Region;
use schemars::JsonSchema;
use serde::Deserialize;

#[derive(Debug, Clone, Deserialize, JsonSchema)]
pub struct RegionArgs {
    pub x: u32,
    pub y: u32,
    pub width: u32,
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

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScreenshotArgs {
    /// Optional window-relative sub-rectangle to capture; omit for the whole window.
    pub region: Option<RegionArgs>,
    /// Capture/observe this window (id from `glass_list_windows`) instead of the
    /// active one, without changing which window subsequent ops target. Omit for
    /// the active window.
    pub window_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WindowHintArgs {
    /// Case-insensitive substring matched against window titles. Used to pick the
    /// right window when several appear, and — since it ignores the process tree —
    /// to locate a window the launched process hands off to an unrelated process
    /// (e.g. some packaged Windows apps).
    pub title: Option<String>,
    /// Exact window-class match. Same purpose as `title` but more stable, since
    /// class names rarely carry the dynamic prefixes/suffixes that titles do.
    pub class: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct StartArgs {
    /// Optional shell command to run (in `cwd`) before launching.
    pub build: Option<String>,
    /// Program and arguments to launch; `run[0]` is the executable.
    pub run: Vec<String>,
    /// Backend to launch under: `"x11"` or `"wayland"` (Linux), `"windows"` (on a
    /// Windows host), `"macos"` (on a macOS host), or `"android"` (an AVD emulator, any
    /// host). Omit for the server default (`GLASS_BACKEND`, else `windows` on Windows,
    /// `macos` on macOS, else x11).
    pub backend: Option<String>,
    /// Containment level for the launched app: `"default"` (filesystem/process
    /// containment, network on), `"strict"` (also no network), or `"off"` (no
    /// containment). Omit for the server default (`GLASS_SANDBOX`, else `default`).
    pub sandbox: Option<String>,
    pub cwd: Option<String>,
    #[serde(default)]
    pub env: Vec<(String, String)>,
    /// Optional `{ title?, class? }` to disambiguate which window is the app's when
    /// more than one appears, or to find a window the launched process hands off to
    /// an unrelated process (some packaged Windows apps). Omit to take the first
    /// window owned by the launched process or a descendant it can follow.
    pub window_hint: Option<WindowHintArgs>,
    pub timeout_ms: Option<u64>,
    /// Spawn a private accessibility (AT-SPI) bus so `glass_a11y_snapshot` / `marks` /
    /// `set_value` / `click_element` / `wait_for_element` work against this app. Opt-in
    /// (default false) — only set when you need the accessibility tree; it spawns extra
    /// processes. Linux only.
    #[serde(default)]
    pub a11y: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WindowArgs {
    /// One of: "focus", "resize", "move", "geometry".
    pub op: String,
    pub x: Option<i32>,
    pub y: Option<i32>,
    pub width: Option<u32>,
    pub height: Option<u32>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SelectWindowArgs {
    /// The window id from `glass_list_windows`. Ids are not stable across calls —
    /// re-list rather than caching them.
    pub id: u64,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickElementArgs {
    /// The element `#id` from `glass_a11y_snapshot`. Valid only within the latest
    /// snapshot — re-snapshot if the UI changed. If the element actually renders in a
    /// popover owned by a different window than the active one (e.g. an open
    /// dropdown's option row), the click is automatically routed into that popover
    /// window and the previously-active window is restored afterward — no extra step
    /// needed.
    pub id: u32,
    /// Optional observe folded into the result: "snapshot" (a fresh a11y tree, also
    /// refreshing the snapshot cache), "settle" (wait for the UI to stop changing,
    /// text-only), or "none" (default).
    #[serde(rename = "return")]
    pub return_: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct SetValueArgs {
    /// The element `#id` from `glass_a11y_snapshot`.
    pub id: u32,
    /// The value to set. For a text field, the text. For a spin/slider, a number.
    /// For a switch/checkbox/toggle, a boolean (`"true"`/`"false"`/`"on"`/`"off"`/
    /// `"1"`/`"0"`) — idempotent. For a dropdown/combo box, an option label
    /// (case-insensitive); glass opens it and picks that option.
    pub text: String,
    /// Optional observe folded into the result: "snapshot" (a fresh a11y tree, also
    /// refreshing the snapshot cache), "settle" (wait for the UI to stop changing,
    /// text-only), or "none" (default).
    #[serde(rename = "return")]
    pub return_: Option<String>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClickArgs {
    pub x: i32,
    pub y: i32,
    /// "left" (default), "right", or "middle".
    pub button: Option<String>,
    pub count: Option<u32>,
    /// Modifier keys to hold during the action, e.g. ["ctrl"] or ["ctrl","shift"] for multi/range-select.
    pub modifiers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct MoveArgs {
    pub x: i32,
    pub y: i32,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DragArgs {
    pub x1: i32,
    pub y1: i32,
    pub x2: i32,
    pub y2: i32,
    pub button: Option<String>,
    /// Modifier keys to hold during the action, e.g. ["ctrl"] or ["ctrl","shift"] for multi/range-select.
    pub modifiers: Option<Vec<String>>,
    /// Span the drag's motion over this many milliseconds so a frame-based GUI
    /// (egui/winit) samples the path across multiple frames (and registers the
    /// drag even while it repaints). Default 200. Lower = faster but coarser.
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
    pub x: i32,
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
    pub x: i32,
    pub y: i32,
    /// Horizontal scroll in **wheel notches** (discrete clicks — small integers like 1–5, NOT
    /// pixels). Positive `dx` sends wheel-right, negative wheel-left; glass clicks `|dx|` times.
    pub dx: Option<i32>,
    /// Vertical scroll in **wheel notches** (discrete clicks — small integers like 1–5, NOT
    /// pixels). Positive `dy` sends wheel-down, negative wheel-up; glass clicks `|dy|` times. How
    /// an app maps a wheel notch to its view (lines, pixels, zoom) is the app's choice.
    pub dy: Option<i32>,
    /// Modifier keys to hold during the action, e.g. ["ctrl"] or ["ctrl","shift"] for multi/range-select.
    pub modifiers: Option<Vec<String>>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct TypeArgs {
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct KeyArgs {
    /// A chord like "ctrl+s", "Return", "alt+F4".
    pub chord: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ClipboardSetArgs {
    /// The text to write to the clipboard.
    pub text: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitStableArgs {
    pub interval_ms: Option<u64>,
    pub settle_frames: Option<u32>,
    pub tolerance: Option<u8>,
    pub timeout_ms: Option<u64>,
    /// Optional window-relative sub-rectangle for the returned frame.
    pub region: Option<RegionArgs>,
    /// Optional window-relative sub-rectangle to watch for settling; when set,
    /// the settle decision ignores changes outside it. Independent of `region`.
    pub stability_region: Option<RegionArgs>,
    /// Return the settled frame as an image (default true). Set false for a
    /// text-only `{settled,width,height}` result with no WebP — cheap when the
    /// next step is a text `glass_diff`. `region` is ignored when false.
    pub include_image: Option<bool>,
    /// Capture/observe this window (id from `glass_list_windows`) instead of the
    /// active one, without changing which window subsequent ops target. Omit for
    /// the active window.
    pub window_id: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct WaitForElementArgs {
    /// Substring of the element's accessible name (selector).
    pub name: Option<String>,
    /// Element role filter, e.g. "Button", "ProgressBar" (selector).
    pub role: Option<String>,
    /// What to wait for (default "appears"): appears|disappears|enabled|disabled|
    /// checked|unchecked|selected|unselected|expanded|collapsed|focused|visible|hidden.
    pub condition: Option<String>,
    /// Additionally require the matched element's `value` to contain this substring.
    /// Not a standalone selector — `name` and/or `role` is still required.
    pub value_contains: Option<String>,
    /// Poll interval (default 200ms — an a11y snapshot per tick).
    pub interval_ms: Option<u64>,
    /// Give up after this long (default 10000ms); returns `{matched:false}`.
    pub timeout_ms: Option<u64>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct ScrollToElementArgs {
    /// Substring of the target element's accessible name (selector). `name` and/or
    /// `role` is required.
    pub name: Option<String>,
    /// Element role filter, e.g. "ListItem", "Button" (selector).
    pub role: Option<String>,
    /// Additionally require the matched element's `value` to contain this substring.
    /// Not a standalone selector — `name` and/or `role` is still required.
    pub value_contains: Option<String>,
    /// Primary sweep direction: "down" (default) or "up". The search reverses to the
    /// other end if the target isn't found first.
    pub direction: Option<String>,
    /// Scroll anchor x (window-relative). Defaults with `y` to the window center;
    /// set both to point the wheel at a specific scrollable container.
    pub x: Option<i32>,
    /// Scroll anchor y (window-relative). See `x`.
    pub y: Option<i32>,
    /// Wheel notches per scroll step (default 3). A calibration escape hatch — larger
    /// covers distance faster but risks stepping past a row's realized band.
    pub step: Option<u32>,
    /// Give up after this long (default 20000ms); returns `{matched:false}`.
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
    /// Capture/observe this window (id from `glass_list_windows`) instead of the
    /// active one, without changing which window subsequent ops target. Omit for
    /// the active window.
    pub window_id: Option<u64>,
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
    pub name: String,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DoctorArgs {
    /// Also spawn and tear down the default backend's headless display to verify it
    /// actually starts (slower). Default false.
    pub deep: Option<bool>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct DiffArgs {
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
    /// Optional window-relative sub-rectangle to diff; omit to diff the whole
    /// window. Scopes the comparison (and the reported `bbox`, which becomes
    /// region-relative) to just this area — the way to ask "did *only* this part
    /// change?" Mirrors `glass_wait_for_region`'s `region`.
    pub region: Option<RegionArgs>,
}

#[derive(Debug, Deserialize, JsonSchema)]
pub struct LogsArgs {
    pub cursor: Option<u64>,
    pub max_lines: Option<usize>,
    /// "stdout", "stderr", or "both" (default).
    pub stream: Option<String>,
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
}

/// A mid-sequence or terminal settle — the `wait_stable` knobs, no image/return.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct SettleArgs {
    pub interval_ms: Option<u64>,
    pub settle_frames: Option<u32>,
    pub tolerance: Option<u8>,
    pub timeout_ms: Option<u64>,
    pub stability_region: Option<RegionArgs>,
}

/// Optional terminal observe after a `glass_do` sequence (run settle → diff →
/// screenshot). All text-first; only `screenshot` (or `diff` with its own
/// `include_image`) returns an image.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct ThenArgs {
    pub settle: Option<SettleArgs>,
    pub diff: Option<DiffArgs>,
    pub screenshot: Option<ScreenshotArgs>,
}

/// Arguments for `glass_do`: an ordered, non-empty action sequence + optional observe.
#[derive(Debug, Deserialize, JsonSchema)]
pub struct DoArgs {
    pub actions: Vec<Action>,
    pub then: Option<ThenArgs>,
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
        assert_eq!(a.id, 5);
    }

    #[test]
    fn set_value_args_parse() {
        let a: SetValueArgs = serde_json::from_str(r#"{"id":5,"text":"hi"}"#).unwrap();
        assert_eq!(a.id, 5);
        assert_eq!(a.text, "hi");
    }

    #[test]
    fn click_element_args_parse_return() {
        let a: ClickElementArgs = serde_json::from_str(r#"{"id":5,"return":"snapshot"}"#).unwrap();
        assert_eq!(a.id, 5);
        assert_eq!(a.return_.as_deref(), Some("snapshot"));
        let b: ClickElementArgs = serde_json::from_str(r#"{"id":1}"#).unwrap();
        assert!(b.return_.is_none());
    }

    #[test]
    fn set_value_args_parse_return() {
        let a: SetValueArgs =
            serde_json::from_str(r#"{"id":2,"text":"hi","return":"settle"}"#).unwrap();
        assert_eq!(a.id, 2);
        assert_eq!(a.return_.as_deref(), Some("settle"));
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
    }
}
