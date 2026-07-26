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
///
/// Beyond generic English pointer verbs, `"re-snapshot"` is also recognized: it's the
/// codebase-wide idiom for "take a fresh accessibility snapshot before retrying" (used
/// identically by `GlassError::AxElementNotFound` and `AxElementChanged`) — added after the
/// first real run against `glass-core`'s actual error text (this file's earlier test cases
/// were all hand-written, never checked against it).
///
/// A message that names one of glass's own registered tools is also inherently a pointer,
/// however the sentence is phrased (`AxElementNotEditable` names three: "focus it with
/// glass_click, then enter text with glass_type / glass_key instead" — no generic pointer
/// verb, but plainly actionable). That check matches against [`crate::server::registered_tools`]
/// — the live `#[tool]` registry — rather than a bare `"glass_"` substring, deliberately:
/// app-controlled or backend-controlled text can contain that prefix in a path or filename
/// with no tool in sight (e.g. the Android reader forwarding raw `uiautomator` stdout that
/// happens to mention its own dump path, `/sdcard/glass_dump.xml`, or the Windows clipboard
/// shim's `glass_clip_hook.dll`) — see the regression tests below. Matching the exact
/// registered names avoids that false positive and stays correct as tools are added or
/// renamed, so resist simplifying this back to `contains("glass_")`.
pub fn names_a_remedy(msg: &str) -> bool {
    const POINTERS: [&str; 9] = [
        "call ",
        "pass ",
        "use ",
        "re-run",
        "relaunch",
        "retry",
        "set ",
        "try ",
        "re-snapshot",
    ];
    let lower = msg.to_lowercase();
    let tools = crate::server::registered_tools();
    POINTERS
        .iter()
        .copied()
        .chain(tools.iter().map(String::as_str))
        .any(|pointer| lower.contains(pointer))
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

    /// Regression for the x11 end-to-end run (`tests/smoke_x11.rs`): `glass_click_element`
    /// on a stale id returned this exact message, and the pre-existing `POINTERS` list
    /// didn't recognize its "re-snapshot" remedy clause — a false negative caught only
    /// once the harness ran against real glass, not a scripted transport. Constructed from
    /// the live enum (not a copied string) so this tracks `glass-core`'s actual wording
    /// rather than a frozen snapshot of it.
    #[test]
    fn recognizes_ax_element_not_found_re_snapshot_wording() {
        let msg = glass_core::GlassError::AxElementNotFound(999_999).to_string();
        assert!(names_a_remedy(&msg), "should recognize re-snapshot: {msg}");
    }

    /// Same "re-snapshot" idiom, the sibling variant for a stale (rather than missing) id.
    #[test]
    fn recognizes_ax_element_changed_re_snapshot_wording() {
        let msg = glass_core::GlassError::AxElementChanged(2).to_string();
        assert!(names_a_remedy(&msg), "should recognize re-snapshot: {msg}");
    }

    /// This variant names three tools ("focus it with glass_click, then enter text with
    /// glass_type / glass_key instead") without using any generic pointer verb, so it only
    /// passes via the registered-tool-name signal, not `POINTERS`.
    #[test]
    fn recognizes_ax_element_not_editable_named_tools() {
        let msg = glass_core::GlassError::AxElementNotEditable(0).to_string();
        assert!(
            names_a_remedy(&msg),
            "should recognize the named glass_ tools: {msg}"
        );
    }

    /// Regression for the false positive a bare `contains("glass_")` check would produce:
    /// the Android a11y reader forwards raw `uiautomator` stdout verbatim into
    /// `GlassError::AccessibilityUnavailable` (`glass-android/src/axmap.rs`'s
    /// `check_dump_status`), and the dump file it polls is `/sdcard/glass_dump.xml`
    /// (`glass-android/src/a11y.rs`'s `DUMP_PATH`) — so a real device failure can read
    /// "uiautomator dump failed: /sdcard/glass_dump.xml: Permission denied", stating only a
    /// cause, naming no tool at all.
    #[test]
    fn android_dump_path_is_not_mistaken_for_a_tool_name() {
        let msg = "uiautomator dump failed: /sdcard/glass_dump.xml: Permission denied";
        assert!(!names_a_remedy(msg), "should not be actionable: {msg}");
    }

    /// Same false-positive shape on Windows: the injected clipboard-shim DLL is
    /// `glass_clip_hook.dll` (`glass-windows/src/containment/config.rs`'s `hook_dll_path`),
    /// so a load failure naming that file, and nothing else, must still read as cause-only.
    #[test]
    fn windows_clip_hook_dll_path_is_not_mistaken_for_a_tool_name() {
        let msg = "failed to load glass_clip_hook.dll: The specified module could not be found.";
        assert!(!names_a_remedy(msg), "should not be actionable: {msg}");
    }
}
