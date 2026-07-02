//! macOS clipboard (the general `NSPasteboard`) read/write behind the `Platform` seam.
//!
//! Text only, matching `Platform::get_clipboard`/`set_clipboard`. macOS has no clipboard
//! containment yet, so this acts on the user's **real system pasteboard** — the same
//! shared-desktop behaviour the other backends have under `GLASS_DISPLAY=:0` / the Windows
//! backend's `sandbox=off`. Isolation will arrive with future macOS containment.
//!
//! `objc2-app-kit` 0.3.2 exposes the `NSPasteboard` methods used here as safe (non-`unsafe`)
//! bindings, so this module needs no `unsafe` (mirrors `input.rs`'s `#![forbid(unsafe_code)]`).
#![forbid(unsafe_code)]

use glass_core::{GlassError, Result};
use objc2_app_kit::{NSPasteboard, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// Read the general pasteboard's plain-text value; `""` when it holds no text.
pub(crate) fn get() -> Result<String> {
    let pasteboard = NSPasteboard::generalPasteboard();
    // `stringForType` is `None` when the pasteboard carries no string of this type — that is
    // "no text" (the seam's documented empty case), not an error.
    let text = pasteboard.stringForType(NSPasteboardTypeString);
    Ok(text.map(|s| s.to_string()).unwrap_or_default())
}

/// Replace the general pasteboard's contents with `text` (plain text). Errors — never a silent
/// no-op — if the pasteboard refuses the write.
pub(crate) fn set(text: &str) -> Result<()> {
    let pasteboard = NSPasteboard::generalPasteboard();
    // `clearContents` takes pasteboard ownership for this process and must precede a write; it
    // returns the new change count, which we don't need.
    let _ = pasteboard.clearContents();
    let value = NSString::from_str(text);
    // `setString:forType:` returns NO on failure — surface it (glass "no silent fallbacks").
    if pasteboard.setString_forType(&value, NSPasteboardTypeString) {
        Ok(())
    } else {
        Err(GlassError::Backend(
            "NSPasteboard rejected the clipboard write (setString returned false)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{get, set};

    // These exercise the REAL system pasteboard (macOS has no containment yet). Each test
    // snapshots the pre-existing contents and restores them BEFORE asserting, so a failing
    // assert can never leave the probe text behind on a machine's clipboard — keeping the
    // default `cargo test -p glass-macos --lib` (scripts/test-macos.sh) non-contaminating.

    #[test]
    fn clipboard_roundtrips_text() {
        let saved = get().unwrap_or_default();
        set("glass-clip-probe-\u{1F9EA}").expect("set clipboard");
        let read = get().expect("get clipboard");
        let _ = set(&saved); // restore before asserting
        assert_eq!(read, "glass-clip-probe-\u{1F9EA}");
    }

    #[test]
    fn clipboard_get_is_empty_when_no_text() {
        let saved = get().unwrap_or_default();
        set("").expect("set empty clipboard");
        let read = get().expect("get clipboard");
        let _ = set(&saved); // restore before asserting
        assert_eq!(read, "");
    }
}
