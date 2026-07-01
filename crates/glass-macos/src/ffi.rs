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

static APP_KIT_INIT: Once = Once::new();

/// Touch `NSApplication.shared` exactly once to establish this process's connection to
/// the window server. Without it, ScreenCaptureKit/CoreGraphics calls from a bare CLI
/// binary abort with `CGS_REQUIRE_INIT` (proven in the objc2 spike; see the module doc
/// above). Must be called from the main thread; safe to call repeatedly — only the
/// first call does anything.
///
/// The main-thread check runs *before* `call_once`, not inside its closure: a panic
/// inside `Once::call_once` poisons the `Once` forever (every later call — even a
/// correct one, from the real main thread — would then panic too with "Once instance
/// has previously been poisoned"). Checking first means a single off-thread misuse
/// can't permanently wedge the one-time init for the rest of the process.
// Called by `scwindow::find_window_for_pids` before the first `SCShareableContent`
// query; later capture/provisioning call sites (Plan 2's remaining steps) will call it
// too — safe and cheap to call redundantly, since only the first call does anything.
// `find_window_for_pids` itself isn't wired into `MacosPlatform::start_app` yet (a later
// task's job), so this function isn't reachable from any true crate root either; kept
// `#[allow(dead_code)]` (harmless now that it has a real caller) rather than deleted so
// the Once and its doc stay in one place instead of being reintroduced per call site.
#[allow(dead_code)]
pub(crate) fn app_kit_init() {
    let mtm = MainThreadMarker::new().expect("app_kit_init must run on the main thread");
    APP_KIT_INIT.call_once(|| {
        let _app = NSApplication::sharedApplication(mtm);
    });
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
