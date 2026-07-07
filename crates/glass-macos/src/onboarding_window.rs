//! The first-run permission **checklist window**: an `NSWindow` with one row per macOS
//! TCC permission glass needs, driven on the main-thread AppKit run loop. Like
//! [`crate::menubar`], this module owns *only* the AppKit surface — the host (`glass-mcp`)
//! supplies each row's label + current grant snapshot and boxed callbacks for the
//! actionable buttons ("Request…" per row, "Re-check" in the footer), so this crate
//! stays free of glass-mcp's permission-probing / relaunch logic and every AppKit `unsafe`
//! stays confined here.
//!
//! ## Threading
//!
//! [`run_checklist`] must be called on the process's real main thread ("thread 0") — the
//! same thread `ffi::app_kit_init` establishes the `NSApplication` WindowServer connection
//! on (see `ffi.rs`'s `thread0` notes). It takes a [`MainThreadMarker`] (via
//! `MainThreadMarker::new()`, `None` off the main thread) and blocks that thread on
//! `NSApplication::run`, exactly like the menu-bar app.
//!
//! Unlike the menu bar, the onboarder does **not** serve: there is no background tokio
//! server running alongside this window. The onboarding process exists only to show this
//! window and run its loop, so `run_checklist` owns the whole main thread for its lifetime
//! and nothing needs to have been spawned before it.
//!
//! ## Actions
//!
//! Each row's "Request…" button and the footer "Re-check" button are wired to a tiny
//! per-button [`ButtonTarget`] (an `objc2` `define_class!` object holding one boxed
//! closure), all responding to the same `fire:` selector — the same minimal-target idiom
//! `menubar.rs` uses, one instance per button so no selector/tag dispatch is needed. A
//! [`WindowDelegate`] terminates the process on `windowWillClose:` so closing the window
//! ends the run loop (the onboarder has no other windows and no server to keep alive).
//! `setTarget:`/`setDelegate:` hold only weak references, so the retained targets, the
//! delegate, the content view and the window are all kept in locals across
//! `NSApplication::run` for the whole lifetime of the app.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSTextField,
    NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSString};

/// One permission's row: the human label, its current grant snapshot (drives the status
/// glyph), and the action that requests the permission's grant (macOS then shows its own
/// consent prompt). `on_open_settings` is a boxed callback so this crate never has to know
/// *how* the grant is requested (glass-mcp's `request_*` calls).
pub struct GrantRow {
    /// The permission's display name (e.g. `"Screen Recording"`).
    pub label: &'static str,
    /// Whether the permission is currently granted — snapshot taken by the host just before
    /// building the window. Drives the ✓ / ○ status glyph; never re-read here.
    pub granted: bool,
    /// "Request…" — request this permission's grant. macOS shows its own consent prompt in
    /// response (whose "Open System Settings" button navigates to the pane); this callback
    /// does not open Settings itself. Invoked on the main thread.
    pub on_open_settings: Box<dyn Fn()>,
}

/// What the host asks the checklist window to show and do: one [`GrantRow`] per permission
/// plus the footer "Re-check" action. The host only opens this window when at least one
/// permission is missing, so an empty `rows` is the sole (defensive) "nothing to configure"
/// case — see [`run_checklist`]'s empty-state handling.
pub struct ChecklistActions {
    /// One row per permission, each with its current snapshot and open/request action.
    pub rows: Vec<GrantRow>,
    /// "Re-check" — re-probe permissions by relaunching as a fresh process (so a new
    /// process re-reads TCC grants) and exiting this one. Supplied by glass-mcp; this crate
    /// only invokes it.
    pub on_recheck: Box<dyn Fn()>,
}

/// The single boxed callback stored in a [`ButtonTarget`]'s instance variables. Not
/// `Send`/`Sync`, and it needn't be: `ButtonTarget` is `MainThreadOnly`, so the object —
/// and thus this closure — only ever runs on the main thread (button actions dispatch on
/// the main run loop).
struct ButtonIvars {
    on_fire: Box<dyn Fn()>,
}

define_class!(
    // SAFETY:
    // - `NSObject` imposes no subclassing requirements.
    // - `ButtonTarget` implements no `Drop` of its own (the macro drops the ivar — the
    //   boxed closure — for us at `dealloc`).
    #[unsafe(super(NSObject))]
    // Button actions run on the main run loop, so the target — and its non-`Send`
    // boxed-closure ivar — is main-thread-only.
    #[thread_kind = MainThreadOnly]
    #[name = "GlassOnboardingButtonTarget"]
    #[ivars = ButtonIvars]
    struct ButtonTarget;

    impl ButtonTarget {
        #[unsafe(method(fire:))]
        fn fire(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_fire)();
        }
    }

    unsafe impl NSObjectProtocol for ButtonTarget {}
);

impl ButtonTarget {
    fn new(mtm: MainThreadMarker, on_fire: Box<dyn Fn()>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ButtonIvars { on_fire });
        // SAFETY: `NSObject`'s `init` takes no arguments and returns the initialized
        // instance; `this` is a freshly-allocated `ButtonTarget` with its ivar set.
        unsafe { msg_send![super(this), init] }
    }
}

define_class!(
    // SAFETY:
    // - `NSObject` imposes no subclassing requirements.
    // - `WindowDelegate` implements no `Drop` of its own and has no ivars.
    #[unsafe(super(NSObject))]
    // `NSWindowDelegate` requires `MainThreadOnly`; window notifications dispatch on the
    // main run loop.
    #[thread_kind = MainThreadOnly]
    #[name = "GlassOnboardingWindowDelegate"]
    struct WindowDelegate;

    unsafe impl NSObjectProtocol for WindowDelegate {}

    unsafe impl NSWindowDelegate for WindowDelegate {
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            // The onboarder has no server and no other windows, so closing the checklist
            // window should end the process (and thus return control from the AppKit loop
            // in `run_checklist`). `self.mtm()` is sound: this delegate is `MainThreadOnly`
            // and the notification is delivered on the main run loop.
            NSApplication::sharedApplication(self.mtm()).terminate(None);
        }
    }
);

impl WindowDelegate {
    fn new(mtm: MainThreadMarker) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(());
        // SAFETY: `NSObject`'s `init` takes no arguments and returns the initialized
        // instance; `this` is a freshly-allocated, zero-ivar `WindowDelegate`.
        unsafe { msg_send![super(this), init] }
    }
}

// Fixed-frame layout (points). Auto Layout / `NSStackView` would add a second constraint
// API surface (and its own feature) for no benefit at this size, so rows are placed with
// plain frames — computed top-down and converted to AppKit's bottom-left origin by `rect`.
//
// VERIFY on-box: these constants are eyeballed, not measured — confirm on a real render that
// labels aren't clipped, the ✓/○ glyph and the "Request…" button sit level with each
// row's name, and nothing overlaps at the default system font size; nudge the constants if so.
const WIDTH: f64 = 480.0;
const H_MARGIN: f64 = 24.0;
const V_MARGIN: f64 = 20.0;
const INSTRUCTION_H: f64 = 40.0;
const SECTION_GAP: f64 = 16.0;
const ROW_H: f64 = 28.0;
const ROW_GAP: f64 = 10.0;
const GLYPH_W: f64 = 20.0;
const GLYPH_GAP: f64 = 8.0;
const LABEL_BUTTON_GAP: f64 = 12.0;
const BUTTON_W: f64 = 130.0;
const FOOTER_H: f64 = 32.0;

const CONTENT_W: f64 = WIDTH - 2.0 * H_MARGIN;
const LABEL_W: f64 = CONTENT_W - GLYPH_W - GLYPH_GAP - LABEL_BUTTON_GAP - BUTTON_W;

/// Height of the row list (or one line in the defensive empty-`rows` case). Shared by
/// [`content_height`] (total window height) and the footer's top offset in [`run_checklist`]
/// — both must agree on where the body ends, so this is the single source of truth for that
/// formula.
fn body_height(n_rows: usize) -> f64 {
    if n_rows == 0 {
        ROW_H
    } else {
        n_rows as f64 * ROW_H + (n_rows as f64 - 1.0) * ROW_GAP
    }
}

/// Total content height, driven by how many rows there are (or one line in the defensive
/// empty-`rows` case).
fn content_height(n_rows: usize) -> f64 {
    V_MARGIN + INSTRUCTION_H + SECTION_GAP + body_height(n_rows) + SECTION_GAP + FOOTER_H + V_MARGIN
}

/// Converts a top-down `(x, top, w, h)` box into an AppKit bottom-left-origin [`CGRect`]
/// within a content view of height `content_h`.
fn rect(content_h: f64, x: f64, top: f64, w: f64, h: f64) -> CGRect {
    CGRect {
        origin: CGPoint {
            x,
            y: content_h - top - h,
        },
        size: CGSize {
            width: w,
            height: h,
        },
    }
}

/// Show the permission checklist window and run the AppKit loop until the window closes or
/// the process terminates (either via a row/re-check action that relaunches-and-exits, or
/// via the window's close button → [`WindowDelegate::window_will_close`] → `terminate:`).
///
/// Main-thread only, like [`crate::menubar::run`]: it takes a [`MainThreadMarker`] and
/// blocks the calling thread (thread 0, the `#[tokio::main]` `block_on` thread) on
/// `NSApplication::run`. Unlike the menu bar there is no background server here — the
/// onboarder does nothing but run this window loop.
///
/// Returns `Err` only for the one precondition this crate can enforce off a real AppKit run
/// loop: being called off the main thread. Everything else (the window appearing, the
/// buttons firing their callbacks, the delegate terminating on close) is main-thread AppKit
/// runtime behavior verified on-box.
pub fn run_checklist(actions: ChecklistActions) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or_else(|| {
        "the onboarding checklist window must start on the main thread".to_string()
    })?;

    let ChecklistActions { rows, on_recheck } = actions;
    let n_rows = rows.len();

    let app = NSApplication::sharedApplication(mtm);
    // `Regular` (a normal foreground app with a Dock icon), not the menu bar's `Accessory`:
    // the onboarder is a visible, front-and-center window the user interacts with directly.
    app.setActivationPolicy(NSApplicationActivationPolicy::Regular);

    let content_h = content_height(n_rows);

    // The window. `Titled | Closable` (no `Resizable`): a fixed-size utility window.
    let style = NSWindowStyleMask::Titled | NSWindowStyleMask::Closable;
    let content_rect = CGRect {
        origin: CGPoint { x: 0.0, y: 0.0 },
        size: CGSize {
            width: WIDTH,
            height: content_h,
        },
    };
    // SAFETY: `initWithContentRect:styleMask:backing:defer:` is `unsafe` only as a
    // designated initializer; `this` is a freshly-allocated `NSWindow`, and all four
    // arguments are valid (a well-formed content rect, a supported style-mask combination,
    // the standard `Buffered` backing store, and `defer: false`).
    let window: Retained<NSWindow> = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            content_rect,
            style,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    window.setTitle(&NSString::from_str("glass permissions"));
    // SAFETY: `setReleasedWhenClosed:` is `unsafe` because it changes the window's memory
    // ownership. We hold the sole strong reference (`window`) for the whole run loop, so
    // opting out of AppKit's auto-release-on-close makes that `Retained` the unambiguous
    // owner and avoids an over-release if the loop ever unwinds without the process exiting.
    unsafe { window.setReleasedWhenClosed(false) };

    // Container content view spanning the whole content rect; every label/button is added
    // as a subview (which retains it, so only the view hierarchy — and the window that
    // retains this view — must be kept alive, done via the bindings held across `run` below).
    let container = NSView::initWithFrame(NSView::alloc(mtm), content_rect);

    // Instruction line (top). A wrapping label so the full sentence shows even if the
    // system font is large.
    let instruction = NSTextField::wrappingLabelWithString(
        &NSString::from_str(
            "glass needs these macOS permissions to drive apps. Grant each, then re-check.",
        ),
        mtm,
    );
    instruction.setFrame(rect(
        content_h,
        H_MARGIN,
        V_MARGIN,
        CONTENT_W,
        INSTRUCTION_H,
    ));
    container.addSubview(&instruction);

    let body_top = V_MARGIN + INSTRUCTION_H + SECTION_GAP;

    // Every per-button target lives here for the whole run loop (`fire:` targets are weak
    // references — see the module doc).
    let mut button_targets: Vec<Retained<ButtonTarget>> = Vec::with_capacity(rows.len() + 1);

    if rows.is_empty() {
        // Defensive: the host only opens this window when a permission is missing, so an empty
        // row set means it shouldn't have opened it at all — show a line rather than a blank
        // checklist.
        let ready =
            NSTextField::labelWithString(&NSString::from_str("No permissions to configure."), mtm);
        ready.setFrame(rect(content_h, H_MARGIN, body_top, CONTENT_W, ROW_H));
        container.addSubview(&ready);
    } else {
        let label_x = H_MARGIN + GLYPH_W + GLYPH_GAP;
        let button_x = label_x + LABEL_W + LABEL_BUTTON_GAP;
        for (i, row) in rows.into_iter().enumerate() {
            let row_top = body_top + i as f64 * (ROW_H + ROW_GAP);

            // Status glyph: ✓ when granted, ○ when not.
            let glyph = if row.granted { "✓" } else { "○" };
            let glyph_label = NSTextField::labelWithString(&NSString::from_str(glyph), mtm);
            glyph_label.setFrame(rect(content_h, H_MARGIN, row_top, GLYPH_W, ROW_H));
            container.addSubview(&glyph_label);

            // Permission name.
            let name = NSTextField::labelWithString(&NSString::from_str(row.label), mtm);
            name.setFrame(rect(content_h, label_x, row_top, LABEL_W, ROW_H));
            container.addSubview(&name);

            // "Request…" button → this row's boxed action, via its own target.
            let target = ButtonTarget::new(mtm, row.on_open_settings);
            // SAFETY: `buttonWithTitle:target:action:` is `unsafe` because the target must
            // respond to the action selector — `target` is a live `ButtonTarget` (kept in
            // `button_targets` for the whole run loop) that implements `fire:`, and
            // `sel!(fire:)` is exactly that selector. The title is a valid `NSString`.
            let button = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("Request…"),
                    Some(&target),
                    Some(sel!(fire:)),
                    mtm,
                )
            };
            button.setFrame(rect(content_h, button_x, row_top, BUTTON_W, ROW_H));
            container.addSubview(&button);
            button_targets.push(target);
        }
    }

    // Footer "Re-check" button (right-aligned), below the body. The body is a single
    // "ready"/empty line when there are no rows, otherwise the `n_rows` checklist rows.
    let footer_top = body_top + body_height(n_rows) + SECTION_GAP;
    let recheck_target = ButtonTarget::new(mtm, on_recheck);
    // SAFETY: identical contract to the per-row buttons above — `recheck_target` is a live
    // `ButtonTarget` implementing `fire:`, kept alive in `button_targets` across the run
    // loop; `sel!(fire:)` is its action selector; the title is a valid `NSString`.
    let recheck = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str("Re-check"),
            Some(&recheck_target),
            Some(sel!(fire:)),
            mtm,
        )
    };
    recheck.setFrame(rect(
        content_h,
        WIDTH - H_MARGIN - BUTTON_W,
        footer_top,
        BUTTON_W,
        FOOTER_H,
    ));
    container.addSubview(&recheck);
    button_targets.push(recheck_target);

    window.setContentView(Some(&container));

    // Terminate the process when the window closes so the AppKit loop below returns.
    let delegate = WindowDelegate::new(mtm);
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));

    // Center, show, and force to the front. A just-launched `LSUIElement` app does not come
    // forward on its own, and the modern cooperative `activate()` (macOS 14+) deliberately
    // refuses to steal focus from the app that launched us — Finder, immediately after a
    // double-click — so the checklist would open *behind* the Finder window and look like
    // nothing happened. Use the forceful path instead: `activateIgnoringOtherApps` (deprecated
    // but still the reliable escape hatch for a user-initiated launch) plus
    // `orderFrontRegardless`, which raises the window even before the app is fully active.
    window.center();
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.orderFrontRegardless();

    // Block the main thread on the AppKit event loop. `window`, `container`, `delegate`, and
    // every `ButtonTarget` must outlive this call (`setTarget:`/`setDelegate:` hold only weak
    // references), so they stay bound until after `run` returns.
    //
    // VERIFY on-box: the window shows the title "glass permissions", the instruction line,
    // one row per permission (✓ for granted, ○ for not) each with a working "Request…"
    // button, and a "Re-check" button that fires `on_recheck`; the window comes to the
    // front on launch; closing it exits the process. None of this is checkable off a real
    // WindowServer/AppKit run loop — the darwin build only proves it compiles.
    app.run();

    // Reached only if the run loop stops without the process exiting. Keep the AppKit
    // objects alive right up to here.
    drop((window, container, delegate, button_targets));
    Ok(())
}
