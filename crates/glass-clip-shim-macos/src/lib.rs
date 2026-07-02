//! glass macOS clipboard shim — injected via `DYLD_INSERT_LIBRARIES` into a contained app
//! process. On load it reads `GLASS_CLIP_PASTEBOARD`; if set, it swizzles
//! `+[NSPasteboard generalPasteboard]` to return a private, named pasteboard instead of the
//! real system one, then writes a sentinel item to a dedicated `<name>.ready` pasteboard so
//! glass can confirm the injection took (a separate board so the app's own `clearContents` on
//! the content board can never wipe the sentinel). Inert if the env var is unset — a copy of
//! this shim loaded into an unrelated (or uncontained) process is a no-op.
//!
//! Isolation: the contained app's ordinary `NSPasteboard.generalPasteboard` calls are
//! transparently redirected to the private named pasteboard; glass reads/writes the same
//! name from the host side; the real general pasteboard — and anything else on the desktop
//! — is never touched.
#![cfg_attr(not(target_os = "macos"), allow(unused_crate_dependencies))]

#[cfg(target_os = "macos")]
mod imp {
    use std::sync::OnceLock;

    use objc2::rc::Retained;
    use objc2::runtime::{AnyClass, Imp, Sel};
    use objc2::{sel, ClassType};
    use objc2_app_kit::NSPasteboard;
    use objc2_foundation::NSString;

    /// The sentinel pasteboard-item type glass looks for to confirm the swizzle took.
    const SENTINEL_TYPE: &str = "tech.fixedwidth.glass.clip-shim";

    /// The private pasteboard name, read once from `GLASS_CLIP_PASTEBOARD` and cached as a
    /// plain `String` — the replacement IMP, as a bare Objective-C method implementation, has
    /// no other way to reach this process's environment each time it's invoked. Cached as a
    /// `String` (not a `Retained<NSString>`) because an `NSString` is not `Sync` and so cannot
    /// live in a `static OnceLock`; the cheap `NSString` is rebuilt at each use site.
    fn name() -> &'static str {
        static NAME: OnceLock<String> = OnceLock::new();
        NAME.get_or_init(|| std::env::var("GLASS_CLIP_PASTEBOARD").unwrap_or_default())
    }

    /// Replacement implementation for `+[NSPasteboard generalPasteboard]`: returns the
    /// private named pasteboard instead of the real one.
    ///
    /// Signature and calling convention match what the Objective-C runtime expects for a
    /// zero-argument class method IMP (`id (*)(Class, SEL)`); the return follows the same
    /// "autoreleased, not owned by the caller" convention the real `generalPasteboard`
    /// uses (its selector carries no `new`/`alloc`/`copy` prefix), via
    /// [`Retained::autorelease_return`] — the pattern `objc2` itself documents for
    /// returning an object from a hand-written method implementation.
    extern "C-unwind" fn glass_general_pasteboard(_cls: &AnyClass, _cmd: Sel) -> *mut NSPasteboard {
        let pb_name = NSString::from_str(name());
        let pb = NSPasteboard::pasteboardWithName(&pb_name);
        Retained::autorelease_return(pb)
    }

    /// Swizzle `+[NSPasteboard generalPasteboard]` and write the sentinel. Called once from
    /// the load-time ctor; inert unless `GLASS_CLIP_PASTEBOARD` is set.
    pub(super) fn install() {
        if std::env::var_os("GLASS_CLIP_PASTEBOARD").is_none() {
            return; // no name set: not a process glass is containing, stay inert
        }

        let method = NSPasteboard::class()
            .class_method(sel!(generalPasteboard))
            .expect("NSPasteboard always declares +generalPasteboard");

        // SAFETY: `glass_general_pasteboard` has the exact `extern "C-unwind" fn(&AnyClass,
        // Sel) -> *mut NSPasteboard` shape the Objective-C runtime expects for this
        // zero-argument class method's IMP. `Imp` is itself defined as a same-ABI, pointer-
        // sized `unsafe extern "C-unwind" fn()` used purely as an opaque carrier type —
        // transmuting our concretely-typed function pointer into it is the same technique
        // objc2's own `MethodImplementation` impls use internally to produce an `Imp` from a
        // typed method fn (see e.g. `objc2::runtime::ClassBuilder::add_class_method`), just
        // applied here to swizzle an existing method instead of defining a new class.
        let imp: Imp = unsafe {
            std::mem::transmute::<extern "C-unwind" fn(&AnyClass, Sel) -> *mut NSPasteboard, Imp>(
                glass_general_pasteboard,
            )
        };
        // SAFETY: `imp` matches the signature the runtime expects for this method (see
        // above). Overriding `generalPasteboard`'s implementation with one that returns a
        // different, equally-valid `NSPasteboard` cannot introduce UB the original method
        // didn't already permit — callers only ever see a normal, live `NSPasteboard`.
        unsafe {
            method.set_implementation(imp);
        }

        // Prove the injection took: write a sentinel item to a DEDICATED `<name>.ready`
        // pasteboard (never the content board the app itself uses — its own `clearContents`
        // on a write would wipe a same-board sentinel) so glass can read it back from the host
        // side and confirm the swizzle is live before trusting a `Private` clipboard route.
        let ready_name = NSString::from_str(&format!("{}.ready", name()));
        let ready = NSPasteboard::pasteboardWithName(&ready_name);
        ready.clearContents();
        let sentinel_type = NSString::from_str(SENTINEL_TYPE);
        let sentinel_value = NSString::from_str("1");
        // Diagnostic only: the swizzle above already took effect regardless of this write. A
        // `false` here means the sentinel write was refused, which would leave glass on the
        // fail-closed `Unsupported` route — surface why rather than failing silently.
        if !ready.setString_forType(&sentinel_value, &sentinel_type) {
            eprintln!("glass-clip-shim: failed to write injection sentinel to {}.ready", name());
        }
    }
}

/// Runs once when the dylib is loaded (via `DYLD_INSERT_LIBRARIES`), before the host
/// process's `main`. See the module doc for what it does and why it's inert unless glass
/// opted this specific process in.
#[cfg(target_os = "macos")]
#[ctor::ctor]
fn glass_clip_shim_load() {
    imp::install();
}
