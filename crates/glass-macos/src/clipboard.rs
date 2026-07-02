//! macOS clipboard (the general `NSPasteboard`) read/write behind the `Platform` seam.
//!
//! Text only, matching `Platform::get_clipboard`/`set_clipboard`. macOS has no clipboard
//! containment yet, so this acts on the user's **real system pasteboard** — the same
//! shared-desktop behaviour the other backends have under `GLASS_DISPLAY=:0` / the Windows
//! backend's `sandbox=off`. Isolation will arrive with future macOS containment.
//!
//! The only `unsafe` here is reading AppKit's `NSPasteboardTypeString` extern static (Rust
//! requires `unsafe` to read any extern static — see the `// SAFETY:` note); the `NSPasteboard`
//! methods themselves are safe objc2 bindings. Mirrors the `kCFBooleanTrue` idiom in
//! `axwindow.rs`/`session.rs`.

use glass_core::{GlassError, Result};
use objc2_app_kit::{NSPasteboard, NSPasteboardType, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// The plain-text pasteboard type constant.
fn text_type() -> &'static NSPasteboardType {
    // SAFETY: `NSPasteboardTypeString` is a framework-owned constant string, live for the
    // process's lifetime; reading the extern static is a plain global read (same idiom as
    // `axwindow.rs`/`session.rs`'s `kCFBooleanTrue`). The `unsafe` is solely Rust's blanket
    // extern-static rule, not a runtime precondition.
    unsafe { NSPasteboardTypeString }
}

/// Read the general pasteboard's plain-text value; `""` when it holds no text.
pub(crate) fn get() -> Result<String> {
    read(&NSPasteboard::generalPasteboard())
}

/// Replace the general pasteboard's contents with `text` (plain text). Errors — never a silent
/// no-op — if the pasteboard refuses the write.
pub(crate) fn set(text: &str) -> Result<()> {
    write(&NSPasteboard::generalPasteboard(), text)
}

/// Read `pb`'s plain-text value; `""` when it holds no string of that type. Split from [`get`]
/// so tests can drive a private scratch pasteboard instead of the shared system one.
fn read(pb: &NSPasteboard) -> Result<String> {
    // `stringForType` is `None` when the pasteboard carries no string of this type — that is
    // "no text" (the seam's documented empty case), not an error.
    let text = pb.stringForType(text_type());
    Ok(text.map(|s| s.to_string()).unwrap_or_default())
}

/// Replace `pb`'s contents with `text`; error if the write is refused. Split from [`set`] so
/// tests can drive a private scratch pasteboard.
fn write(pb: &NSPasteboard, text: &str) -> Result<()> {
    // `clearContents` takes pasteboard ownership for this process and must precede a write; it
    // returns the new change count, which we don't need.
    let _ = pb.clearContents();
    let value = NSString::from_str(text);
    // `setString:forType:` returns NO on failure — surface it (glass "no silent fallbacks").
    if pb.setString_forType(&value, text_type()) {
        Ok(())
    } else {
        Err(GlassError::Backend(
            "NSPasteboard rejected the clipboard write (setString returned false)".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::{read, write};
    use objc2::rc::Retained;
    use objc2_app_kit::NSPasteboard;

    // Drive a PRIVATE scratch pasteboard (`pasteboardWithUniqueName`), never the shared system
    // one, so the suite never reads — let alone clears — a developer's real clipboard.
    // `clearContents` wipes ALL items (images/files/RTF), which a text-only save/restore could
    // not protect, so isolation is the correct fix, not restoration. (Scratch pasteboards are
    // released at process exit; a short test process needn't free them explicitly.)
    fn scratch() -> Retained<NSPasteboard> {
        NSPasteboard::pasteboardWithUniqueName()
    }

    #[test]
    fn write_then_read_roundtrips_text() {
        let pb = scratch();
        write(&pb, "glass-clip-probe-\u{1F9EA}").expect("write");
        assert_eq!(read(&pb).expect("read"), "glass-clip-probe-\u{1F9EA}");
    }

    #[test]
    fn read_is_empty_when_pasteboard_has_no_text() {
        // A fresh unique pasteboard holds no string type → `stringForType` is `None` → "".
        let pb = scratch();
        assert_eq!(read(&pb).expect("read"), "");
    }
}
