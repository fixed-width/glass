//! The first-run permission **checklist window**: an `NSWindow` with one row per macOS
//! TCC permission glass needs, driven on the main-thread AppKit run loop. Like
//! [`crate::menubar`], this module owns *only* the AppKit surface — the host (`glass-mcp`)
//! supplies each row's label + current grant snapshot and boxed callbacks for the
//! actionable buttons ("Request…" and "Open Settings…" per row, "Re-check" in the footer), so this crate
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
//! Unlike the menu bar, the onboarder does **not** serve: there is no background tokio server
//! alongside this window, so `run_checklist` owns the whole main thread for its lifetime.
//!
//! ## Actions
//!
//! Each row action and the footer "Re-check" button are wired to a tiny
//! per-button [`ButtonTarget`] (an `objc2` `define_class!` object holding one boxed closure),
//! all responding to the same `fire:` selector — one instance per button, so no selector/tag
//! dispatch is needed. A [`WindowDelegate`] terminates the process on `windowWillClose:` so
//! closing the window ends the run loop. `setTarget:`/`setDelegate:` hold only weak references,
//! so the targets, the delegate, the content view and the window are all kept in locals across
//! `NSApplication::run`.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton, NSTextField,
    NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSString, NSTimer};

/// One permission's observed state and its explicit user actions.
pub struct GrantRow {
    /// The permission's display name.
    pub label: &'static str,
    /// The grant snapshot from this process launch.
    pub granted: bool,
    /// Request native consent and report whether access is currently available.
    /// A false result does not establish whether macOS displayed a prompt.
    pub on_request: Box<dyn Fn() -> bool>,
    /// Open this permission's Settings pane only when the user chooses that action.
    pub on_open_settings: Box<dyn Fn() -> Result<(), String>>,
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
pub(crate) struct ButtonIvars {
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
    pub(crate) struct ButtonTarget;

    impl ButtonTarget {
        #[unsafe(method(fire:))]
        fn fire(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_fire)();
        }
    }

    unsafe impl NSObjectProtocol for ButtonTarget {}
);

impl ButtonTarget {
    pub(crate) fn new(mtm: MainThreadMarker, on_fire: Box<dyn Fn()>) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ButtonIvars { on_fire });
        // SAFETY: `NSObject`'s `init` takes no arguments and returns the initialized
        // instance; `this` is a freshly-allocated `ButtonTarget` with its ivar set.
        unsafe { msg_send![super(this), init] }
    }
}

/// Fresh permission facts supplied by the host without blocking the AppKit thread.
pub struct ChecklistUpdate {
    pub granted: [bool; 2],
    pub message: String,
    pub finished: bool,
}

pub(crate) struct PollTimer {
    pub(crate) timer: Retained<NSTimer>,
    _target: Retained<ButtonTarget>,
}

impl PollTimer {
    pub(crate) fn new(mtm: MainThreadMarker, on_tick: Box<dyn Fn()>) -> Self {
        let target = ButtonTarget::new(mtm, on_tick);
        // SAFETY: target implements fire:, remains retained, and the timer stays on this main run loop.
        let timer = unsafe {
            NSTimer::scheduledTimerWithTimeInterval_target_selector_userInfo_repeats(
                1.0,
                &target,
                sel!(fire:),
                None,
                true,
            )
        };
        Self {
            timer,
            _target: target,
        }
    }
}

impl Drop for PollTimer {
    fn drop(&mut self) {
        self.timer.invalidate();
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

// Fixed-frame layout (points): rows are placed with plain frames, computed top-down and
// converted to AppKit's bottom-left origin by `rect`.
//
const WIDTH: f64 = 640.0;
const H_MARGIN: f64 = 24.0;
const V_MARGIN: f64 = 20.0;
const INSTRUCTION_H: f64 = 72.0;
const SECTION_GAP: f64 = 16.0;
const ROW_H: f64 = 28.0;
const ROW_GAP: f64 = 10.0;
const GLYPH_W: f64 = 20.0;
const GLYPH_GAP: f64 = 8.0;
const LABEL_BUTTON_GAP: f64 = 12.0;
const BUTTON_W: f64 = 130.0;
const FOOTER_H: f64 = 32.0;
const ACTION_H: f64 = 96.0;
const ACTION_GAP: f64 = 8.0;

const CONTENT_W: f64 = WIDTH - 2.0 * H_MARGIN;
const LABEL_W: f64 =
    CONTENT_W - GLYPH_W - GLYPH_GAP - LABEL_BUTTON_GAP - BUTTON_W * 2.0 - ACTION_GAP;

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
    V_MARGIN
        + INSTRUCTION_H
        + SECTION_GAP
        + body_height(n_rows)
        + SECTION_GAP
        + ACTION_H
        + SECTION_GAP
        + FOOTER_H
        + V_MARGIN
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
/// Main-thread only, like [`crate::menubar::run`]: it takes a [`MainThreadMarker`] and blocks
/// the calling thread (thread 0, the `#[tokio::main]` `block_on` thread) on
/// `NSApplication::run`.
///
/// Returns `Err` only for the one precondition this crate can enforce off a real AppKit run
/// loop: being called off the main thread.
pub fn run_checklist(actions: ChecklistActions) -> Result<(), String> {
    run_checklist_with_poll(actions, None)
}

/// Run the checklist with optional fresh-process permission updates.
pub fn run_checklist_with_poll(
    actions: ChecklistActions,
    poll: Option<Box<dyn Fn() -> Option<ChecklistUpdate>>>,
) -> Result<(), String> {
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
            "Allow Accessibility first, then Screen Recording. Request asks macOS for consent. If no prompt appears, choose Open Settings… for that permission. After granting access, choose Re-check or macOS’s Quit & Reopen.",
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

    let action_top = body_top + body_height(n_rows) + SECTION_GAP;
    let action_status = NSTextField::wrappingLabelWithString(
        &NSString::from_str(
            "Complete only missing permissions. Opening Settings does not grant access automatically.",
        ),
        mtm,
    );
    action_status.setFrame(rect(content_h, H_MARGIN, action_top, CONTENT_W, ACTION_H));
    container.addSubview(&action_status);

    // Every per-button target lives here for the whole run loop (`fire:` targets are weak
    // references — see the module doc).
    let mut button_targets: Vec<Retained<ButtonTarget>> = Vec::with_capacity(rows.len() * 2 + 1);
    let mut row_controls = Vec::new();

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
            let label = row.label;
            let request_status = action_status.clone();
            let target = ButtonTarget::new(
                mtm,
                Box::new(move || {
                    let available = (row.on_request)();
                    let message = if available {
                        format!(
                            "{label} is available to this process. After finishing any remaining permission, choose Re-check."
                        )
                    } else {
                        format!(
                            "{label} is not available to this process yet. Finish any macOS consent prompt. If no prompt appears, choose Open Settings… on this row and enable Glass, then return here and choose Re-check."
                        )
                    };
                    request_status.setStringValue(&NSString::from_str(&message));
                }),
            );
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
            button.setEnabled(!row.granted);
            container.addSubview(&button);
            button_targets.push(target);
            let settings_status = action_status.clone();
            let settings_target = ButtonTarget::new(
                mtm,
                Box::new(move || {
                    let message = match (row.on_open_settings)() {
                        Ok(()) => format!(
                            "In System Settings → Privacy & Security, enable Glass for {label}. If Glass is absent, use + to add /Applications/GlassMcp.app. Return here and choose Re-check after granting the missing permissions."
                        ),
                        Err(error) => format!(
                            "Could not open Settings for {label}: {error}. Open System Settings → Privacy & Security manually, enable Glass, then choose Re-check."
                        ),
                    };
                    settings_status.setStringValue(&NSString::from_str(&message));
                }),
            );
            // SAFETY: the retained target implements fire: and outlives the control.
            let settings = unsafe {
                NSButton::buttonWithTitle_target_action(
                    &NSString::from_str("Open Settings…"),
                    Some(&settings_target),
                    Some(sel!(fire:)),
                    mtm,
                )
            };
            settings.setFrame(rect(
                content_h,
                button_x + BUTTON_W + ACTION_GAP,
                row_top,
                BUTTON_W,
                ROW_H,
            ));
            settings.setEnabled(!row.granted);
            container.addSubview(&settings);
            row_controls.push((glyph_label, button, settings));
            button_targets.push(settings_target);
        }
    }

    // Footer "Re-check" button (right-aligned), below the body. The body is a single
    // "ready"/empty line when there are no rows, otherwise the `n_rows` checklist rows.
    let footer_top = action_top + ACTION_H + SECTION_GAP;
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

    // A just-launched `LSUIElement` app does not come forward on its own, and the cooperative
    // `activate()` (macOS 14+) refuses to steal focus from the app that launched us — Finder,
    // right after a double-click — so the checklist would open *behind* the Finder window.
    // `activateIgnoringOtherApps` is deprecated but still the reliable escape hatch, and
    // `orderFrontRegardless` raises the window before the app is fully active.
    window.center();
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    window.orderFrontRegardless();

    let timer = poll.map(|poll| {
        let instruction = instruction.clone();
        let window = window.clone();
        PollTimer::new(
            mtm,
            Box::new(move || {
                if let Some(update) = poll() {
                    instruction.setStringValue(&NSString::from_str(&update.message));
                    for ((glyph, button, settings), granted) in
                        row_controls.iter().zip(update.granted)
                    {
                        glyph.setStringValue(&NSString::from_str(if granted {
                            "✓"
                        } else {
                            "○"
                        }));
                        button.setEnabled(!granted);
                        settings.setEnabled(!granted);
                    }
                    if update.finished {
                        window.close();
                    }
                }
            }),
        )
    });

    // Block the main thread on the AppKit event loop. `window`, `container`, `delegate`, and
    // every `ButtonTarget` must outlive this call (`setTarget:`/`setDelegate:` hold only weak
    // references), so they stay bound until after `run` returns.
    //
    // None of the rendering is checkable off a real WindowServer/AppKit run loop — the darwin
    // build only proves it compiles.
    app.run();
    drop(timer);

    // Reached only if the run loop stops without the process exiting. Keep the AppKit
    // objects alive right up to here.
    drop((window, container, delegate, button_targets));
    Ok(())
}
