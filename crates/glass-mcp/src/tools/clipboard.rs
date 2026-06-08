//! Clipboard get/set tools.

use glass_core::Glass;

use crate::params::ClipboardSetArgs;
use crate::tools::{ToolOutput, ToolResult};

pub fn clipboard_get(glass: &mut Glass) -> ToolResult {
    let text = glass.get_clipboard().map_err(|e| e.to_string())?;
    Ok(ToolOutput::text(crate::untrusted::wrap_untrusted(&text)))
}

pub fn clipboard_set(glass: &mut Glass, a: &ClipboardSetArgs) -> ToolResult {
    glass.set_clipboard(&a.text).map_err(|e| e.to_string())?;
    Ok(ToolOutput::text("ok"))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tools::testutil::*;
    use crate::tools::{start as start_tool, OutContent};
    use crate::params::{ClipboardSetArgs, StartArgs};

    fn started() -> Glass {
        let mut g = glass_with(FakePlatform::new(100, 100));
        let a = StartArgs {
            build: None,
            run: vec!["app".into()],
            backend: None,
            sandbox: None,
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: None,
        };
        start_tool(&mut g, &a).unwrap();
        g
    }

    fn text(out: &ToolOutput) -> &str {
        match &out.0[0] {
            OutContent::Text(t) => t,
            _ => panic!("expected text"),
        }
    }

    #[test]
    fn clipboard_set_then_get_roundtrips() {
        let mut g = started();
        // clipboard_set returns glass's own "ok" — NOT wrapped.
        assert_eq!(text(&clipboard_set(&mut g, &ClipboardSetArgs { text: "foo".into() }).unwrap()), "ok");
        // clipboard_get returns app-derived text — MUST be wrapped.
        let out = clipboard_get(&mut g).unwrap();
        let got = text(&out);
        assert!(got.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {got}");
        assert!(got.contains("⟦untrusted:") && got.contains("⟦/untrusted:"), "enveloped: {got}");
        assert!(got.contains("foo"), "body intact: {got}");
    }

    #[test]
    fn clipboard_get_empty_is_blank() {
        let mut g = started();
        let out = clipboard_get(&mut g).unwrap();
        let got = text(&out);
        assert!(got.starts_with(crate::untrusted::NOTE), "must be marked untrusted: {got}");
        assert!(got.contains("⟦untrusted:") && got.contains("⟦/untrusted:"), "enveloped: {got}");
        // empty body -> the envelope is present but body section is blank
        let after_open = got.split("⟧\n").nth(1).unwrap_or("");
        let body = after_open.rsplit_once('\n').map(|(b, _)| b).unwrap_or(after_open);
        assert_eq!(body, "", "body must be empty for empty clipboard: {got}");
    }
}
