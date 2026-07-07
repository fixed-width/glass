//! The visible menu-bar UI: an `NSStatusItem` + `NSMenu` driven on the main-thread AppKit
//! run loop. This module owns *only* the AppKit surface — the host (`glass-mcp`) supplies the
//! text to show and boxed callbacks for the actionable items, so this crate stays free of
//! glass-mcp's serve/onboarding logic (matching how every other objc2/AppKit call in this
//! crate is confined to `glass-macos`).
//!
//! ## Threading
//!
//! [`run`] must be called on the process's real main thread ("thread 0") — the same thread
//! `ffi::app_kit_init` established the `NSApplication.sharedApplication` WindowServer
//! connection on (see `ffi.rs`'s `thread0` notes). It takes a [`MainThreadMarker`] (via
//! `MainThreadMarker::new()`, `None` off the main thread) and blocks that thread on
//! `NSApplication::run` — the standard menu-bar-only app pattern. The caller is expected to
//! have already spawned its server on background threads before calling this, since `run`
//! never returns until the app terminates.
//!
//! ## Actions
//!
//! "Quit glass" uses AppKit's built-in `terminate:` routed through the responder chain to
//! `NSApp` (no custom target needed). "Copy endpoint" and "Restart" are wired to a minimal
//! custom [`MenuTarget`] (an `objc2` `define_class!` object) that just invokes the boxed
//! callbacks the host supplied. `setTarget:` is a *weak* reference, so the retained
//! `MenuTarget` (and the status item and menu) are held in locals across `run`'s
//! `NSApplication::run` call for the whole lifetime of the app.

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, NSObjectProtocol, Sel};
use objc2::{define_class, msg_send, sel, DefinedClass, MainThreadMarker, MainThreadOnly};
use objc2_app_kit::{
    NSApplication, NSApplicationActivationPolicy, NSMenu, NSMenuItem, NSStatusBar,
    NSVariableStatusItemLength,
};
use objc2_foundation::NSString;

use glass_core::{GlassError, Result};

/// What the host asks the menu bar to show and do. The two actionable items ("Copy
/// endpoint", "Restart") are boxed callbacks so this crate never has to know *how* they're
/// implemented (glass-mcp's `pbcopy`/`launchctl` calls); the two text fields are shown
/// verbatim.
pub struct MenuBarActions {
    /// The status-item's button title in the menu bar (e.g. `"glass ●"`).
    pub title: String,
    /// The disabled first menu line — the served endpoint URL, or a bind-conflict notice when
    /// another glass already holds the address (so the menu is never a silent dead surface).
    pub status_line: String,
    /// "Copy endpoint" — copy the MCP endpoint to the clipboard. Errors are the host's to
    /// surface; this crate only invokes it.
    pub on_copy: Box<dyn Fn()>,
    /// "Restart" — restart the background LaunchAgent so a fresh process re-reads TCC grants.
    pub on_restart: Box<dyn Fn()>,
}

/// The boxed callbacks, stored in [`MenuTarget`]'s instance variables. Not `Send`/`Sync`, and
/// it needn't be: `MenuTarget` is `MainThreadOnly`, so the object — and thus these closures —
/// only ever runs on the main thread (menu actions dispatch on the main run loop).
struct Ivars {
    on_copy: Box<dyn Fn()>,
    on_restart: Box<dyn Fn()>,
}

define_class!(
    // SAFETY:
    // - `NSObject` imposes no subclassing requirements.
    // - `MenuTarget` implements no `Drop` of its own (the macro drops the ivars — the two
    //   boxed closures — for us at `dealloc`).
    #[unsafe(super(NSObject))]
    // The target is only ever created and messaged on the main thread (menu actions run on
    // the main run loop), so it — and its non-`Send` boxed-closure ivars — are main-thread-only.
    #[thread_kind = MainThreadOnly]
    #[name = "GlassMenuTarget"]
    #[ivars = Ivars]
    struct MenuTarget;

    impl MenuTarget {
        #[unsafe(method(copyEndpoint:))]
        fn copy_endpoint(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_copy)();
        }

        #[unsafe(method(restart:))]
        fn restart(&self, _sender: Option<&AnyObject>) {
            (self.ivars().on_restart)();
        }
    }

    unsafe impl NSObjectProtocol for MenuTarget {}
);

impl MenuTarget {
    fn new(mtm: MainThreadMarker, ivars: Ivars) -> Retained<Self> {
        let this = Self::alloc(mtm).set_ivars(ivars);
        // SAFETY: `NSObject`'s `init` takes no arguments and returns the initialized instance;
        // `this` is a freshly-allocated `MenuTarget` with its ivars set.
        unsafe { msg_send![super(this), init] }
    }
}

/// Build one actionable `NSMenuItem`. `target` is `Some` for our custom-target items
/// ("Copy endpoint"/"Restart") and `None` for `terminate:` (routed to `NSApp` through the
/// responder chain).
fn action_item(
    mtm: MainThreadMarker,
    title: &str,
    action: Sel,
    key_equivalent: &str,
    target: Option<&MenuTarget>,
) -> Retained<NSMenuItem> {
    // SAFETY: `initWithTitle:action:keyEquivalent:` is `unsafe` only because it takes a raw
    // selector — `action` is always either a selector `MenuTarget` implements
    // (`copyEndpoint:`/`restart:`) or AppKit's own `terminate:`; the title and key-equivalent
    // are valid `NSString`s.
    let item = unsafe {
        NSMenuItem::initWithTitle_action_keyEquivalent(
            NSMenuItem::alloc(mtm),
            &NSString::from_str(title),
            Some(action),
            &NSString::from_str(key_equivalent),
        )
    };
    if let Some(target) = target {
        // SAFETY: `setTarget:` is `unsafe` because the target must respond to the item's
        // action selector — `target` is a live `MenuTarget` implementing `copyEndpoint:`/
        // `restart:`, kept alive by [`run`] for the whole run loop (this is a weak reference).
        unsafe { item.setTarget(Some(target)) };
    }
    item.setEnabled(true);
    item
}

/// Run the menu-bar app: install the status item + menu and block the main thread on the
/// AppKit run loop. Returns only if the run loop stops without the process exiting (a user
/// "Quit glass" terminates the process directly via `terminate:`).
///
/// Fails fast with [`GlassError::Backend`] if called off the main thread — the one
/// precondition this crate can enforce; everything else (the status item appearing, the menu
/// firing its actions, `terminate:` quitting cleanly) is main-thread AppKit runtime behavior.
pub fn run(actions: MenuBarActions) -> Result<()> {
    let mtm = MainThreadMarker::new().ok_or_else(|| {
        GlassError::Backend("the menu-bar app must start on the main thread".into())
    })?;

    let app = NSApplication::sharedApplication(mtm);
    // `Accessory` is the runtime equivalent of `LSUIElement`: a menu-bar presence with no Dock
    // icon and no app menu — exactly what a background daemon's status item wants.
    let _ = app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);

    // The status item lives in the system menu bar for the process's lifetime; hold onto it.
    let status_item =
        NSStatusBar::systemStatusBar().statusItemWithLength(NSVariableStatusItemLength);
    if let Some(button) = status_item.button(mtm) {
        button.setTitle(&NSString::from_str(&actions.title));
    }

    let target = MenuTarget::new(
        mtm,
        Ivars {
            on_copy: actions.on_copy,
            on_restart: actions.on_restart,
        },
    );

    let menu = NSMenu::new(mtm);
    // Manage enabled-state ourselves so the informational status line stays greyed regardless
    // of AppKit's automatic target/action-based enabling.
    menu.setAutoenablesItems(false);

    // 1. Disabled status line: the endpoint, or a bind-conflict notice.
    let status_line = NSMenuItem::new(mtm);
    status_line.setTitle(&NSString::from_str(&actions.status_line));
    status_line.setEnabled(false);
    menu.addItem(&status_line);

    let sep1 = NSMenuItem::separatorItem(mtm);
    menu.addItem(&sep1);

    // 2. Copy endpoint. 3. Restart. Both routed to our custom target.
    let copy_item = action_item(mtm, "Copy endpoint", sel!(copyEndpoint:), "", Some(&target));
    menu.addItem(&copy_item);
    let restart_item = action_item(mtm, "Restart", sel!(restart:), "", Some(&target));
    menu.addItem(&restart_item);

    let sep2 = NSMenuItem::separatorItem(mtm);
    menu.addItem(&sep2);

    // 4. Quit glass — nil target lets the responder chain deliver `terminate:` to `NSApp`.
    let quit_item = action_item(mtm, "Quit glass", sel!(terminate:), "q", None);
    menu.addItem(&quit_item);

    status_item.setMenu(Some(&menu));

    // Block the main thread on the AppKit event loop while the server serves on the caller's
    // background threads. `status_item`, `menu`, and `target` must outlive this call
    // (`setTarget:` holds only a weak reference), so they stay bound below.
    //
    // VERIFY on-box: the status item shows the `title` ("glass ●"), the dropdown lists the four
    // items with the status line greyed, "Copy endpoint"/"Restart" invoke the callbacks, and
    // "Quit glass" (⌘Q → `terminate:`) exits the process cleanly. None of this is checkable off
    // a real WindowServer/AppKit run loop; the darwin build only proves it compiles.
    app.run();

    // Reached only if the run loop stops without the process exiting (`terminate:` normally
    // exits the process outright). Keep the AppKit objects alive right up to here.
    drop((status_item, menu, target));
    Ok(())
}
