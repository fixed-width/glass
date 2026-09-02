//! Pointer and keyboard tools.

use glass_core::{Glass, KeyEvent, Modifier, MouseButton, PointerEvent};

use crate::params::*;
use crate::tools::{
    ContextualError, ContextualOutput, ContextualToolResult, ToolContext, ToolOutput, ToolResult,
    parse_button,
};

fn standalone(result: ContextualToolResult) -> ToolResult {
    result.map(|o| o.output).map_err(|e| e.message)
}

pub(crate) fn parse_modifiers(mods: Option<&[String]>) -> Result<Vec<Modifier>, String> {
    let mut out = Vec::new();
    for m in mods.unwrap_or(&[]) {
        out.push(Modifier::from_name(m).ok_or_else(|| {
            format!("unknown modifier '{m}' (use ctrl/shift/alt/super; cmd = super on macOS)")
        })?);
    }
    Ok(out)
}

fn validate_click_count(count: Option<u32>) -> Result<u32, ContextualError> {
    let count = count.unwrap_or(1);
    if !(1..=MAX_CLICK_COUNT).contains(&count) {
        return Err(ContextualError::validation(format!(
            "`count` must be between 1 and {MAX_CLICK_COUNT}"
        )));
    }
    Ok(count)
}

pub(crate) fn validate_click_args(
    a: &ClickArgs,
) -> Result<(MouseButton, Vec<Modifier>, u32), ContextualError> {
    let count = validate_click_count(a.count)?;
    let button = parse_button(a.button.as_deref()).map_err(ContextualError::validation)?;
    let modifiers = parse_modifiers(a.modifiers.as_deref()).map_err(ContextualError::validation)?;
    Ok((button, modifiers, count))
}

pub(crate) fn validate_drag_args(
    a: &DragArgs,
) -> Result<(MouseButton, Vec<Modifier>), ContextualError> {
    let button = parse_button(a.button.as_deref()).map_err(ContextualError::validation)?;
    let modifiers = parse_modifiers(a.modifiers.as_deref()).map_err(ContextualError::validation)?;
    Ok((button, modifiers))
}

pub(crate) fn validate_scroll_args(
    a: &ScrollArgs,
) -> Result<(i32, i32, Vec<Modifier>), ContextualError> {
    let dx = a.dx.unwrap_or(0);
    let dy = a.dy.unwrap_or(0);
    let valid = -MAX_SCROLL_NOTCHES..=MAX_SCROLL_NOTCHES;
    if !valid.contains(&dx) || !valid.contains(&dy) {
        return Err(ContextualError::validation(format!(
            "`dx` and `dy` must each be between -{MAX_SCROLL_NOTCHES} and {MAX_SCROLL_NOTCHES}"
        )));
    }
    let modifiers = parse_modifiers(a.modifiers.as_deref()).map_err(ContextualError::validation)?;
    Ok((dx, dy, modifiers))
}

pub(crate) fn validate_key_args(a: &KeyArgs) -> Result<(), ContextualError> {
    glass_core::keys::parse_chord(&a.chord)
        .map(|_| ())
        .map_err(|error| ContextualError::validation(error.to_string()))
}

pub fn click(glass: &mut Glass, a: &ClickArgs) -> ToolResult {
    standalone(click_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn click_with(
    glass: &mut Glass,
    a: &ClickArgs,
    context: ToolContext,
) -> ContextualToolResult {
    let (button, modifiers, count) = validate_click_args(a)?;
    glass
        .pointer_by(
            &PointerEvent::Click {
                x: a.x,
                y: a.y,
                button,
                count,
                modifiers,
            },
            context.deadline,
        )
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    Ok(ContextualOutput::immediate(ToolOutput::result(
        "glass_click",
        serde_json::json!({}),
    )))
}

pub fn mouse_move(glass: &mut Glass, a: &MoveArgs) -> ToolResult {
    standalone(mouse_move_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn mouse_move_with(
    glass: &mut Glass,
    a: &MoveArgs,
    context: ToolContext,
) -> ContextualToolResult {
    glass
        .pointer_by(&PointerEvent::Move { x: a.x, y: a.y }, context.deadline)
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    Ok(ContextualOutput::immediate(ToolOutput::result(
        "glass_move",
        serde_json::json!({}),
    )))
}

pub fn drag(glass: &mut Glass, a: &DragArgs) -> ToolResult {
    standalone(drag_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn drag_with(
    glass: &mut Glass,
    a: &DragArgs,
    context: ToolContext,
) -> ContextualToolResult {
    let (button, modifiers) = validate_drag_args(a)?;
    glass
        .pointer_by(
            &PointerEvent::Drag {
                from_x: a.x1,
                from_y: a.y1,
                to_x: a.x2,
                to_y: a.y2,
                button,
                modifiers,
                duration_ms: a.duration_ms.unwrap_or(200).min(10_000),
            },
            context.deadline,
        )
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    Ok(ContextualOutput::immediate(ToolOutput::result(
        "glass_drag",
        serde_json::json!({}),
    )))
}

pub fn gesture(glass: &mut Glass, a: &GestureArgs) -> ToolResult {
    let n = a.pointers.len();
    if n < 2 {
        return Err("glass_gesture needs 2+ pointers; use glass_drag for a single pointer".into());
    }
    if n > glass_core::MAX_GESTURE_POINTERS {
        return Err(format!(
            "too many pointers ({n}); max is {}",
            glass_core::MAX_GESTURE_POINTERS
        ));
    }
    let pointers = a
        .pointers
        .iter()
        .map(|p| glass_core::Segment {
            from_x: p.from.x,
            from_y: p.from.y,
            to_x: p.to.x,
            to_y: p.to.y,
        })
        .collect();
    glass
        .pointer(&PointerEvent::Gesture {
            pointers,
            duration_ms: a.duration_ms.unwrap_or(250).min(10_000),
        })
        .map_err(|e| e.to_string())?;
    Ok(ToolOutput::result("glass_gesture", serde_json::json!({})))
}

pub fn scroll(glass: &mut Glass, a: &ScrollArgs) -> ToolResult {
    standalone(scroll_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn scroll_with(
    glass: &mut Glass,
    a: &ScrollArgs,
    context: ToolContext,
) -> ContextualToolResult {
    let (dx, dy, modifiers) = validate_scroll_args(a)?;
    glass
        .pointer_by(
            &PointerEvent::Scroll {
                x: a.x,
                y: a.y,
                dx,
                dy,
                modifiers,
            },
            context.deadline,
        )
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    Ok(ContextualOutput::immediate(ToolOutput::result(
        "glass_scroll",
        serde_json::json!({}),
    )))
}

pub fn type_text(glass: &mut Glass, a: &TypeArgs) -> ToolResult {
    standalone(type_text_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn type_text_with(
    glass: &mut Glass,
    a: &TypeArgs,
    context: ToolContext,
) -> ContextualToolResult {
    match crate::tools::semantic_action::validate_type_args(a)? {
        crate::tools::semantic_action::ValidatedType::Untargeted => glass
            .key_by(&KeyEvent::Text(a.text.clone()), context.deadline)
            .map_err(|e| ContextualError::from_caller_bound(e, context))?,
        crate::tools::semantic_action::ValidatedType::Targeted(params) => {
            glass
                .type_target_by(&params, &a.text, context.deadline)
                .map_err(|error| {
                    let message = error.to_string();
                    match error.source {
                        Some(source) => ContextualError::from_caller_bound(source, context),
                        None => ContextualError::from_caller_bound(
                            glass_core::GlassError::Backend(message),
                            context,
                        ),
                    }
                })?;
        }
    }
    // Past this point the keystrokes have landed: a failing observe (e.g. `snapshot`
    // with no a11y reader) must say so, or an agent retries and types the text twice.
    let (observed, extra, timed_out_by) =
        crate::tools::resolve_return_with(glass, a.return_.as_deref(), context).map_err(|e| {
            e.after_dispatch()
                .annotate("text was typed; return observe failed")
        })?;
    let mut result = serde_json::json!({});
    if let Some(o) = observed {
        result["observed"] = o;
    }
    Ok(ContextualOutput::with_timeout(
        ToolOutput::result_with("glass_type", result, extra),
        timed_out_by,
    ))
}

pub fn key(glass: &mut Glass, a: &KeyArgs) -> ToolResult {
    standalone(key_with(glass, a, ToolContext::UNBOUNDED))
}

pub(crate) fn key_with(
    glass: &mut Glass,
    a: &KeyArgs,
    context: ToolContext,
) -> ContextualToolResult {
    validate_key_args(a)?;
    glass
        .key_by(&KeyEvent::Chord(a.chord.clone()), context.deadline)
        .map_err(|e| ContextualError::from_caller_bound(e, context))?;
    Ok(ContextualOutput::immediate(ToolOutput::result(
        "glass_key",
        serde_json::json!({}),
    )))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::start as start_tool;
    use crate::tools::testutil::*;
    use std::sync::{Arc, Mutex};

    fn started() -> Glass {
        started_with(FakePlatform::new(100, 100))
    }

    fn started_with(platform: FakePlatform) -> Glass {
        let mut g = glass_with(platform);
        let a = StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            window_hint: None,
            timeout_ms: None,
            a11y: None,
        };
        start_tool(&mut g, &a).unwrap();
        g
    }

    /// These input tools all return an empty `result` on success; assert the envelope
    /// shape (ok/tool/registered) plus that emptiness in one call.
    fn assert_ok(out: &ToolOutput, tool: &str) {
        let v = assert_envelope(out, tool);
        assert_eq!(v, serde_json::json!({}), "envelope result: {v}");
    }

    #[test]
    fn click_in_bounds_ok() {
        let mut g = started();
        let a = ClickArgs {
            x: 10,
            y: 20,
            button: None,
            count: None,
            modifiers: None,
        };
        assert_ok(&click(&mut g, &a).unwrap(), "glass_click");
    }

    #[test]
    fn move_in_bounds_ok() {
        let mut g = started();
        let a = MoveArgs { x: 10, y: 20 };
        assert_ok(&mouse_move(&mut g, &a).unwrap(), "glass_move");
    }

    #[test]
    fn click_out_of_bounds_errors() {
        let mut g = started();
        let a = ClickArgs {
            x: 100,
            y: 20,
            button: None,
            count: None,
            modifiers: None,
        }; // valid 0..=99
        assert!(click(&mut g, &a).unwrap_err().contains("out of bounds"));
    }

    #[test]
    fn bad_button_errors() {
        let mut g = started();
        let a = ClickArgs {
            x: 1,
            y: 1,
            button: Some("nope".into()),
            count: None,
            modifiers: None,
        };
        assert!(click(&mut g, &a).unwrap_err().contains("unknown button"));
    }

    #[test]
    fn click_rejects_zero_and_unbounded_count_before_pointer_dispatch() {
        for (count, expected) in [
            (0, "between 1 and 10"),
            (MAX_CLICK_COUNT + 1, "between 1 and 10"),
            (u32::MAX, "between 1 and 10"),
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut g =
                started_with(FakePlatform::new(100, 100).with_event_log(Arc::clone(&events)));
            let error = click(
                &mut g,
                &ClickArgs {
                    x: 1,
                    y: 1,
                    button: None,
                    count: Some(count),
                    modifiers: None,
                },
            )
            .unwrap_err();

            assert!(
                error.contains(expected),
                "unexpected error for {count}: {error}"
            );
            assert!(
                events.lock().unwrap().is_empty(),
                "invalid count {count} reached the platform"
            );
        }
    }

    #[test]
    fn type_and_key_ok() {
        let mut g = started();
        assert_ok(
            &type_text(
                &mut g,
                &TypeArgs {
                    target: None,
                    focus_mode: None,
                    timeout_ms: None,
                    max_nodes: None,
                    text: "hi".into(),
                    return_: None,
                },
            )
            .unwrap(),
            "glass_type",
        );
        assert_ok(
            &key(
                &mut g,
                &KeyArgs {
                    chord: "ctrl+s".into(),
                },
            )
            .unwrap(),
            "glass_key",
        );
    }

    #[test]
    fn drag_and_scroll_ok() {
        let mut g = started();
        let d = DragArgs {
            x1: 1,
            y1: 2,
            x2: 3,
            y2: 4,
            button: None,
            modifiers: None,
            duration_ms: None,
        };
        assert_ok(&drag(&mut g, &d).unwrap(), "glass_drag");
        let s = ScrollArgs {
            x: 5,
            y: 6,
            dx: None,
            dy: Some(2),
            modifiers: None,
        };
        assert_ok(&scroll(&mut g, &s).unwrap(), "glass_scroll");
    }

    #[test]
    fn scroll_rejects_extreme_magnitudes_before_pointer_dispatch() {
        for (dx, dy) in [
            (Some(-MAX_SCROLL_NOTCHES - 1), None),
            (Some(MAX_SCROLL_NOTCHES + 1), None),
            (Some(i32::MIN), None),
            (Some(i32::MAX), None),
            (None, Some(-MAX_SCROLL_NOTCHES - 1)),
            (None, Some(MAX_SCROLL_NOTCHES + 1)),
            (None, Some(i32::MIN)),
            (None, Some(i32::MAX)),
        ] {
            let events = Arc::new(Mutex::new(Vec::new()));
            let mut g =
                started_with(FakePlatform::new(100, 100).with_event_log(Arc::clone(&events)));
            let error = scroll(
                &mut g,
                &ScrollArgs {
                    x: 1,
                    y: 1,
                    dx,
                    dy,
                    modifiers: None,
                },
            )
            .unwrap_err();

            assert!(
                error.contains("between -100 and 100"),
                "unexpected error for ({dx:?}, {dy:?}): {error}"
            );
            assert!(
                events.lock().unwrap().is_empty(),
                "invalid scroll ({dx:?}, {dy:?}) reached the platform"
            );
        }
    }

    #[test]
    fn gesture_two_pointers_ok() {
        let mut g = started();
        let a = GestureArgs {
            pointers: vec![
                PointerArgs {
                    from: PointArg { x: 30, y: 40 },
                    to: PointArg { x: 10, y: 40 },
                },
                PointerArgs {
                    from: PointArg { x: 50, y: 40 },
                    to: PointArg { x: 70, y: 40 },
                },
            ],
            duration_ms: Some(120),
        };
        assert_ok(&gesture(&mut g, &a).unwrap(), "glass_gesture");
    }

    #[test]
    fn gesture_one_pointer_errors() {
        let mut g = started();
        let a = GestureArgs {
            pointers: vec![PointerArgs {
                from: PointArg { x: 1, y: 1 },
                to: PointArg { x: 2, y: 2 },
            }],
            duration_ms: None,
        };
        assert!(gesture(&mut g, &a).is_err());
    }

    #[test]
    fn click_parses_and_rejects_modifiers() {
        let mut g = started();
        let ok = ClickArgs {
            x: 1,
            y: 1,
            button: None,
            count: None,
            modifiers: Some(vec!["ctrl".into()]),
        };
        assert_ok(&click(&mut g, &ok).unwrap(), "glass_click");
        let bad = ClickArgs {
            x: 1,
            y: 1,
            button: None,
            count: None,
            modifiers: Some(vec!["hyper".into()]),
        };
        assert!(
            click(&mut g, &bad)
                .unwrap_err()
                .contains("unknown modifier")
        );
    }
}
