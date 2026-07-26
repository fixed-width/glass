//! The frozen-surface assertions: every tool result is `{ok,tool,result}`, and
//! app-controlled text arrives as an untrusted sibling block — never inside `result`.

use crate::smoke::transport::CallResult;
use serde_json::Value;

/// Opening marker of the untrusted envelope glass wraps app-derived text in.
const UNTRUSTED_MARKER: &str = "⟦untrusted:";

/// Assert the frozen `{ok,tool,result}` envelope and return the inner `result`.
pub fn check_envelope(tool: &str, r: &CallResult) -> Result<Value, String> {
    // `CallResult::from_mcp` leaves `envelope: None` on `isError` — the message is not an
    // envelope and must never be parsed as one — so without this arm an ordinary tool failure
    // fell through to "no envelope in the first content block", reporting the frozen protocol
    // surface as broken while the server's own explanation sat unread in `siblings`.
    if r.is_error {
        let msg = r.siblings.join(" ");
        return Err(if msg.trim().is_empty() {
            // Silence would be worse than an odd message: the call really did fail.
            format!("{tool} returned an error with no message")
        } else {
            format!("{tool} returned an error: {msg}")
        });
    }
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
/// Beyond generic English pointer verbs, `"re-snapshot"` is recognized too: it is the
/// codebase-wide idiom for "take a fresh accessibility snapshot before retrying", used by
/// `GlassError::AxElementNotFound` and `AxElementChanged`.
///
/// A message that names one of glass's own registered tools is also a pointer, however the
/// sentence is phrased (`AxElementNotEditable` names three, with no generic pointer verb).
/// That check matches against [`crate::server::registered_tools`] — the live `#[tool]`
/// registry — rather than a bare `"glass_"` substring: app- or backend-controlled text can
/// carry that prefix in a path or filename with no tool in sight (the Android reader's
/// `/sdcard/glass_dump.xml`, the Windows clipboard shim's `glass_clip_hook.dll` — see the
/// regression tests below), so resist simplifying this back to `contains("glass_")`.
/// Every pointer is matched as a run of WHOLE words, never as a raw substring: `"use"` sits
/// inside `because`, `"try"` inside `geometry` and `registry`, `"set"` inside `offset`, so a
/// substring match scored a cause-only message like "at-spi registry not running" actionable.
pub fn names_a_remedy(msg: &str) -> bool {
    const POINTERS: [&str; 9] = [
        "call",
        "pass",
        "use",
        "re-run",
        "relaunch",
        "retry",
        "set",
        "try",
        "re-snapshot",
    ];
    let words = words(msg);
    let tools = crate::server::registered_tools();
    POINTERS
        .iter()
        .copied()
        .chain(tools.iter().map(String::as_str))
        .any(|pointer| contains_phrase(&words, pointer))
}

/// Alphanumeric word runs: `-` and `_` separate, so `re-snapshot` and `glass_click_element`
/// each tokenize into their parts and are matched as consecutive words by
/// [`contains_phrase`]. Same tokenizer as the doc-lint in `server.rs`
/// (`names_windows_backend`).
fn words(text: &str) -> Vec<&str> {
    text.split(|c: char| !c.is_alphanumeric())
        .filter(|w| !w.is_empty())
        .collect()
}

/// Do `phrase`'s words appear as a consecutive run in `words`, case-insensitively?
fn contains_phrase(words: &[&str], phrase: &str) -> bool {
    let needle: Vec<&str> = self::words(phrase);
    if needle.is_empty() {
        return false;
    }
    words.windows(needle.len()).any(|run| {
        run.iter()
            .zip(&needle)
            .all(|(w, n)| w.eq_ignore_ascii_case(n))
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok_call(tool: &str, result: Value, siblings: &[&str]) -> CallResult {
        CallResult::ok(tool, result, siblings, 0)
    }

    #[test]
    fn a_well_formed_envelope_yields_its_result() {
        let c = ok_call("glass_start", json!({ "width": 800 }), &[]);
        let result = check_envelope("glass_start", &c).unwrap();
        assert_eq!(result["width"], json!(800));
    }

    /// A missing envelope on a *successful* result is a genuine freeze violation: this pins
    /// that the `is_error` arm in front of it did not swallow the case it was added beside.
    #[test]
    fn a_missing_envelope_on_a_successful_result_is_still_rejected() {
        let c = CallResult {
            is_error: false,
            envelope: None,
            siblings: vec![],
            images: 0,
        };
        let err = check_envelope("glass_start", &c).unwrap_err();
        assert_eq!(
            err,
            "glass_start: no `{ok,tool,result}` envelope in the first content block"
        );
    }

    /// The defect this arm exists for: an ordinary tool failure reported the frozen protocol
    /// surface as broken while the server's own explanation sat unread in `siblings`. Built
    /// from the live `GlassError`, so it tracks glass-core's actual wording.
    #[test]
    fn a_tool_error_carries_the_servers_own_message() {
        let msg =
            glass_core::GlassError::AccessibilityUnavailable("no a11y bus".into()).to_string();
        let c = CallResult {
            is_error: true,
            envelope: None,
            siblings: vec![msg.clone()],
            images: 0,
        };
        let err = check_envelope("glass_a11y_snapshot", &c).unwrap_err();
        assert!(
            err.contains(&msg),
            "the server's explanation must reach the report: {err}"
        );
        assert!(
            !err.contains("no `{ok,tool,result}` envelope"),
            "a tool error must not be reported as a broken protocol surface: {err}"
        );
    }

    /// An `isError` with nothing in it would otherwise render as a sentence that stops at a
    /// colon. The call still failed, so it has to say something.
    #[test]
    fn a_tool_error_with_no_message_still_says_the_call_failed() {
        let c = CallResult {
            is_error: true,
            envelope: None,
            siblings: vec![],
            images: 0,
        };
        let err = check_envelope("glass_logs", &c).unwrap_err();
        assert_eq!(err, "glass_logs returned an error with no message");
    }

    #[test]
    fn a_mismatched_tool_name_is_rejected() {
        let c = ok_call("glass_stop", json!({}), &[]);
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
        // App-controlled text is a sibling block, never inside `result`.
        let c = ok_call(
            "glass_a11y_snapshot",
            json!({ "outline": "⟦untrusted:abc⟧ #1 Button ⟦/untrusted:abc⟧" }),
            &[],
        );
        let err = check_envelope("glass_a11y_snapshot", &c).unwrap_err();
        assert!(err.contains("result"), "must say where it found it: {err}");
    }

    #[test]
    fn untrusted_sibling_requires_the_marker() {
        let with = ok_call(
            "glass_a11y_snapshot",
            json!({}),
            &["⟦untrusted:abc⟧ body ⟦/untrusted:abc⟧"],
        );
        assert!(untrusted_sibling(&with).is_ok());
        let without = ok_call("glass_a11y_snapshot", json!({}), &["plain text"]);
        assert!(untrusted_sibling(&without).is_err());
    }

    #[test]
    fn a_remedyless_message_is_not_actionable() {
        assert!(!names_a_remedy("no clickable on-screen geometry"));
        assert!(names_a_remedy("no active session — call glass_start first"));
        assert!(names_a_remedy("not a boolean; pass \"true\" or \"false\""));
    }

    /// `"try "` sits inside `geometry` and `registry`, `"use "` inside `because`, `"set "`
    /// inside `offset` — every one of these is a cause-only message that a substring match
    /// scored actionable, and that the canonical case above happened not to catch.
    #[test]
    fn a_pointer_verb_hiding_inside_a_longer_word_is_not_a_remedy() {
        for msg in [
            "at-spi registry not running",
            "the element has no on-screen geometry to click",
            "the snapshot is stale because the window changed",
            "the frame arrived at a negative offset",
            "the entry is not focusable",
        ] {
            assert!(!names_a_remedy(msg), "should not be actionable: {msg}");
        }
    }

    #[test]
    fn a_pointer_verb_as_a_whole_word_is_still_a_remedy() {
        for msg in [
            "unknown mode; use \"exact\" or \"perceptual\"",
            "Try a shorter timeout",
            "set GLASS_BACKEND to a supported backend",
        ] {
            assert!(names_a_remedy(msg), "should be actionable: {msg}");
        }
    }

    /// Regression for the x11 end-to-end run (`tests/smoke_x11.rs`): `glass_click_element`
    /// on a stale id returned this message, and `POINTERS` didn't recognize its "re-snapshot"
    /// remedy clause — a false negative only a run against real glass could surface.
    /// Constructed from the live enum, so it tracks `glass-core`'s actual wording.
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

    /// This variant names three tools without using any generic pointer verb, so it only
    /// passes via the registered-tool-name signal, not `POINTERS`.
    #[test]
    fn recognizes_ax_element_not_editable_named_tools() {
        let msg = glass_core::GlassError::AxElementNotEditable(0).to_string();
        assert!(
            names_a_remedy(&msg),
            "should recognize the named glass_ tools: {msg}"
        );
    }

    /// The false positive a bare `contains("glass_")` check would produce: the Android a11y
    /// reader forwards raw `uiautomator` stdout verbatim into
    /// `GlassError::AccessibilityUnavailable`, and the dump file it polls is
    /// `/sdcard/glass_dump.xml` — so a real device failure can name that path while stating
    /// only a cause.
    #[test]
    fn android_dump_path_is_not_mistaken_for_a_tool_name() {
        let msg = "uiautomator dump failed: /sdcard/glass_dump.xml: Permission denied";
        assert!(!names_a_remedy(msg), "should not be actionable: {msg}");
    }

    /// Same false-positive shape on Windows: the injected clipboard-shim DLL is
    /// `glass_clip_hook.dll`, so a load failure naming that file must still read as
    /// cause-only.
    #[test]
    fn windows_clip_hook_dll_path_is_not_mistaken_for_a_tool_name() {
        let msg = "failed to load glass_clip_hook.dll: The specified module could not be found.";
        assert!(!names_a_remedy(msg), "should not be actionable: {msg}");
    }
}
