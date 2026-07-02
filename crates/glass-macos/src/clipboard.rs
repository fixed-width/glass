//! macOS clipboard (`NSPasteboard`) read/write behind the `Platform` seam.
//!
//! Text only, matching `Platform::get_clipboard`/`set_clipboard`. Two pasteboards, chosen by
//! `MacosPlatform`'s `clipboard_route` (`crate::clipboard_route::ClipboardRoute`):
//!
//! - **`RealGeneral`** (`sandbox: off`): [`get`]/[`set`] act on the user's **real system
//!   pasteboard** — the same shared-desktop behaviour the other backends have under
//!   `GLASS_DISPLAY=:0` / the Windows backend's `sandbox=off`.
//! - **`Private(name)`** (contained + injectable + the clip shim confirmed): [`get_named`]/
//!   [`set_named`] act on the private named pasteboard the shim redirected the contained
//!   app's `NSPasteboard.generalPasteboard` to — the real general pasteboard is never
//!   touched. [`shim_present`] confirms the shim's sentinel item landed there before
//!   `start_app` trusts this route.
//!
//! `Unsupported` (contained, non-injectable/unconfirmed) never reaches this module at all —
//! `MacosPlatform::get_clipboard`/`set_clipboard` short-circuit to `GlassError::Unsupported`
//! first, fail-closed with no pasteboard bridge.
//!
//! The only `unsafe` here is reading AppKit's `NSPasteboardTypeString` extern static (Rust
//! requires `unsafe` to read any extern static — see the `// SAFETY:` note); the `NSPasteboard`
//! methods themselves are safe objc2 bindings. Mirrors the `kCFBooleanTrue` idiom in
//! `axwindow.rs`/`session.rs`.

use glass_core::{GlassError, Result};
use objc2_app_kit::{NSPasteboard, NSPasteboardType, NSPasteboardTypeString};
use objc2_foundation::NSString;

/// The clip shim's sentinel pasteboard-item type (`glass-clip-shim-macos`'s
/// `SENTINEL_TYPE`) — written to the private named pasteboard once the shim's swizzle is
/// live, so [`shim_present`] can confirm injection actually took rather than trusting
/// `injectable` (codesign-derived) alone.
const SHIM_SENTINEL_TYPE: &str = "tech.fixedwidth.glass.clip-shim";

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

/// Read a named pasteboard's plain-text value; `""` when it holds no text. Used for the
/// `ClipboardRoute::Private(name)` route — `name` is the per-session private pasteboard the
/// clip shim redirected the contained app to.
pub(crate) fn get_named(name: &str) -> Result<String> {
    read(&NSPasteboard::pasteboardWithName(&NSString::from_str(name)))
}

/// Replace a named pasteboard's contents with `text` (plain text). Same routing as
/// [`get_named`].
pub(crate) fn set_named(name: &str, text: &str) -> Result<()> {
    write(&NSPasteboard::pasteboardWithName(&NSString::from_str(name)), text)
}

/// Whether the clip shim's sentinel item ([`SHIM_SENTINEL_TYPE`]) is present on the named
/// pasteboard `name` — confirms the shim's swizzle-and-write actually took, not merely that
/// the target was injectable. `start_app` calls this to decide whether a `Private(name)`
/// route is trustworthy.
pub(crate) fn shim_present(name: &str) -> bool {
    let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(name));
    let sentinel_type = NSString::from_str(SHIM_SENTINEL_TYPE);
    pb.stringForType(&sentinel_type).is_some()
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
    use super::{get_named, read, set_named, shim_present, write, SHIM_SENTINEL_TYPE};
    use objc2::rc::Retained;
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    // Drive a PRIVATE scratch pasteboard (`pasteboardWithUniqueName`), never the shared system
    // one, so the suite never reads — let alone clears — a developer's real clipboard.
    // `clearContents` wipes ALL items (images/files/RTF), which a text-only save/restore could
    // not protect, so isolation is the correct fix, not restoration. (Scratch pasteboards are
    // released at process exit; a short test process needn't free them explicitly.)
    fn scratch() -> Retained<NSPasteboard> {
        NSPasteboard::pasteboardWithUniqueName()
    }

    /// A private *named* pasteboard for the `get_named`/`set_named`/`shim_present` tests
    /// below, scoped by this test process's pid and `tag` so parallel tests in the same
    /// process, and repeated CI runs, never collide on the same name. Unlike `scratch()`, this
    /// exercises `NSPasteboard::pasteboardWithName` — the exact lookup `get_named`/
    /// `set_named`/`shim_present` themselves perform — rather than `pasteboardWithUniqueName`.
    fn named(tag: &str) -> String {
        format!("tech.fixedwidth.glass.clip-shim-test.{}.{tag}", std::process::id())
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

    #[test]
    fn get_named_set_named_roundtrip_text() {
        let name = named("roundtrip");
        set_named(&name, "glass-named-probe-\u{1F9EA}").expect("set_named");
        assert_eq!(get_named(&name).expect("get_named"), "glass-named-probe-\u{1F9EA}");
    }

    #[test]
    fn get_named_is_empty_when_pasteboard_has_no_text() {
        let name = named("empty");
        assert_eq!(get_named(&name).expect("get_named"), "");
    }

    #[test]
    fn shim_present_is_false_without_the_sentinel() {
        let name = named("no-sentinel");
        assert!(!shim_present(&name));
    }

    #[test]
    fn shim_present_is_true_once_the_sentinel_type_is_written() {
        // Mirrors exactly what `glass-clip-shim-macos::imp::install` does on a real injected
        // launch — write the sentinel type to the named pasteboard — without depending on the
        // shim dylib itself.
        let name = named("sentinel");
        let pb = NSPasteboard::pasteboardWithName(&NSString::from_str(&name));
        let sentinel_type = NSString::from_str(SHIM_SENTINEL_TYPE);
        let value = NSString::from_str("1");
        assert!(pb.setString_forType(&value, &sentinel_type));
        assert!(shim_present(&name));
    }
}
