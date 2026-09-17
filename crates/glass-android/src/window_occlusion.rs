//! Negative evidence from the companion's live window stack; no within-window hit test.

use glass_core::accessibility::{AxContext, AxTarget, AxTree, PointerHit};
use glass_core::{GlassError, Result};
use serde::Deserialize;
use serde_json::Value;

pub(crate) fn screen_point(
    ctx: &AxContext,
    target: &AxTarget,
    (x, y): (i32, i32),
) -> Option<(i32, i32)> {
    let bounds = target.bounds?;
    if u32::try_from(x).ok()? >= ctx.window.width
        || u32::try_from(y).ok()? >= ctx.window.height
        || i64::from(x) < i64::from(bounds.x)
        || i64::from(y) < i64::from(bounds.y)
        || i64::from(x) >= i64::from(bounds.x) + i64::from(bounds.width)
        || i64::from(y) >= i64::from(bounds.y) + i64::from(bounds.height)
    {
        return None;
    }
    Some((x.checked_add(ctx.window.x)?, y.checked_add(ctx.window.y)?))
}

#[derive(Deserialize)]
struct WindowEvidence {
    x: i32,
    y: i32,
    window_id: i32,
    display_id: i32,
    occluding_window_id: Option<i32>,
}

pub(crate) fn classify(
    tree: &AxTree,
    target: &AxTarget,
    asked: &str,
    actual: Option<&str>,
    point: (i32, i32),
    evidence: Option<&Value>,
) -> Result<PointerHit> {
    let Some(actual) = actual.filter(|package| !package.is_empty()) else {
        return Ok(PointerHit::Inconclusive);
    };
    if !asked.is_empty() && actual != asked {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    let current = tree
        .find(target.id)
        .ok_or(GlassError::AxElementChanged(target.id.0))?;
    if !target.matches(current.role, current.name.as_deref())
        || target.bounds != current.bounds
        || !current.states.enabled
    {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    let Some(evidence) = evidence else {
        return Ok(PointerHit::Inconclusive);
    };
    if evidence.get("version").and_then(Value::as_u64) != Some(1) {
        return Ok(PointerHit::Inconclusive);
    }
    let evidence = WindowEvidence::deserialize(evidence).map_err(|error| {
        GlassError::AccessibilityUnavailable(format!(
            "invalid Android window occlusion evidence: {error}"
        ))
    })?;
    if (evidence.x, evidence.y) != point {
        return Err(GlassError::AccessibilityUnavailable(
            "Android window occlusion evidence answered a different point".into(),
        ));
    }
    // Android input currently targets the default display. A missing cover says nothing
    // about another view in the target window, including accessibility-hidden interceptors.
    Ok(
        if evidence.display_id == 0
            && evidence.window_id >= 0
            && evidence
                .occluding_window_id
                .is_some_and(|id| id >= 0 && id != evidence.window_id)
        {
            PointerHit::Other
        } else {
            PointerHit::Inconclusive
        },
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::{AxNodeId, AxRect, AxRole, WalkLimits};
    use glass_core::{Deadline, WindowGeometry};

    fn fixture(bounds: AxRect) -> (AxContext, AxTarget) {
        (
            AxContext {
                pids: vec![],
                window: WindowGeometry {
                    x: -50,
                    y: 100,
                    width: 300,
                    height: 400,
                },
                window_handle: None,
                a11y_bus_addr: None,
                limits: WalkLimits::DEFAULT,
                deadline: Deadline::UNBOUNDED,
            },
            AxTarget {
                id: AxNodeId(1),
                role: AxRole::Button,
                name: Some("Save".into()),
                bounds: Some(bounds),
                value: None,
            },
        )
    }

    #[test]
    fn target_edges_are_half_open_even_when_the_point_is_inside_the_window() {
        let (ctx, target) = fixture(AxRect {
            x: 30,
            y: 40,
            width: 60,
            height: 80,
        });
        for (point, expected) in [
            ((30, 40), Some((-20, 140))),
            ((31, 41), Some((-19, 141))),
            ((89, 119), Some((39, 219))),
            ((29, 60), None),
            ((28, 60), None),
            ((50, 39), None),
            ((50, 38), None),
            ((90, 60), None),
            ((91, 60), None),
            ((50, 120), None),
            ((50, 121), None),
        ] {
            assert_eq!(screen_point(&ctx, &target, point), expected, "{point:?}");
        }
    }

    #[test]
    fn clipped_target_does_not_admit_points_outside_the_window() {
        let (ctx, target) = fixture(AxRect {
            x: -20,
            y: -30,
            width: 400,
            height: 500,
        });
        for (point, expected) in [
            ((0, 0), Some((-50, 100))),
            ((1, 1), Some((-49, 101))),
            ((299, 399), Some((249, 499))),
            ((-1, 20), None),
            ((-2, 20), None),
            ((20, -1), None),
            ((20, -2), None),
            ((300, 20), None),
            ((301, 20), None),
            ((20, 400), None),
            ((20, 401), None),
        ] {
            assert_eq!(screen_point(&ctx, &target, point), expected, "{point:?}");
        }
    }

    #[test]
    fn unknown_or_empty_bounds_have_no_screen_point() {
        let (mut ctx, mut target) = fixture(AxRect {
            x: 0,
            y: 0,
            width: 60,
            height: 80,
        });
        for bounds in [
            None,
            Some(AxRect::default()),
            Some(AxRect {
                width: 60,
                ..AxRect::default()
            }),
            Some(AxRect {
                height: 80,
                ..AxRect::default()
            }),
        ] {
            target.bounds = bounds;
            assert_eq!(screen_point(&ctx, &target, (0, 0)), None, "{bounds:?}");
        }
        target.bounds = Some(AxRect {
            x: 0,
            y: 0,
            width: 60,
            height: 80,
        });
        for (width, height) in [(0, 400), (300, 0)] {
            ctx.window.width = width;
            ctx.window.height = height;
            assert_eq!(screen_point(&ctx, &target, (0, 0)), None);
        }
    }

    #[test]
    fn wide_target_edges_do_not_wrap_and_screen_translation_is_checked_on_both_axes() {
        let (mut ctx, target) = fixture(AxRect {
            x: 1,
            y: 1,
            width: u32::MAX,
            height: u32::MAX,
        });
        assert_eq!(screen_point(&ctx, &target, (20, 30)), Some((-30, 130)));
        for (x, y) in [(i32::MAX, 0), (0, i32::MAX)] {
            ctx.window.x = x;
            ctx.window.y = y;
            assert_eq!(screen_point(&ctx, &target, (20, 30)), None);
        }
    }
}
