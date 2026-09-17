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
    if x < 0
        || y < 0
        || x as u32 >= ctx.window.width
        || y as u32 >= ctx.window.height
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
