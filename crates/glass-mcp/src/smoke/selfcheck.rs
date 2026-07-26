//! Prove the checks can fail. Each scenario feeds a check a response that is wrong in
//! one specific way; the check must report red. A scenario that passes means the
//! corresponding assertion has stopped working.

use crate::smoke::checks;
use crate::smoke::profile::OutlineNode;
use crate::smoke::report::CheckStatus;
use crate::smoke::transport::{CallResult, ScriptedTransport};
use serde_json::json;

/// Faults, and the check that must catch each one.
pub fn run_self_check() -> Result<String, String> {
    let mut escaped = Vec::new();

    // 1. Mutated envelope: `ok` is false.
    let mut t = ScriptedTransport::new(vec![(
        "glass_screenshot",
        Ok(CallResult {
            is_error: false,
            envelope: Some(json!({
                "ok": false,
                "tool": "glass_screenshot",
                "result": { "width": 8, "height": 8 }
            })),
            siblings: vec![],
            images: 1,
        }),
    )]);
    if checks::check_screenshot(&mut t).status != CheckStatus::Fail {
        escaped.push("mutated envelope (ok:false)");
    }

    // 2. App text inside `result` instead of an untrusted sibling.
    let mut t = ScriptedTransport::new(vec![(
        "glass_a11y_snapshot",
        Ok(CallResult {
            is_error: false,
            envelope: Some(json!({
                "ok": true,
                "tool": "glass_a11y_snapshot",
                "result": { "outline": "⟦untrusted:x⟧ #1 Window ⟦/untrusted:x⟧" }
            })),
            siblings: vec![],
            images: 0,
        }),
    )]);
    if checks::check_a11y(&mut t).0.status != CheckStatus::Fail {
        escaped.push("untrusted text inside result");
    }

    // 3. Interaction that reports ok but nothing reports the written value.
    // `check_interaction` verifies through `glass_wait_for_element`'s `value_contains` — the
    // consumer-layer signal glass actually exposes for a written value — not by re-reading a
    // name from the outline. So the fault here is `glass_set_value` returning ok while the
    // follow-up `glass_wait_for_element` comes back unmatched: a write that reported success
    // but that nothing downstream ever observed.
    let nodes = [OutlineNode {
        id: 12,
        role: "TextField".into(),
        name: None,
        states: vec!["editable".into()],
    }];
    let mut t = ScriptedTransport::new(vec![
        (
            "glass_set_value",
            Ok(CallResult {
                is_error: false,
                envelope: Some(json!({
                    "ok": true,
                    "tool": "glass_set_value",
                    "result": { "id": 12 }
                })),
                siblings: vec![],
                images: 0,
            }),
        ),
        (
            "glass_wait_for_element",
            Ok(CallResult {
                is_error: false,
                envelope: Some(json!({
                    "ok": true,
                    "tool": "glass_wait_for_element",
                    "result": { "matched": false, "elapsed_ms": 5000 }
                })),
                siblings: vec![],
                images: 0,
            }),
        ),
    ]);
    if checks::check_interaction(&mut t, &nodes).status != CheckStatus::Fail {
        escaped.push("no-op interaction reported ok");
    }

    // 4. Error message stripped of its remedy.
    let mut t = ScriptedTransport::new(vec![(
        "glass_click_element",
        Ok(CallResult {
            is_error: true,
            envelope: None,
            siblings: vec!["unknown element id 999999".into()],
            images: 0,
        }),
    )]);
    if checks::check_error_honesty(&mut t).status != CheckStatus::Fail {
        escaped.push("remedy-less error message");
    }

    if escaped.is_empty() {
        Ok("4 injected faults, all caught".into())
    } else {
        Err(format!("faults not caught: {}", escaped.join("; ")))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_injected_fault_is_caught() {
        run_self_check().expect("a fault slipped through");
    }
}
