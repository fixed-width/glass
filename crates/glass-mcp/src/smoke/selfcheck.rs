//! Prove the checks can fail — for the reason each scenario is named after, not just any
//! reason. Matching on `CheckStatus::Fail` alone is not enough: a fixture that leaves an
//! unrelated fallback path to `Fail` sitting behind the assertion under test (an exhausted
//! script queue, a missing-sibling short-circuit, ...) would still go red if that assertion
//! were deleted — "caught" for the wrong reason, which would certify a broken guard as
//! working. So every scenario here also asserts on a `detail` substring distinctive to the
//! specific assertion it targets.

use crate::smoke::checks;
use crate::smoke::profile::OutlineNode;
use crate::smoke::report::{CheckOutcome, CheckStatus};
use crate::smoke::transport::{CallResult, ScriptedTransport};
use serde_json::{Value, json};

/// One fault-injection scenario: the outcome a check produced against a deliberately wrong
/// response, and a `detail` substring unique to the assertion it targets. `status` alone
/// cannot distinguish "failed for the intended reason" from "failed some other way" — see
/// the module doc.
struct Scenario {
    name: &'static str,
    outcome: CheckOutcome,
    expect_detail: &'static str,
}

impl Scenario {
    /// Caught means both: the check went red, AND it named the assertion this scenario
    /// targets — not some other failure path that also happens to return `Fail`.
    fn caught(&self) -> bool {
        self.outcome.status == CheckStatus::Fail && self.outcome.detail.contains(self.expect_detail)
    }
}

/// Faults, and the check that must catch each one — for the reason each is named after.
pub fn run_self_check() -> Result<String, String> {
    let scenarios = [
        mutated_envelope_scenario(),
        untrusted_text_in_result_scenario(),
        noop_interaction_scenario(),
        remedyless_error_scenario(),
    ];

    let escaped: Vec<&str> = scenarios
        .iter()
        .filter(|s| !s.caught())
        .map(|s| s.name)
        .collect();
    if escaped.is_empty() {
        Ok(format!("{} injected faults, all caught", scenarios.len()))
    } else {
        Err(format!("faults not caught: {}", escaped.join("; ")))
    }
}

fn ok_call(tool: &str, result: Value, siblings: Vec<&str>, images: usize) -> CallResult {
    CallResult {
        is_error: false,
        envelope: Some(json!({ "ok": true, "tool": tool, "result": result })),
        siblings: siblings.into_iter().map(String::from).collect(),
        images,
    }
}

/// Scenario 1, mutated envelope: `ok` is false. `check_envelope`'s `ok`-guard is what must catch
/// this, and its error text always ends "expected true" — a phrase no other failure path
/// in `check_screenshot` produces, so matching it rules out "failed because the image count
/// or dimensions happened to be wrong instead".
fn mutated_envelope_scenario() -> Scenario {
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
    Scenario {
        name: "mutated envelope (ok:false)",
        outcome: checks::check_screenshot(&mut t),
        expect_detail: "expected true",
    }
}

/// Scenario 2, app text inside `result`, *alongside* a genuinely valid untrusted sibling with
/// parseable outline content. The sibling must be real: if it were empty, deleting
/// `check_envelope`'s A1 rule (untrusted markers must never appear inside `result`) would
/// still leave `check_a11y` failing — via `untrusted_sibling`'s missing-marker error — for
/// an unrelated reason, so removing the assertion under test would go undetected. With a
/// valid sibling present, deleting that rule lets `check_a11y` fall through to it, parse a
/// nonempty tree, and reach a genuine `Pass` — which is what makes this scenario meaningful.
fn untrusted_text_in_result_scenario() -> Scenario {
    let sibling = "⟦untrusted:cafe⟧\n#1 Window \"Untitled\"\n  #2 TextField \"Body\" [editable]\n⟦/untrusted:cafe⟧";
    let mut t = ScriptedTransport::new(vec![(
        "glass_a11y_snapshot",
        Ok(CallResult {
            is_error: false,
            envelope: Some(json!({
                "ok": true,
                "tool": "glass_a11y_snapshot",
                "result": { "outline": "⟦untrusted:x⟧ #1 Window ⟦/untrusted:x⟧" }
            })),
            siblings: vec![sibling.to_string()],
            images: 0,
        }),
    )]);
    Scenario {
        name: "untrusted text inside result",
        outcome: checks::check_a11y(&mut t).0,
        expect_detail: "inside `result`",
    }
}

/// Scenario 3: `glass_set_value` reports ok, but `glass_wait_for_element` — the consumer-layer
/// signal `check_interaction` actually verifies against — never confirms the write
/// (`matched: false`). The pixel-path calls (`glass_click`, `glass_key`) are scripted too,
/// and the `wait_for_element` response carries no sibling (so `matched_element_id` reads
/// `None`, not a mismatch): if `check_interaction`'s `matched` guard were deleted, execution
/// would fall through those two scripted calls to a genuine `Pass`, rather than hitting
/// `ScriptedTransport`'s exhausted-queue error and going red for an unrelated reason.
fn noop_interaction_scenario() -> Scenario {
    let nodes = [OutlineNode {
        id: 12,
        role: "TextField".into(),
        name: None,
        states: vec!["editable".into()],
    }];
    let mut t = ScriptedTransport::new(vec![
        (
            "glass_set_value",
            Ok(ok_call("glass_set_value", json!({ "id": 12 }), vec![], 0)),
        ),
        (
            "glass_wait_for_element",
            Ok(ok_call(
                "glass_wait_for_element",
                json!({ "matched": false, "elapsed_ms": 5000 }),
                vec![],
                0,
            )),
        ),
        (
            "glass_click",
            Ok(ok_call("glass_click", json!({}), vec![], 0)),
        ),
        ("glass_key", Ok(ok_call("glass_key", json!({}), vec![], 0))),
    ]);
    Scenario {
        name: "no-op interaction reported ok",
        outcome: checks::check_interaction(&mut t, &nodes),
        expect_detail: "no element reported the written value",
    }
}

/// Scenario 4, error message names a cause but no remedy. `names_a_remedy`'s rejection is what must
/// catch this; its error text always contains "no remedy", distinguishing it from the
/// sibling failure mode this same check guards ("succeeded" when misuse is not rejected
/// at all).
fn remedyless_error_scenario() -> Scenario {
    let mut t = ScriptedTransport::new(vec![(
        "glass_click_element",
        Ok(CallResult {
            is_error: true,
            envelope: None,
            siblings: vec!["unknown element id 999999".into()],
            images: 0,
        }),
    )]);
    Scenario {
        name: "remedy-less error message",
        outcome: checks::check_error_honesty(&mut t),
        expect_detail: "no remedy",
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
