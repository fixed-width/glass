//! Clipboard get/set tools.

use glass_core::Glass;

use crate::params::ClipboardSetArgs;
use crate::tools::{OutContent, ToolOutput, ToolResult};

pub fn clipboard_get(glass: &mut Glass) -> ToolResult {
    let text = glass.get_clipboard().map_err(|e| e.to_string())?;
    Ok(ToolOutput::result_with(
        "glass_clipboard_get",
        serde_json::json!({}),
        vec![OutContent::Text(crate::untrusted::wrap_untrusted(&text))],
    ))
}

pub fn clipboard_set(glass: &mut Glass, a: &ClipboardSetArgs) -> ToolResult {
    glass.set_clipboard(&a.text).map_err(|e| e.to_string())?;
    Ok(ToolOutput::result(
        "glass_clipboard_set",
        serde_json::json!({}),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::params::{ClipboardSetArgs, StartArgs};
    use crate::tools::testutil::*;
    use crate::tools::{start as start_tool, OutContent};

    fn started() -> Glass {
        let mut g = glass_with(FakePlatform::new(100, 100));
        let a = StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: std::collections::BTreeMap::new(),
            window_hint: None,
            timeout_ms: None,
            a11y: None,
        };
        start_tool(&mut g, &a).unwrap();
        g
    }

    fn text_at(out: &ToolOutput, i: usize) -> &str {
        match &out.0[i] {
            OutContent::Text(t) => t,
            _ => panic!("expected text at index {i}"),
        }
    }

    /// Parse `out.0[0]` as the leading envelope and assert its `ok`/`tool` shape.
    fn envelope_value(out: &ToolOutput, tool: &str) -> serde_json::Value {
        let v: serde_json::Value =
            serde_json::from_str(text_at(out, 0)).expect("envelope must be valid JSON");
        assert_eq!(v["ok"], serde_json::json!(true), "envelope: {v}");
        assert_eq!(v["tool"], serde_json::json!(tool), "envelope: {v}");
        v
    }

    #[test]
    fn clipboard_set_then_get_roundtrips() {
        let mut g = started();
        // clipboard_set returns the uniform envelope with an empty result.
        let out = clipboard_set(&mut g, &ClipboardSetArgs { text: "foo".into() }).unwrap();
        let v = envelope_value(&out, "glass_clipboard_set");
        assert_eq!(v["result"], serde_json::json!({}), "envelope: {v}");

        // clipboard_get returns the envelope, then app-derived text as an
        // untrusted sibling block — the clipboard body is NOT trusted metadata.
        let out = clipboard_get(&mut g).unwrap();
        let v = envelope_value(&out, "glass_clipboard_get");
        assert_eq!(v["result"], serde_json::json!({}), "envelope: {v}");

        let got = text_at(&out, 1);
        assert!(
            got.starts_with(crate::untrusted::NOTE),
            "must be marked untrusted: {got}"
        );
        assert!(
            got.contains("⟦untrusted:") && got.contains("⟦/untrusted:"),
            "enveloped: {got}"
        );
        assert!(got.contains("foo"), "body intact: {got}");
    }

    #[test]
    fn clipboard_get_empty_is_blank() {
        let mut g = started();
        let out = clipboard_get(&mut g).unwrap();
        envelope_value(&out, "glass_clipboard_get");
        let got = text_at(&out, 1);
        assert!(
            got.starts_with(crate::untrusted::NOTE),
            "must be marked untrusted: {got}"
        );
        assert!(
            got.contains("⟦untrusted:") && got.contains("⟦/untrusted:"),
            "enveloped: {got}"
        );
        // empty body -> the envelope is present but body section is blank
        let after_open = got.split("⟧\n").nth(1).unwrap_or("");
        let body = after_open
            .rsplit_once('\n')
            .map(|(b, _)| b)
            .unwrap_or(after_open);
        assert_eq!(body, "", "body must be empty for empty clipboard: {got}");
    }
}
