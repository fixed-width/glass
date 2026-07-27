//! Prove the checks can fail — for the reason each scenario is named after, not just any
//! reason. Matching on `CheckStatus::Fail` alone is not enough: a fixture that leaves an
//! unrelated fallback path to `Fail` behind the assertion under test (an exhausted script
//! queue, a missing-sibling short-circuit) would still go red if that assertion were deleted,
//! certifying a broken guard as working. So every scenario also asserts on a `detail`
//! substring distinctive to the assertion it targets.

use crate::smoke::checks;
use crate::smoke::profile::{OutlineNode, Profile, X11_CANDIDATES};
use crate::smoke::report::{CheckOutcome, CheckStatus};
use crate::smoke::transport::{CALL_TIMEOUT, CallResult, ScriptedTransport};
use serde_json::json;

/// One fault-injection scenario: the outcome a check produced against a deliberately wrong
/// response, and a `detail` substring unique to the assertion it targets. `status` alone
/// cannot distinguish "failed for the intended reason" from "failed some other way".
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

    /// `contains("")` is always true, so an empty `expect_detail` would silently revert a
    /// scenario to the status-only check this module exists to argue against.
    fn names_an_assertion(&self) -> Result<(), String> {
        if self.expect_detail.is_empty() {
            return Err(format!(
                "scenario {:?} has an empty expect_detail, which matches any failure",
                self.name
            ));
        }
        Ok(())
    }
}

/// How many faults [`run_self_check`] injects. Written out rather than read off the array, and
/// annotated on it: a scenario dropped without this being changed with it is then a length
/// mismatch the compiler rejects, rather than a self-check that reports success having injected
/// one fewer. cargo-mutants does not mutate array literals, so no sweep would report that.
const INJECTED_FAULTS: usize = 6;

/// Faults, and the check that must catch each one — for the reason each is named after.
pub fn run_self_check() -> Result<String, String> {
    let scenarios: [Scenario; INJECTED_FAULTS] = [
        mutated_envelope_scenario(),
        untrusted_text_in_result_scenario(),
        noop_interaction_scenario(),
        remedyless_error_scenario(),
        dropped_error_message_scenario(),
        windowless_start_scenario(),
    ];

    for s in &scenarios {
        s.names_an_assertion()?;
    }
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

/// Scenario 1, mutated envelope: `ok` is false. `check_envelope`'s `ok`-guard must catch this,
/// and its error text always ends "expected true" — a phrase no other failure path in
/// `check_screenshot` produces, so matching it rules out a failure on the image count or
/// dimensions instead.
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
/// parseable outline content. The sibling must be real: were it empty, deleting
/// `check_envelope`'s rule that untrusted markers must never appear inside `result` would
/// still leave `check_a11y` failing on `untrusted_sibling`'s missing-marker error, hiding the
/// removal. With a valid sibling, deleting that rule lets `check_a11y` reach a genuine `Pass`.
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
/// signal `check_interaction` verifies against — never confirms the write (`matched: false`).
/// The calls after it are scripted too, and the response carries no sibling (so
/// `matched_element_id` reads `None`, not a mismatch): deleting the `matched` guard therefore
/// falls through to a genuine `Pass`, rather than going red on an exhausted script queue.
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
            Ok(CallResult::ok(
                "glass_set_value",
                json!({ "id": 12 }),
                &[],
                0,
            )),
        ),
        (
            "glass_wait_for_element",
            Ok(CallResult::ok(
                "glass_wait_for_element",
                json!({ "matched": false, "elapsed_ms": 5000 }),
                &[],
                0,
            )),
        ),
        (
            "glass_click",
            Ok(CallResult::ok("glass_click", json!({}), &[], 0)),
        ),
        (
            "glass_key",
            Ok(CallResult::ok("glass_key", json!({}), &[], 0)),
        ),
        (
            "glass_list_windows",
            Ok(CallResult::ok(
                "glass_list_windows",
                json!({ "count": 1 }),
                &[],
                0,
            )),
        ),
    ]);
    Scenario {
        name: "no-op interaction reported ok",
        outcome: checks::check_interaction(&mut t, Some(&nodes)),
        expect_detail: "no element reported the written value",
    }
}

/// Scenario 4, error message names a cause but no remedy. `names_a_remedy`'s rejection must
/// catch this; its text always contains "no remedy", distinguishing it from the sibling
/// failure mode this same check guards ("succeeded" when misuse is not rejected at all).
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

/// Scenario 5: an ordinary tool error, whose message the report must carry. `check_envelope`'s
/// `is_error` arm must catch this; the substring is glass-core's own wording for the injected
/// error. Deleting the arm does not make this pass — it makes `check_logs` fall through to
/// "no `{ok,tool,result}` envelope in the first content block", still `Fail` but no longer
/// naming what the server said, so the fault reports as escaped.
///
/// Driven through `check_logs` deliberately: the four scenarios above exercise `screenshot`,
/// `a11y snapshot`, `interaction` and `error honesty`, so this reaches a check none of them do.
fn dropped_error_message_scenario() -> Scenario {
    let mut t = ScriptedTransport::new(vec![(
        "glass_logs",
        Ok(CallResult {
            is_error: true,
            envelope: None,
            siblings: vec![glass_core::GlassError::NoActiveSession.to_string()],
            images: 0,
        }),
    )]);
    Scenario {
        name: "tool error message dropped",
        outcome: checks::check_logs(&mut t),
        expect_detail: "no active session",
    }
}

/// Scenario 6: `glass_start` reports ok for a window with no `height`. `check_start`'s geometry
/// guard must catch this; its text names the missing field, which no other failure path in that
/// check produces — a transport error carries the server's own message instead. Deleting the guard
/// does not go red another way: the detail is then interpolated straight from the response, so the
/// check reaches a genuine `Pass` on a launch that established no window.
///
/// Driven through `check_start` deliberately: it is the check whose whole job is to notice a
/// broken launch, and no scenario above reaches it.
fn windowless_start_scenario() -> Scenario {
    let mut t = ScriptedTransport::new(vec![(
        "glass_start",
        Ok(CallResult::ok(
            "glass_start",
            json!({ "x": 0, "y": 0, "width": 800 }),
            &[],
            0,
        )),
    )]);
    let p = Profile {
        backend: "x11".to_string(),
        app: &X11_CANDIDATES[0],
        // Nothing to offer: an offer is appended only on the failure path, and a scenario whose
        // detail carried one would no longer be scoped to the guard under test.
        alternatives: vec![],
        start_deadline: CALL_TIMEOUT,
    };
    Scenario {
        name: "start reported a window with no height",
        outcome: checks::check_start(&mut t, &p),
        expect_detail: "no `height` in its geometry",
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_injected_fault_is_caught() {
        run_self_check().expect("a fault slipped through");
    }

    #[test]
    fn an_empty_expect_detail_is_rejected_rather_than_matching_anything() {
        let s = Scenario {
            name: "unnamed assertion",
            outcome: CheckOutcome::fail(1, "start", "any failure at all"),
            expect_detail: "",
        };
        assert!(
            s.caught(),
            "an empty substring matches anything — which is exactly why it must be rejected"
        );
        let err = s.names_an_assertion().unwrap_err();
        assert!(err.contains("unnamed assertion"), "got: {err}");
    }

    fn scenario(outcome: CheckOutcome, expect_detail: &'static str) -> Scenario {
        Scenario {
            name: "fixture",
            outcome,
            expect_detail,
        }
    }

    #[test]
    fn a_check_that_failed_for_the_named_reason_is_caught() {
        let s = scenario(
            CheckOutcome::fail(1, "start", "expected true, got false"),
            "expected true",
        );
        assert!(s.caught());
    }

    /// `Fail`, but for the wrong reason — the status-only match the module header argues against.
    #[test]
    fn a_check_that_failed_for_another_reason_is_not_caught() {
        let s = scenario(
            CheckOutcome::fail(1, "start", "script exhausted"),
            "expected true",
        );
        assert!(!s.caught());
    }

    /// A passing check has not been caught however well its detail reads.
    #[test]
    fn a_check_that_passed_is_not_caught() {
        let s = scenario(
            CheckOutcome::pass(1, "start", "expected true"),
            "expected true",
        );
        assert!(!s.caught());
    }

    /// The Ok message is the only thing a caller sees on success, so it must state what was
    /// actually exercised. The count is written out rather than taken from [`INJECTED_FAULTS`]:
    /// a count checked against its own source is equally true of "0 injected faults, all
    /// caught", which is what an emptied scenario list would report.
    #[test]
    fn the_success_message_names_how_many_faults_were_injected() {
        assert_eq!(
            run_self_check().expect("the shipped scenarios must all be caught"),
            "6 injected faults, all caught"
        );
    }
}
