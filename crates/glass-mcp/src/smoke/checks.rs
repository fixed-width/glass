//! The release checks. Each takes the transport seam and returns one outcome, so
//! every one is unit-testable against a scripted transport — no display required.

use crate::smoke::envelope::{check_envelope, untrusted_sibling};
use crate::smoke::profile::{OutlineNode, Profile, parse_outline};
use crate::smoke::report::CheckOutcome;
use crate::smoke::transport::{CallResult, McpTransport};
use serde_json::{Value, json};

/// Fold a `Result` into an outcome so a transport error is a reported failure,
/// never a panic that loses the rest of the run.
fn outcome(step: u8, name: &str, r: Result<String, String>) -> CheckOutcome {
    match r {
        Ok(detail) => CheckOutcome::pass(step, name, detail),
        Err(detail) => CheckOutcome::fail(step, name, detail),
    }
}

pub fn check_version(t: &mut dyn McpTransport, expected: &str) -> CheckOutcome {
    outcome(
        1,
        "version",
        t.server_version().and_then(|got| {
            if got == expected {
                Ok(got)
            } else {
                Err(format!(
                    "binary reports {got}, expected {expected} — wrong or stale artifact"
                ))
            }
        }),
    )
}

pub fn check_start(t: &mut dyn McpTransport, p: &Profile) -> CheckOutcome {
    let mut run = vec![Value::String(p.app.bin.to_string())];
    run.extend(p.app.args.iter().map(|a| Value::String((*a).to_string())));
    let args = json!({ "run": run, "backend": p.backend, "a11y": true });
    outcome(
        2,
        "start",
        t.call("glass_start", args).and_then(|r| {
            let result = check_envelope("glass_start", &r)?;
            for field in ["x", "y", "width", "height"] {
                if !result[field].is_number() {
                    return Err(format!("glass_start returned no `{field}` in its geometry"));
                }
            }
            Ok(format!("{}x{}", result["width"], result["height"]))
        }),
    )
}

pub fn check_health(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(3, "capabilities+doctor", health(t))
}

/// `check_health`'s body. `glass_doctor`'s `overall` already applies its severity rule (a
/// non-default backend's failing check is only a warning there), so this reads `overall`
/// as the verdict rather than re-deriving one from individual check statuses — walking
/// `sections[].checks[]` is only for naming which check(s) failed.
fn health(t: &mut dyn McpTransport) -> Result<String, String> {
    let caps = t.call("glass_capabilities", json!({}))?;
    let caps = check_envelope("glass_capabilities", &caps)?;
    let doc = t.call("glass_doctor", json!({}))?;
    let doc = check_envelope("glass_doctor", &doc)?;

    if doc["overall"].as_str() == Some("fail") {
        let failed: Vec<&str> = doc["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|s| s["checks"].as_array().into_iter().flatten())
            .filter(|c| c["status"].as_str() == Some("fail"))
            .map(|c| c["name"].as_str().unwrap_or("<unnamed>"))
            .collect();
        return Err(if failed.is_empty() {
            // Silence here would be worse than an odd message: the run truly failed.
            "doctor overall verdict is fail, but no individual check reports fail".to_string()
        } else {
            format!("doctor FAIL: {}", failed.join(", "))
        });
    }

    let backend = caps["backend"].as_str().unwrap_or("<unknown>");
    Ok(format!("backend {backend}"))
}

pub fn check_screenshot(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        4,
        "screenshot",
        t.call("glass_screenshot", json!({})).and_then(|r| {
            let result = check_envelope("glass_screenshot", &r)?;
            if r.images == 0 {
                return Err("glass_screenshot returned no image block".into());
            }
            if !result["width"].is_number() || !result["height"].is_number() {
                return Err("glass_screenshot envelope has no width/height".into());
            }
            Ok(format!("{}x{} image", result["width"], result["height"]))
        }),
    )
}

/// Returns the outcome and the parsed tree, which the interaction check needs.
pub fn check_a11y(t: &mut dyn McpTransport) -> (CheckOutcome, Vec<OutlineNode>) {
    let mut nodes = Vec::new();
    let r = (|| {
        let call = t.call("glass_a11y_snapshot", json!({}))?;
        check_envelope("glass_a11y_snapshot", &call)?;
        let body = untrusted_sibling(&call)?;
        nodes = parse_outline(body);
        if nodes.is_empty() {
            return Err("accessibility tree is empty".into());
        }
        Ok(format!("{} nodes", nodes.len()))
    })();
    (outcome(5, "a11y snapshot", r), nodes)
}

/// The text the interaction check writes. Distinctive so a stale tree cannot pass.
const PROBE_TEXT: &str = "glass smoke";

/// Both interaction paths. The element path is verified through
/// `glass_wait_for_element`'s `value_contains` — the signal glass actually exposes for a
/// written value. A written text field's content lands in the a11y `value` property, not
/// `name`, and the outline text glass renders for an agent carries only `name` — so
/// "the tool returned ok" is not evidence the app changed, and neither is a re-read of the
/// outline.
pub fn check_interaction(t: &mut dyn McpTransport, nodes: &[OutlineNode]) -> CheckOutcome {
    let Some(target) = crate::smoke::profile::first_editable(nodes) else {
        return CheckOutcome::skip(6, "interaction", "no editable element in the tree");
    };
    let id = target.id;
    // An unmapped role renders as `Other(<native token>)` in the outline (see
    // `glass_core::outline::write_line`), which `AxRole::from_name` cannot parse back — so
    // `glass_wait_for_element`'s `role` selector cannot target it. A skip that says why beats
    // a fail that blames the app for a role glass itself cannot address.
    if target.role.contains('(') {
        return CheckOutcome::skip(
            6,
            "interaction",
            format!(
                "target element #{id} has no addressable role ({}) to verify against",
                target.role
            ),
        );
    }
    let role = target.role.as_str();
    outcome(
        6,
        "interaction",
        (|| {
            let set = t.call("glass_set_value", json!({ "id": id, "text": PROBE_TEXT }))?;
            check_envelope("glass_set_value", &set)?;

            let wait = t.call(
                "glass_wait_for_element",
                json!({ "role": role, "value_contains": PROBE_TEXT, "timeout_ms": 5000 }),
            )?;
            let result = check_envelope("glass_wait_for_element", &wait)?;
            if result["matched"].as_bool() != Some(true) {
                return Err(
                    "glass_set_value returned ok but no element reported the written value"
                        .to_string(),
                );
            }
            // The matched element rides in an untrusted sibling (see
            // `crate::tools::wait::element_sibling`); a missing/unparseable sibling or a
            // sibling with no `id` means the id cannot be confirmed, not that it mismatches —
            // only a definite mismatch fails the check.
            if let Some(matched_id) = matched_element_id(&wait)
                && matched_id != u64::from(id)
            {
                return Err(format!(
                    "glass_set_value wrote to #{id} but element #{matched_id} \
                     reported the value instead — the write landed somewhere unintended"
                ));
            }

            // Pixel path: a click inside the window, then a key. These assert the tools
            // accept and report; the element path above is what proves an effect.
            let click = t.call("glass_click", json!({ "x": 5, "y": 5 }))?;
            check_envelope("glass_click", &click)?;
            let key = t.call("glass_key", json!({ "chord": "Escape" }))?;
            check_envelope("glass_key", &key)?;
            Ok(format!("element #{id} took the value; pixel path ok"))
        })(),
    )
}

/// The matched element's `id`, extracted from an untrusted-wrapped sibling's JSON body (the
/// wrapper is a note line, `⟦untrusted:<nonce>⟧`, the body, then the close marker — not bare
/// JSON, so the object is located by its outermost braces rather than parsed whole). `None`
/// when there is no sibling, no `{...}` span in it, the span doesn't parse, or it carries no
/// `id` — every one of those means "cannot confirm", not a mismatch.
fn matched_element_id(r: &CallResult) -> Option<u64> {
    let sibling = untrusted_sibling(r).ok()?;
    let start = sibling.find('{')?;
    let end = sibling.rfind('}')?;
    serde_json::from_str::<Value>(&sibling[start..=end])
        .ok()?
        .get("id")?
        .as_u64()
}

pub fn check_logs(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        8,
        "logs",
        t.call("glass_logs", json!({})).and_then(|r| {
            check_envelope("glass_logs", &r)?;
            // An app that logged nothing is normal; app text that is NOT untrusted-wrapped
            // is a freeze violation, so only check the wrapping when there is text.
            if r.siblings.is_empty() {
                return Ok("no output".into());
            }
            untrusted_sibling(&r)?;
            Ok(format!("{} block(s), untrusted-wrapped", r.siblings.len()))
        }),
    )
}

/// Deliberate misuse must return an error that names a remedy, not only a cause.
pub fn check_error_honesty(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        9,
        "error honesty",
        t.call("glass_click_element", json!({ "id": 999_999 }))
            .and_then(|r| {
                if !r.is_error {
                    return Err("glass_click_element on a nonexistent id succeeded".into());
                }
                let msg = r.siblings.join(" ");
                if crate::smoke::envelope::names_a_remedy(&msg) {
                    Ok(format!("actionable: {msg}"))
                } else {
                    Err(format!("error names a cause but no remedy: {msg}"))
                }
            }),
    )
}

pub fn check_stop(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        10,
        "stop",
        t.call("glass_stop", json!({})).and_then(|r| {
            check_envelope("glass_stop", &r)?;
            Ok("session ended".into())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smoke::profile::X11_CANDIDATES;
    use crate::smoke::report::CheckStatus;
    use crate::smoke::transport::{CallResult, ScriptedTransport};

    fn ok(tool: &str, result: Value, siblings: Vec<&str>, images: usize) -> CallResult {
        CallResult {
            is_error: false,
            envelope: Some(json!({ "ok": true, "tool": tool, "result": result })),
            siblings: siblings.into_iter().map(String::from).collect(),
            images,
        }
    }

    fn profile() -> Profile {
        Profile {
            backend: "x11".into(),
            app: &X11_CANDIDATES[3],
        }
    }

    #[test]
    fn version_must_match_the_expected_tag() {
        let mut t = ScriptedTransport::new(vec![]);
        // ScriptedTransport reports "0.0.0-scripted".
        assert_eq!(
            check_version(&mut t, "0.0.0-scripted").status,
            CheckStatus::Pass
        );
        let mut t = ScriptedTransport::new(vec![]);
        let out = check_version(&mut t, "1.1.0");
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("1.1.0"),
            "must name both versions: {}",
            out.detail
        );
    }

    #[test]
    fn start_requires_geometry_in_the_envelope() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Ok(ok(
                "glass_start",
                json!({ "x": 0, "y": 0, "width": 800, "height": 600 }),
                vec![],
                0,
            )),
        )]);
        assert_eq!(check_start(&mut t, &profile()).status, CheckStatus::Pass);

        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Ok(ok("glass_start", json!({ "x": 0, "y": 0 }), vec![], 0)),
        )]);
        let out = check_start(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("width"),
            "must say what was missing: {}",
            out.detail
        );
    }

    #[test]
    fn screenshot_requires_both_an_image_and_dimensions() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_screenshot",
            Ok(ok(
                "glass_screenshot",
                json!({ "width": 800, "height": 600 }),
                vec![],
                1,
            )),
        )]);
        assert_eq!(check_screenshot(&mut t).status, CheckStatus::Pass);

        let mut t = ScriptedTransport::new(vec![(
            "glass_screenshot",
            Ok(ok(
                "glass_screenshot",
                json!({ "width": 800, "height": 600 }),
                vec![],
                0,
            )),
        )]);
        let out = check_screenshot(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("image"), "got: {}", out.detail);
    }

    #[test]
    fn a11y_requires_a_nonempty_tree_in_an_untrusted_sibling() {
        let outline = "⟦untrusted:abc⟧\n#1 Window \"Untitled\"\n  #2 TextBox \"Body\" [editable]\n⟦/untrusted:abc⟧";
        let mut t = ScriptedTransport::new(vec![(
            "glass_a11y_snapshot",
            Ok(ok("glass_a11y_snapshot", json!({}), vec![outline], 0)),
        )]);
        let (out, nodes) = check_a11y(&mut t);
        assert_eq!(out.status, CheckStatus::Pass);
        assert_eq!(nodes.len(), 2);

        // Unwrapped app text is a freeze violation, not a pass.
        let mut t = ScriptedTransport::new(vec![(
            "glass_a11y_snapshot",
            Ok(ok("glass_a11y_snapshot", json!({}), vec!["#1 Window"], 0)),
        )]);
        let (out, _) = check_a11y(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
    }

    /// A `glass_doctor` result mirroring the server's real payload shape
    /// (`{report, sections, overall}`), not the `{checks}` shape a stale mock once
    /// fabricated and hid this bug behind.
    fn doctor_result(overall: &str, sections: Value) -> Value {
        json!({ "report": "glass doctor\n...", "overall": overall, "sections": sections })
    }

    #[test]
    fn health_fails_when_doctor_reports_fail() {
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_capabilities",
                Ok(ok(
                    "glass_capabilities",
                    json!({ "backend": "x11" }),
                    vec![],
                    0,
                )),
            ),
            (
                "glass_doctor",
                Ok(ok(
                    "glass_doctor",
                    doctor_result(
                        "fail",
                        json!([{
                            "title": "x11",
                            "backend": "x11",
                            "checks": [{ "name": "display", "status": "fail", "detail": "not found" }],
                        }]),
                    ),
                    vec![],
                    0,
                )),
            ),
        ]);
        let out = check_health(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("display"),
            "must name the failing check: {}",
            out.detail
        );
    }

    #[test]
    fn health_fails_on_overall_fail_even_with_no_check_individually_reporting_it() {
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_capabilities",
                Ok(ok(
                    "glass_capabilities",
                    json!({ "backend": "x11" }),
                    vec![],
                    0,
                )),
            ),
            (
                "glass_doctor",
                Ok(ok(
                    "glass_doctor",
                    doctor_result("fail", json!([])),
                    vec![],
                    0,
                )),
            ),
        ]);
        let out = check_health(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            !out.detail.is_empty(),
            "an unattributed overall fail must still say something, not go silent"
        );
    }

    #[test]
    fn health_passes_when_doctor_reports_ok() {
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_capabilities",
                Ok(ok(
                    "glass_capabilities",
                    json!({ "backend": "x11" }),
                    vec![],
                    0,
                )),
            ),
            (
                "glass_doctor",
                Ok(ok(
                    "glass_doctor",
                    doctor_result(
                        "ok",
                        json!([{
                            "title": "x11",
                            "backend": "x11",
                            "checks": [{ "name": "display", "status": "ok", "detail": "found" }],
                        }]),
                    ),
                    vec![],
                    0,
                )),
            ),
        ]);
        let out = check_health(&mut t);
        assert_eq!(out.status, CheckStatus::Pass);
        assert_eq!(
            out.detail, "backend x11",
            "must render the plain backend string, not a JSON-quoted one: {}",
            out.detail
        );
    }

    #[test]
    fn a_transport_error_is_a_failure_not_a_panic() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Err("broken pipe".into()))]);
        let out = check_start(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("broken pipe"));
    }

    fn nodes_with_editable() -> Vec<OutlineNode> {
        vec![OutlineNode {
            id: 12,
            role: "TextBox".into(),
            name: None,
            states: vec!["editable".into()],
        }]
    }

    /// A `glass_wait_for_element` match, wrapped exactly as the real tool wraps it: a note
    /// line, the nonce-delimited markers, then the element JSON — not bare JSON. Building it
    /// through the real wrapper (rather than a hand-rolled marker string) is what makes the
    /// extraction in `matched_element_id` actually exercised by these tests.
    fn matched_element(id: u32) -> String {
        let body = json!({
            "id": id,
            "role": "TextField",
            "name": Value::Null,
            "value": PROBE_TEXT,
            "bounds": Value::Null,
            "states": ["editable"],
        })
        .to_string();
        crate::untrusted::wrap_untrusted(&body)
    }

    #[test]
    fn interaction_verifies_at_the_consumer_layer_not_by_ok() {
        // set_value ok, then glass_wait_for_element — the signal glass actually exposes for a
        // written value — confirms it landed on the same element.
        let sibling = matched_element(12);
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_set_value",
                Ok(ok("glass_set_value", json!({ "id": 12 }), vec![], 0)),
            ),
            (
                "glass_wait_for_element",
                Ok(ok(
                    "glass_wait_for_element",
                    json!({ "matched": true, "elapsed_ms": 5 }),
                    vec![sibling.as_str()],
                    0,
                )),
            ),
            ("glass_click", Ok(ok("glass_click", json!({}), vec![], 0))),
            ("glass_key", Ok(ok("glass_key", json!({}), vec![], 0))),
        ]);
        assert_eq!(
            check_interaction(&mut t, &nodes_with_editable()).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn interaction_passes_when_matched_but_the_id_cannot_be_confirmed() {
        // matched:true with no untrusted sibling to read an id from: "cannot confirm" is not
        // a failure — only a definite id mismatch is.
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_set_value",
                Ok(ok("glass_set_value", json!({ "id": 12 }), vec![], 0)),
            ),
            (
                "glass_wait_for_element",
                Ok(ok(
                    "glass_wait_for_element",
                    json!({ "matched": true, "elapsed_ms": 5 }),
                    vec![],
                    0,
                )),
            ),
            ("glass_click", Ok(ok("glass_click", json!({}), vec![], 0))),
            ("glass_key", Ok(ok("glass_key", json!({}), vec![], 0))),
        ]);
        assert_eq!(
            check_interaction(&mut t, &nodes_with_editable()).status,
            CheckStatus::Pass
        );
    }

    #[test]
    fn interaction_fails_when_no_element_reports_the_written_value() {
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_set_value",
                Ok(ok("glass_set_value", json!({ "id": 12 }), vec![], 0)),
            ),
            (
                "glass_wait_for_element",
                Ok(ok(
                    "glass_wait_for_element",
                    json!({ "matched": false, "elapsed_ms": 5000 }),
                    vec![],
                    0,
                )),
            ),
        ]);
        let out = check_interaction(&mut t, &nodes_with_editable());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("no element"), "got: {}", out.detail);
    }

    #[test]
    fn interaction_fails_when_a_different_element_reports_the_value() {
        // The probe text shows up, but on #99, not the #12 we wrote to: the write landed
        // somewhere unintended, which is a real defect this check must catch.
        let sibling = matched_element(99);
        let mut t = ScriptedTransport::new(vec![
            (
                "glass_set_value",
                Ok(ok("glass_set_value", json!({ "id": 12 }), vec![], 0)),
            ),
            (
                "glass_wait_for_element",
                Ok(ok(
                    "glass_wait_for_element",
                    json!({ "matched": true, "elapsed_ms": 5 }),
                    vec![sibling.as_str()],
                    0,
                )),
            ),
        ]);
        let out = check_interaction(&mut t, &nodes_with_editable());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("12") && out.detail.contains("99"),
            "must name both ids: {}",
            out.detail
        );
    }

    #[test]
    fn interaction_skips_when_no_editable_element_exists() {
        let mut t = ScriptedTransport::new(vec![]);
        let nodes = vec![OutlineNode {
            id: 1,
            role: "Button".into(),
            name: None,
            states: vec![],
        }];
        assert_eq!(check_interaction(&mut t, &nodes).status, CheckStatus::Skip);
    }

    #[test]
    fn interaction_skips_when_the_target_has_no_addressable_role() {
        // `Other(...)` is what an unmapped role renders as in the outline; `AxRole::from_name`
        // cannot parse it back, so `glass_wait_for_element`'s `role` selector cannot target
        // it. This must skip with a reason, not fail and blame the app.
        let mut t = ScriptedTransport::new(vec![]);
        let nodes = vec![OutlineNode {
            id: 7,
            role: "Other(AXDisclosureTriangle)".into(),
            name: None,
            states: vec!["editable".into()],
        }];
        let out = check_interaction(&mut t, &nodes);
        assert_eq!(out.status, CheckStatus::Skip);
        assert!(out.detail.contains("role"), "got: {}", out.detail);
    }

    #[test]
    fn error_honesty_requires_an_actionable_message() {
        let remedyless = CallResult {
            is_error: true,
            envelope: None,
            siblings: vec!["unknown element id 999999".into()],
            images: 0,
        };
        let mut t = ScriptedTransport::new(vec![("glass_click_element", Ok(remedyless))]);
        let out = check_error_honesty(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("remedy"), "got: {}", out.detail);

        let actionable = CallResult {
            is_error: true,
            envelope: None,
            siblings: vec![
                "unknown element id 999999 — call glass_a11y_snapshot to re-read ids".into(),
            ],
            images: 0,
        };
        let mut t = ScriptedTransport::new(vec![("glass_click_element", Ok(actionable))]);
        assert_eq!(check_error_honesty(&mut t).status, CheckStatus::Pass);
    }

    #[test]
    fn error_honesty_fails_when_a_bad_call_succeeds() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_click_element",
            Ok(ok(
                "glass_click_element",
                json!({ "id": 999999 }),
                vec![],
                0,
            )),
        )]);
        let out = check_error_honesty(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("succeeded"), "got: {}", out.detail);
    }

    #[test]
    fn logs_must_arrive_untrusted_wrapped() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_logs",
            Ok(ok(
                "glass_logs",
                json!({ "count": 1 }),
                vec!["⟦untrusted:q⟧\nstarted\n⟦/untrusted:q⟧"],
                0,
            )),
        )]);
        assert_eq!(check_logs(&mut t).status, CheckStatus::Pass);
    }

    #[test]
    fn stop_must_return_a_clean_envelope() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_stop",
            Ok(ok("glass_stop", json!({}), vec![], 0)),
        )]);
        assert_eq!(check_stop(&mut t).status, CheckStatus::Pass);
    }
}
