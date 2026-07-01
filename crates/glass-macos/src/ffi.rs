//! objc2 FFI helpers shared across the macOS backend: one-time AppKit init, plus the
//! documented convention every later call site follows for bridging an async ObjC
//! completion-handler back into synchronous Rust.
//!
//! ## The reusable pattern: async completion-handler → channel bridge
//!
//! Proven end-to-end in `.superpowers/sdd/objc2-spike-report.md` against
//! ScreenCaptureKit's nested `SCShareableContent` → `SCScreenshotManager` completion
//! handlers. The concrete `block2::RcBlock`s live at each call site (capture, display
//! provisioning, etc. — Plan 2 tasks 2+), not here; this is the recipe they all follow:
//!
//! 1. Build the block with `block2::RcBlock::new(move |raw_ptr_args...| { ... })`, typed
//!    exactly to match the generated binding's completion-handler signature (check each
//!    API individually — some use raw `*mut T` args, others `Option<NonNull<T>>`; they
//!    are not consistent, so read the generated source rather than assume).
//! 2. Pass `&the_rc_block` directly where a `&block2::DynBlock<dyn Fn(...)>` parameter is
//!    expected — `RcBlock<F>` `Deref`s to `Block<F>` (aka `DynBlock<F>`), no cast needed.
//! 3. Inside the closure, do all the ObjC-object work synchronously (dereference the raw
//!    pointer via `unsafe { &*ptr }`, call further async APIs and nest another block if
//!    needed) and only cross back out of the callback with **plain owned/`Send` data**
//!    (primitives, `String`, an enum of them) over a `std::sync::mpsc::channel`. Never
//!    send a `Retained<T>`/raw objc2 object across the channel — build the final answer
//!    entirely inside the last nested callback instead.
//! 4. Block on `rx.recv_timeout(...)` on the calling thread. The completion handler runs
//!    on whatever queue the framework was told to use (or a GCD default) — it does not
//!    require the caller to be pumping a run loop.
//!
//! ## Gotchas carried forward from the spike (keep in mind at every call site)
//!
//! - `use objc2::AnyThread;` must be in scope for `ClassType::alloc()` on
//!   any-thread-usable classes — otherwise the compiler reports "no associated function
//!   `alloc`" even though the trait method is right there (it's a trait method, and the
//!   trait isn't imported by default).
//! - `NSArray<T>::iter()` yields owned `Retained<T>`, not `&T` — each element is a fresh
//!   strong reference, safe to move out of the loop; calling `.retain()` on the item is a
//!   type error (that's `objc2::Message`'s associated fn, not a `Retained<T>` method).
//! - Several `objc2-core-graphics` free functions are deprecated in favor of associated
//!   functions taking `Option<&Self>` as an explicit first argument, not true `&self`
//!   methods: `CGColorSpaceCreateDeviceRGB()` → `CGColorSpace::new_device_rgb()`,
//!   `CGContextDrawImage(ctx, rect, img)` → `CGContext::draw_image(ctx, rect, img)`,
//!   `CGImageGetWidth(img)` → `CGImage::width(Some(img))`.
//! - `CGBitmapContextCreate` (the classic, non-"Adaptive" constructor) is hand-written at
//!   the `objc2-core-graphics` crate root, not under its `src/generated/` tree — easy to
//!   miss if you only grep `generated/`.
//! - `MainThreadMarker::new()` returns `Option<Self>` (`None` off the main thread) — the
//!   static-checked idiom `objc2-app-kit`'s main-thread-only APIs expect
//!   (`NSApplication::sharedApplication(mtm)` takes it directly); prefer it over an ad
//!   hoc runtime assertion.
//! - `msg_send!`'s return-type inference is automatic from the `let` binding's annotated
//!   type (`Retained<T>` / `Option<Retained<T>>` / `bool` / a primitive) and the
//!   selector's method family (`new`/`alloc`/`init`/`copy`) — no `msg_send_id!` needed
//!   (deprecated in objc2 0.6).

use std::sync::Once;

use objc2::MainThreadMarker;
use objc2_app_kit::NSApplication;
use objc2_foundation::NSError;

use glass_core::GlassError;

use crate::permissions::Permission;

static APP_KIT_INIT: Once = Once::new();

/// `SCStreamErrorDomain`'s code for a declined Screen Recording TCC grant — observed
/// verbatim in the spike's TCC-declined run (`.superpowers/sdd/objc2-spike-report.md`).
const TCC_DECLINE_CODE: isize = -3801;

/// Touch `NSApplication.shared` exactly once to establish this process's connection to
/// the window server. Without it, ScreenCaptureKit/CoreGraphics calls from a bare CLI
/// binary abort with `CGS_REQUIRE_INIT` (proven in the objc2 spike; see the module doc
/// above). The *first* call must happen on the main thread; safe to call repeatedly
/// (including from any other thread) afterward — only the first call does anything.
///
/// The completed-check runs *before* touching `MainThreadMarker` at all: once the
/// one-time init has actually happened, this becomes a cheap, thread-agnostic no-op, so
/// every call site that only cares "has `app_kit_init` run yet" (all of them — see below)
/// can be reached from a non-main worker thread once startup has called
/// [`init_main_thread`] once. See `.superpowers/sdd/thread0-research.md` and
/// `.superpowers/sdd/thread0-spike-report.md` for why this is sound: the WindowServer
/// connection `NSApplication.sharedApplication` establishes is a process-wide, one-time
/// resource, not a per-thread one.
///
/// The main-thread check (for the first, real call) runs *before* `call_once`, not
/// inside its closure: a panic inside `Once::call_once` poisons the `Once` forever
/// (every later call — even a correct one, from the real main thread — would then panic
/// too with "Once instance has previously been poisoned"). Checking first means a single
/// off-thread misuse can't permanently wedge the one-time init for the rest of the
/// process.
///
/// Called by `backend.rs`'s `discover_window` (before `start_app`'s window-discovery poll
/// loop) and by `capture::capture_window` (before every capture) — safe and cheap to call
/// redundantly, since only the first call does anything.
pub(crate) fn app_kit_init() {
    if APP_KIT_INIT.is_completed() {
        return;
    }
    let mtm = MainThreadMarker::new().expect("app_kit_init must run on the main thread");
    APP_KIT_INIT.call_once(|| {
        let _app = NSApplication::sharedApplication(mtm);
    });
}

/// Public entry point for a host process (e.g. `glass-mcp`'s `main()`) to perform the
/// one-time AppKit/WindowServer init from the process's real main thread at startup,
/// before spawning any worker thread that will later call into `MacosPlatform`. Thin
/// wrapper over [`app_kit_init`] — see its doc for the full contract. After this returns,
/// every subsequent `app_kit_init()` call (transitively, every `MacosPlatform` operation)
/// is a cheap no-op safe to call from any thread.
pub fn init_main_thread() {
    app_kit_init();
}

/// Classify a `null` async ScreenCaptureKit result's paired `NSError`:
/// [`GlassError::PermissionDenied`] for a Screen Recording TCC decline (domain
/// `SCStreamErrorDomain`, code `-3801`, and/or a "declined" description — the spike
/// observed all three together, but any one is treated as authoritative since Apple
/// doesn't document which fields are stable across OS versions), [`GlassError::CaptureFailed`]
/// otherwise. `fallback_msg` covers the (framework-contract-violating, but defensively
/// handled) case where both the result and the error came back null.
///
/// Shared by every completion handler in this crate that can hand back a null result —
/// `scwindow.rs`'s discovery query and `capture.rs`'s content/image queries — per this
/// module's async-bridge convention, so a TCC decline is classified identically everywhere
/// instead of each call site rolling its own (partial) version of this check.
pub(crate) fn classify_null_result(err_ptr: *mut NSError, fallback_msg: &str) -> GlassError {
    if err_ptr.is_null() {
        return GlassError::CaptureFailed(fallback_msg.to_string());
    }
    // SAFETY: the framework guarantees a non-null, valid `NSError` whenever it hands back
    // a null content/image — proven in the spike's TCC-declined run.
    let err: &NSError = unsafe { &*err_ptr };
    let domain = err.domain().to_string();
    let code = err.code();
    let description = err.localizedDescription().to_string();
    let detail = format!("{domain} (code {code}): {description}");

    let is_tcc_decline = domain.contains("SCStreamErrorDomain")
        || code == TCC_DECLINE_CODE
        || description.to_lowercase().contains("declined");
    if is_tcc_decline {
        Permission::ScreenRecording.denied_with_detail(detail)
    } else {
        GlassError::CaptureFailed(detail)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[should_panic(expected = "must run on the main thread")]
    fn app_kit_init_panics_off_the_main_thread() {
        // libtest always runs each #[test] on a freshly spawned worker thread, never the
        // process's real main thread, so `MainThreadMarker::new()` is `None` here — this
        // exercises the off-main-thread guard rather than the real NSApplication touch.
        // The real call only happens from Task 2's `MacosPlatform::start_app`, which
        // glass always drives from the main thread.
        app_kit_init();
    }
}
