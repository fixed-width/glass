//! The frozen-surface assertions: every tool result is `{ok,tool,result}`, and
//! app-controlled text arrives as an untrusted sibling block — never inside `result`.

use crate::smoke::transport::CallResult;
use serde_json::Value;

/// Opening marker of the untrusted envelope glass wraps app-derived text in.
const UNTRUSTED_MARKER: &str = "⟦untrusted:";

/// Assert the frozen `{ok,tool,result}` envelope and return the inner `result`.
pub fn check_envelope(tool: &str, r: &CallResult) -> Result<Value, String> {
    let env = r.envelope.as_ref().ok_or_else(|| {
        format!("{tool}: no `{{ok,tool,result}}` envelope in the first content block")
    })?;
    if env["ok"] != Value::Bool(true) {
        return Err(format!(
            "{tool}: envelope `ok` is {}, expected true",
            env["ok"]
        ));
    }
    let got = env["tool"].as_str().unwrap_or("<missing>");
    if got != tool {
        return Err(format!(
            "{tool}: envelope names tool {got:?}, expected {tool:?}"
        ));
    }
    let result = env
        .get("result")
        .cloned()
        .ok_or_else(|| format!("{tool}: envelope has no `result`"))?;
    if result.to_string().contains(UNTRUSTED_MARKER) {
        return Err(format!(
            "{tool}: untrusted app text found inside `result`; it must be a sibling content block"
        ));
    }
    Ok(result)
}

/// The first sibling block carrying the untrusted envelope.
pub fn untrusted_sibling(r: &CallResult) -> Result<&str, String> {
    r.siblings
        .iter()
        .map(String::as_str)
        .find(|s| s.contains(UNTRUSTED_MARKER))
        .ok_or_else(|| "no untrusted-wrapped sibling block; app text must be marked".to_string())
}

/// Heuristic: does this message point the caller somewhere, or only name a cause?
///
/// It cannot judge prose — it checks for an imperative pointer (a tool name to call,
/// a value to pass, an action to take). A message that states only a cause
/// ("no clickable on-screen geometry") fails, which is the regression this guards.
pub fn names_a_remedy(msg: &str) -> bool {
    const POINTERS: [&str; 8] = [
        "call ", "pass ", "use ", "re-run", "relaunch", "retry", "set ", "try ",
    ];
    let lower = msg.to_lowercase();
    POINTERS.iter().any(|p| lower.contains(p))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_call(tool: &str, result: Value, siblings: Vec<&str>) -> CallResult {
        CallResult {
            is_error: false,
            envelope: Some(json!({ "ok": true, "tool": tool, "result": result })),
            siblings: siblings.into_iter().map(String::from).collect(),
            images: 0,
        }
    }

    #[test]
    fn a_well_formed_envelope_yields_its_result() {
        let c = ok_call("glass_start", json!({ "width": 800 }), vec![]);
        let result = check_envelope("glass_start", &c).unwrap();
        assert_eq!(result["width"], json!(800));
    }

    #[test]
    fn a_missing_envelope_is_rejected() {
        let c = CallResult {
            is_error: false,
            envelope: None,
            siblings: vec![],
            images: 0,
        };
        let err = check_envelope("glass_start", &c).unwrap_err();
        assert!(err.contains("envelope"), "got: {err}");
    }

    #[test]
    fn a_mismatched_tool_name_is_rejected() {
        let c = ok_call("glass_stop", json!({}), vec![]);
        let err = check_envelope("glass_start", &c).unwrap_err();
        assert!(
            err.contains("glass_start") && err.contains("glass_stop"),
            "got: {err}"
        );
    }

    #[test]
    fn ok_false_is_rejected() {
        let c = CallResult {
            is_error: false,
            envelope: Some(json!({ "ok": false, "tool": "glass_start", "result": {} })),
            siblings: vec![],
            images: 0,
        };
        assert!(check_envelope("glass_start", &c).is_err());
    }

    #[test]
    fn an_envelope_without_a_result_is_rejected() {
        let c = CallResult {
            is_error: false,
            envelope: Some(json!({ "ok": true, "tool": "glass_start" })),
            siblings: vec![],
            images: 0,
        };
        let err = check_envelope("glass_start", &c).unwrap_err();
        assert!(err.contains("result"), "got: {err}");
    }

    #[test]
    fn untrusted_app_text_inside_result_is_rejected() {
        // The A1 rule: app-controlled text is a sibling block, never inside `result`.
        let c = ok_call(
            "glass_a11y_snapshot",
            json!({ "outline": "⟦untrusted:abc⟧ #1 Button ⟦/untrusted:abc⟧" }),
            vec![],
        );
        let err = check_envelope("glass_a11y_snapshot", &c).unwrap_err();
        assert!(err.contains("result"), "must say where it found it: {err}");
    }

    #[test]
    fn untrusted_sibling_requires_the_marker() {
        let with = ok_call(
            "glass_a11y_snapshot",
            json!({}),
            vec!["⟦untrusted:abc⟧ body ⟦/untrusted:abc⟧"],
        );
        assert!(untrusted_sibling(&with).is_ok());
        let without = ok_call("glass_a11y_snapshot", json!({}), vec!["plain text"]);
        assert!(untrusted_sibling(&without).is_err());
    }

    #[test]
    fn a_remedyless_message_is_not_actionable() {
        assert!(!names_a_remedy("no clickable on-screen geometry"));
        assert!(names_a_remedy("no active session — call glass_start first"));
        assert!(names_a_remedy("not a boolean; pass \"true\" or \"false\""));
    }
}
