//! Pure builders that turn glass input events into `adb shell input …` command
//! argument vectors, plus the `Injector` seam (`ShellInjector` shells out; a
//! future on-device agent can replace it for lower latency).

use std::sync::Arc;

use crate::adb::Adb;
use crate::agent::{AgentClient, Pt};
use glass_core::keys::parse_chord;
use glass_core::{GlassError, KeyEvent, Modifier, PointerEvent, Result, WindowGeometry};

/// Pixels of swipe travel per scroll "click" (`Scroll.dx/dy` are wheel clicks —
/// X11 clicks the wheel `|delta|` times). Tunable.
const SCROLL_STEP_PX: i32 = 120;
/// Swipe duration (ms) for scrolls, and for drags that don't specify one.
const SWIPE_MS: u64 = 300;

/// Build the `adb shell input …` command(s) for a pointer event, mapping
/// window-relative coords to absolute device coords via `origin`. One argv per
/// command; an empty vec means "nothing to inject" (a touch `Move` has no hover
/// equivalent). Mouse button and keyboard modifiers are ignored — a touch
/// contact is single-button and can't carry modifiers.
pub fn pointer_commands(origin: &WindowGeometry, event: &PointerEvent) -> Vec<Vec<String>> {
    let abs = |x: i32, y: i32| (origin.x + x, origin.y + y);
    match *event {
        PointerEvent::Move { .. } => vec![],
        PointerEvent::Click { x, y, count, .. } => {
            let (ax, ay) = abs(x, y);
            (0..count.max(1)).map(|_| tap(ax, ay)).collect()
        }
        PointerEvent::Drag { from_x, from_y, to_x, to_y, duration_ms, .. } => {
            let (fx, fy) = abs(from_x, from_y);
            let (tx, ty) = abs(to_x, to_y);
            let ms = if duration_ms == 0 { SWIPE_MS } else { duration_ms };
            vec![swipe(fx, fy, tx, ty, ms)]
        }
        PointerEvent::Scroll { x, y, dx, dy, .. } => {
            let (cx, cy) = abs(x, y);
            // Touch scroll = swipe opposite the wheel direction. The swipe is
            // anchored at the event point and clamped to the window, so an anchor
            // within SCROLL_STEP_PX of the relevant edge yields a short/degenerate
            // swipe — scroll from a mid-content point. A later on-device agent will
            // fling properly.
            let hi_x = (origin.x + origin.width as i32 - 1).max(origin.x);
            let hi_y = (origin.y + origin.height as i32 - 1).max(origin.y);
            let ex = cx.saturating_sub(dx.saturating_mul(SCROLL_STEP_PX)).clamp(origin.x, hi_x);
            let ey = cy.saturating_sub(dy.saturating_mul(SCROLL_STEP_PX)).clamp(origin.y, hi_y);
            vec![swipe(cx, cy, ex, ey, SWIPE_MS)]
        }
    }
}

fn tap(x: i32, y: i32) -> Vec<String> {
    vec!["shell".into(), "input".into(), "tap".into(), x.to_string(), y.to_string()]
}

fn swipe(x1: i32, y1: i32, x2: i32, y2: i32, ms: u64) -> Vec<String> {
    vec![
        "shell".into(), "input".into(), "swipe".into(),
        x1.to_string(), y1.to_string(), x2.to_string(), y2.to_string(), ms.to_string(),
    ]
}

/// Build the `adb shell input …` command(s) for a key event.
pub fn key_commands(event: &KeyEvent) -> Result<Vec<Vec<String>>> {
    match event {
        KeyEvent::Text(s) if s.is_empty() => Ok(vec![]),
        KeyEvent::Text(s) => Ok(vec![text_command(s)]),
        KeyEvent::Chord(c) => Ok(vec![chord_command(c)?]),
    }
}

/// `input text` of `s`, made safe for the device shell: spaces become `%s`
/// (input's space escape) and the whole argument is single-quoted so shell
/// metacharacters are taken literally. We build the remote command string
/// ourselves to avoid `adb`'s argument re-splitting.
///
/// Known limit: Android's `input text` turns every `%s` into a space, so a
/// literal `%s` already in `s` round-trips as a space; and only ASCII is
/// reliable. A later on-device agent handles literals and Unicode faithfully.
fn text_command(s: &str) -> Vec<String> {
    let spaced = s.replace(' ', "%s");
    let quoted = format!("'{}'", spaced.replace('\'', r"'\''"));
    vec!["shell".into(), format!("input text {quoted}")]
}

/// A no-modifier chord → `input keyevent`; a modifier chord → `input
/// keycombination` (Android 12+/API 31+), which presses the keys together.
fn chord_command(chord: &str) -> Result<Vec<String>> {
    let (mods, keysym) = parse_chord(chord)?;
    let key = android_keycode(keysym)
        .ok_or_else(|| GlassError::InvalidKey(format!("no Android keycode for the key in '{chord}'")))?;
    if mods.is_empty() {
        Ok(vec!["shell".into(), "input".into(), "keyevent".into(), key.to_string()])
    } else {
        let mut argv = vec!["shell".into(), "input".into(), "keycombination".into()];
        argv.extend(mods.iter().map(|m| meta_keycode(*m).to_string()));
        argv.push(key.to_string());
        Ok(argv)
    }
}

/// X keysym (from `parse_chord`) → Android `KEYCODE_*` numeric value.
fn android_keycode(keysym: u32) -> Option<u32> {
    if let Some(c) = char::from_u32(keysym) {
        if c.is_ascii_alphabetic() {
            return Some(29 + (c.to_ascii_lowercase() as u32 - 'a' as u32)); // KEYCODE_A = 29
        }
        if c.is_ascii_digit() {
            return Some(7 + (c as u32 - '0' as u32)); // KEYCODE_0 = 7
        }
    }
    let kc = match keysym {
        0xff0d => 66,  // Return    → ENTER
        0xff1b => 111, // Escape    → ESCAPE
        0xff09 => 61,  // Tab       → TAB
        0xff08 => 67,  // Backspace → DEL
        0xffff => 112, // Delete    → FORWARD_DEL
        0x0020 => 62,  // space     → SPACE
        0xff52 => 19,  // Up        → DPAD_UP
        0xff54 => 20,  // Down      → DPAD_DOWN
        0xff51 => 21,  // Left      → DPAD_LEFT
        0xff53 => 22,  // Right     → DPAD_RIGHT
        0xff50 => 122, // Home      → MOVE_HOME
        0xff57 => 123, // End       → MOVE_END
        0xffbe..=0xffc9 => 131 + (keysym - 0xffbe), // F1..F12 → KEYCODE_F1(131)..F12(142)
        _ => return None,
    };
    Some(kc)
}

/// Modifier → Android meta `KEYCODE_*_LEFT`.
fn meta_keycode(m: Modifier) -> u32 {
    match m {
        Modifier::Control => 113, // CTRL_LEFT
        Modifier::Shift => 59,    // SHIFT_LEFT
        Modifier::Alt => 57,      // ALT_LEFT
        Modifier::Super => 117,   // META_LEFT
    }
}

/// Pixels between interpolated samples along an agent drag path. Small enough that the first
/// samples clear Android touch-slop near the start, so a drag is recognized at its true origin
/// rather than its end (a 2-point path injects DOWN+UP only, which touch-slop swallows).
const AGENT_STEP_PX: i32 = 16;

/// Interpolate a drag path `from`→`to` over `ms` into evenly spaced samples (≥8) so the
/// on-device agent injects real `ACTION_MOVE` events. First sample `t=0`, last `t=ms`.
fn agent_path(fx: i32, fy: i32, tx: i32, ty: i32, ms: u64) -> Vec<Pt> {
    let dist = (tx - fx).abs().max((ty - fy).abs());
    let steps = (dist / AGENT_STEP_PX).clamp(8, 64) as u64;
    (0..=steps)
        .map(|i| {
            let f = i as f64 / steps as f64;
            Pt {
                x: fx + ((tx - fx) as f64 * f).round() as i32,
                y: fy + ((ty - fy) as f64 * f).round() as i32,
                t_ms: (ms as f64 * f).round() as u64,
            }
        })
        .collect()
}

/// Map a pointer event to the agent's gesture(s): a list of absolute-display pointer paths
/// (one path per tap/swipe), mirroring `pointer_commands`' window→display mapping. `Click`
/// with count N yields N single-point tap paths; `Drag` and `Scroll` each yield one
/// interpolated multi-point path (so the swipe has real velocity); `Move` yields nothing.
pub(crate) fn agent_pointer(origin: &WindowGeometry, event: &PointerEvent) -> Vec<Vec<Pt>> {
    let abs = |x: i32, y: i32| (origin.x + x, origin.y + y);
    match *event {
        PointerEvent::Move { .. } => vec![],
        PointerEvent::Click { x, y, count, .. } => {
            let (ax, ay) = abs(x, y);
            (0..count.max(1)).map(|_| vec![Pt { x: ax, y: ay, t_ms: 0 }]).collect()
        }
        PointerEvent::Drag { from_x, from_y, to_x, to_y, duration_ms, .. } => {
            let (fx, fy) = abs(from_x, from_y);
            let (tx, ty) = abs(to_x, to_y);
            let ms = if duration_ms == 0 { SWIPE_MS } else { duration_ms };
            vec![agent_path(fx, fy, tx, ty, ms)]
        }
        PointerEvent::Scroll { x, y, dx, dy, .. } => {
            let (cx, cy) = abs(x, y);
            let hi_x = (origin.x + origin.width as i32 - 1).max(origin.x);
            let hi_y = (origin.y + origin.height as i32 - 1).max(origin.y);
            let ex = cx.saturating_sub(dx.saturating_mul(SCROLL_STEP_PX)).clamp(origin.x, hi_x);
            let ey = cy.saturating_sub(dy.saturating_mul(SCROLL_STEP_PX)).clamp(origin.y, hi_y);
            // Interpolate so the swipe carries real velocity (a fling), not a single jump —
            // a 2-point swipe under-scrolls and stalls on long lists (dogfood #17).
            vec![agent_path(cx, cy, ex, ey, SWIPE_MS)]
        }
    }
}

/// Injects via the on-device agent (real MotionEvents + faithful keys/Unicode). The `Adb`
/// argument of the `Injector` methods is unused — the agent is reached over its socket.
pub(crate) struct AgentInjector {
    pub(crate) agent: Arc<AgentClient>,
}

impl Injector for AgentInjector {
    fn pointer(&self, _adb: &Adb, origin: &WindowGeometry, event: &PointerEvent) -> Result<()> {
        // One agent request per gesture: a Click{count:N} sends N sequential taps.
        for gesture in agent_pointer(origin, event) {
            self.agent.pointer(&gesture, "left")?;
        }
        Ok(())
    }
    fn key(&self, _adb: &Adb, event: &KeyEvent) -> Result<()> {
        match event {
            KeyEvent::Text(s) if s.is_empty() => Ok(()),
            KeyEvent::Text(s) => self.agent.text(s),
            KeyEvent::Chord(c) => self.agent.key(c),
        }
    }
}

/// Pointer/key injection seam. `ShellInjector` shells out via `adb input`; a
/// future on-device agent can implement this for lower-latency injection.
pub trait Injector {
    fn pointer(&self, adb: &Adb, origin: &WindowGeometry, event: &PointerEvent) -> Result<()>;
    fn key(&self, adb: &Adb, event: &KeyEvent) -> Result<()>;
}

/// Injects by running `adb shell input …` commands.
pub struct ShellInjector;

impl Injector for ShellInjector {
    fn pointer(&self, adb: &Adb, origin: &WindowGeometry, event: &PointerEvent) -> Result<()> {
        for argv in pointer_commands(origin, event) {
            adb.run(argv.iter().map(String::as_str))?;
        }
        Ok(())
    }

    fn key(&self, adb: &Adb, event: &KeyEvent) -> Result<()> {
        for argv in key_commands(event)? {
            adb.run(argv.iter().map(String::as_str))?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod agent_inject_tests {
    use super::*;
    use crate::agent::Pt;
    use glass_core::{MouseButton, PointerEvent, WindowGeometry};

    fn origin() -> WindowGeometry { WindowGeometry { x: 100, y: 200, width: 500, height: 800 } }

    #[test]
    fn click_maps_to_absolute_taps() {
        let ev = PointerEvent::Click { x: 10, y: 20, button: MouseButton::Left, count: 2, modifiers: vec![] };
        let g = agent_pointer(&origin(), &ev);
        assert_eq!(g.len(), 2);
        assert_eq!(g[0], vec![Pt { x: 110, y: 220, t_ms: 0 }]);
        assert_eq!(g[1], vec![Pt { x: 110, y: 220, t_ms: 0 }]);
    }

    #[test]
    fn drag_interpolates_intermediate_samples() {
        let ev = PointerEvent::Drag { from_x: 0, from_y: 0, to_x: 50, to_y: 60, duration_ms: 250, button: MouseButton::Left, modifiers: vec![] };
        let g = agent_pointer(&origin(), &ev);
        assert_eq!(g.len(), 1);
        let path = &g[0];
        // Endpoints exact: DOWN at abs(from) @ t0, UP at abs(to) @ t=ms.
        assert_eq!(path.first().copied().unwrap(), Pt { x: 100, y: 200, t_ms: 0 });
        assert_eq!(path.last().copied().unwrap(), Pt { x: 150, y: 260, t_ms: 250 });
        // Real ACTION_MOVE samples between DOWN and UP — a 2-point path is swallowed by
        // Android touch-slop (onDragStart fires at the end coordinate). See dogfood F8.
        assert!(path.len() >= 8, "expected interpolated samples, got {}", path.len());
        // Monotonic in time and along the (down-right) path.
        assert!(path.windows(2).all(|w| w[0].t_ms <= w[1].t_ms), "t_ms not monotonic: {path:?}");
        assert!(
            path.windows(2).all(|w| w[0].x <= w[1].x && w[0].y <= w[1].y),
            "not monotonic along path: {path:?}"
        );
    }

    #[test]
    fn move_maps_to_nothing() {
        let ev = PointerEvent::Move { x: 1, y: 2 };
        assert!(agent_pointer(&origin(), &ev).is_empty());
    }

    #[test]
    fn scroll_interpolates_one_swipe() {
        let ev = PointerEvent::Scroll { x: 250, y: 400, dx: 0, dy: 1, modifiers: vec![] };
        let g = agent_pointer(&origin(), &ev);
        assert_eq!(g.len(), 1);
        let path = &g[0];
        assert_eq!(path.first().copied().unwrap(), Pt { x: 350, y: 600, t_ms: 0 });
        assert_eq!(path.last().copied().unwrap(), Pt { x: 350, y: 480, t_ms: SWIPE_MS });
        // Interpolated samples give the swipe real velocity → a fling, so deep scrolls
        // don't stall. Dogfood finding #17.
        assert!(path.len() >= 8, "scroll should fling with interpolated samples, got {}", path.len());
    }

    /// Drift guard: `pointer_commands` and `agent_pointer` must agree on absolute
    /// coordinates. A future edit that shifts one mapping without the other will
    /// fail here.
    #[test]
    fn agent_pointer_agrees_with_pointer_commands_coords() {
        let o = WindowGeometry { x: 100, y: 200, width: 500, height: 800 };
        // Click → tap: same absolute coord in both representations.
        let click = PointerEvent::Click { x: 10, y: 20, button: MouseButton::Left, count: 1, modifiers: vec![] };
        let argv = pointer_commands(&o, &click);
        let path = agent_pointer(&o, &click);
        assert_eq!(argv[0], ["shell", "input", "tap", "110", "220"].map(String::from).to_vec());
        assert_eq!(path[0][0], Pt { x: 110, y: 220, t_ms: 0 });
        // Scroll → swipe: anchor + end coords agree between argv and the Pt path.
        let scroll = PointerEvent::Scroll { x: 250, y: 400, dx: 0, dy: 1, modifiers: vec![] };
        let sargv = pointer_commands(&o, &scroll);
        let spath = agent_pointer(&o, &scroll);
        assert_eq!((sargv[0][3].as_str(), sargv[0][4].as_str()), ("350", "600"));
        assert_eq!(spath[0][0], Pt { x: 350, y: 600, t_ms: 0 });
        assert_eq!((sargv[0][5].as_str(), sargv[0][6].as_str()), ("350", "480"));
        let end = spath[0].last().unwrap();
        assert_eq!((end.x, end.y), (350, 480));
    }
}

#[cfg(test)]
mod pointer_tests {
    use super::*;
    use glass_core::{MouseButton, PointerEvent, WindowGeometry};

    fn win() -> WindowGeometry {
        WindowGeometry { x: 0, y: 63, width: 1080, height: 2400 }
    }

    #[test]
    fn move_injects_nothing() {
        assert!(pointer_commands(&win(), &PointerEvent::Move { x: 5, y: 5 }).is_empty());
    }

    #[test]
    fn click_taps_at_absolute_coords() {
        let ev = PointerEvent::Click {
            x: 10, y: 20, button: MouseButton::Left, count: 1, modifiers: vec![],
        };
        assert_eq!(pointer_commands(&win(), &ev), vec![vec![
            "shell".to_string(), "input".into(), "tap".into(), "10".into(), "83".into(),
        ]]);
    }

    #[test]
    fn multi_count_click_taps_repeatedly() {
        let ev = PointerEvent::Click {
            x: 1, y: 1, button: MouseButton::Left, count: 2, modifiers: vec![],
        };
        assert_eq!(pointer_commands(&win(), &ev).len(), 2);
    }

    #[test]
    fn drag_swipes_with_duration() {
        let ev = PointerEvent::Drag {
            from_x: 0, from_y: 0, to_x: 100, to_y: 200,
            button: MouseButton::Left, modifiers: vec![], duration_ms: 250,
        };
        assert_eq!(pointer_commands(&win(), &ev), vec![vec![
            "shell".to_string(), "input".into(), "swipe".into(),
            "0".into(), "63".into(), "100".into(), "263".into(), "250".into(),
        ]]);
    }

    #[test]
    fn scroll_down_swipes_upward_opposite_the_wheel() {
        let ev = PointerEvent::Scroll { x: 540, y: 1200, dx: 0, dy: 1, modifiers: vec![] };
        let got = pointer_commands(&win(), &ev);
        assert_eq!(got, vec![vec![
            "shell".to_string(), "input".into(), "swipe".into(),
            "540".into(), "1263".into(), "540".into(), "1143".into(), "300".into(),
        ]]);
    }

    #[test]
    fn scroll_clamps_to_the_window() {
        let ev = PointerEvent::Scroll { x: 10, y: 1, dx: 0, dy: 100, modifiers: vec![] };
        let got = pointer_commands(&win(), &ev);
        let end_y = &got[0][6];
        assert_eq!(end_y, "63");
    }
}

#[cfg(test)]
mod key_tests {
    use super::*;
    use glass_core::{GlassError, KeyEvent};

    #[test]
    fn empty_text_injects_nothing() {
        assert!(key_commands(&KeyEvent::Text(String::new())).unwrap().is_empty());
    }

    #[test]
    fn text_is_space_escaped_and_quoted() {
        let got = key_commands(&KeyEvent::Text("hello world".into())).unwrap();
        assert_eq!(got, vec![vec!["shell".to_string(), "input text 'hello%sworld'".into()]]);
    }

    #[test]
    fn text_single_quote_is_shell_escaped() {
        let got = key_commands(&KeyEvent::Text("it's".into())).unwrap();
        assert_eq!(got, vec![vec!["shell".to_string(), r"input text 'it'\''s'".into()]]);
    }

    #[test]
    fn plain_chord_is_a_keyevent() {
        let got = key_commands(&KeyEvent::Chord("Enter".into())).unwrap();
        assert_eq!(got, vec![vec!["shell".to_string(), "input".into(), "keyevent".into(), "66".into()]]);
    }

    #[test]
    fn letter_chord_maps_to_keycode_a() {
        let got = key_commands(&KeyEvent::Chord("a".into())).unwrap();
        assert_eq!(got, vec![vec!["shell".to_string(), "input".into(), "keyevent".into(), "29".into()]]);
    }

    #[test]
    fn modifier_chord_is_a_keycombination() {
        let got = key_commands(&KeyEvent::Chord("ctrl+a".into())).unwrap();
        assert_eq!(got, vec![vec![
            "shell".to_string(), "input".into(), "keycombination".into(), "113".into(), "29".into(),
        ]]);
    }

    #[test]
    fn multi_modifier_chord_lists_each_meta_then_the_key() {
        let got = key_commands(&KeyEvent::Chord("ctrl+shift+a".into())).unwrap();
        assert_eq!(got, vec![vec![
            "shell".to_string(), "input".into(), "keycombination".into(),
            "113".into(), "59".into(), "29".into(),
        ]]);
    }

    #[test]
    fn function_key_chord_maps_to_f_keycode() {
        let got = key_commands(&KeyEvent::Chord("alt+F4".into())).unwrap();
        assert_eq!(got, vec![vec![
            "shell".to_string(), "input".into(), "keycombination".into(), "57".into(), "134".into(),
        ]]);
    }

    #[test]
    fn unmappable_chord_key_errors() {
        assert!(matches!(
            key_commands(&KeyEvent::Chord("ctrl+/".into())),
            Err(GlassError::InvalidKey(_))
        ));
    }
}
