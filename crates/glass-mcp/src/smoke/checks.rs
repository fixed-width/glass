//! The release checks. Each takes the transport seam and returns one outcome, so
//! every one is unit-testable against a scripted transport — no display required.

use crate::smoke::envelope::{check_envelope, untrusted_sibling};
use crate::smoke::profile::{OutlineNode, Profile, parse_outline};
use crate::smoke::report::CheckOutcome;
use crate::smoke::transport::McpTransport;
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
}
