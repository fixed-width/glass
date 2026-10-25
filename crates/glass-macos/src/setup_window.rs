//! Account setup surface; enrollment and readiness remain in the host.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton,
    NSModalPanelRunLoopMode, NSSecureTextField, NSTextField, NSView, NSWindow, NSWindowDelegate,
    NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSRunLoop, NSString};
use std::rc::Rc;
use zeroize::Zeroizing;

use crate::onboarding_window::{ButtonTarget, PollTimer};

#[derive(Clone)]
pub struct SetupProgress {
    pub message: String,
    pub running: bool,
    pub can_cancel: bool,
    pub start_account: Option<EnvironmentStart>,
    pub resume_creation: Option<ResumeCreation>,
}

pub struct SetupActions {
    pub account: String,
    pub on_resume: Option<Box<dyn FnOnce()>>,
    pub resume_creation: Option<ResumeCreation>,
    pub on_begin: Box<dyn Fn(String)>,
    pub on_disable: Box<dyn Fn()>,
    pub on_create: Box<dyn Fn(NewAccount)>,
    pub on_start: Box<dyn Fn(StartAccount)>,
    pub poll: Box<dyn Fn() -> SetupProgress>,
    pub can_close: Box<dyn Fn() -> bool>,
}

pub struct NewAccount {
    pub name: String,
    pub password: Zeroizing<String>,
    pub previous_attempt: Option<String>,
}

#[derive(Clone)]
pub struct ResumeCreation {
    pub name: String,
    pub attempt: String,
}

pub struct StartAccount {
    pub name: String,
    pub password: Option<Zeroizing<String>>,
}

#[derive(Clone)]
pub struct EnvironmentStart {
    pub name: String,
    pub needs_password: bool,
}

fn start_account_dialog(mtm: MainThreadMarker, name: &str) -> Option<StartAccount> {
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str("Start the development desktop"));
    alert.setInformativeText(&NSString::from_str(&format!("Enter the password for {name}. Glass uses macOS Screen Sharing locally to start its separate desktop and may request administrator approval. This password stays in memory and is not saved.")));
    alert.addButtonWithTitle(&NSString::from_str("Start environment"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let password = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        rect(0.0, 0.0, 360.0, 26.0),
    );
    password.setPlaceholderString(Some(&NSString::from_str("Development account password")));
    alert.setAccessoryView(Some(&password));
    let result = loop {
        if alert.runModal() != 1000 {
            break None;
        }
        let value = Zeroizing::new(password.stringValue().to_string());
        if !value.is_empty() && value.len() <= 63 && !value.contains('\0') && name.len() <= 63 {
            break Some(StartAccount {
                name: name.to_owned(),
                password: Some(value),
            });
        }
        alert.setInformativeText(&NSString::from_str("Local macOS desktop startup supports nonempty passwords and account names up to 63 UTF-8 bytes. Glass will not truncate or change existing credentials."));
    };
    password.setStringValue(&NSString::from_str(""));
    result
}

fn create_account_dialog(
    mtm: MainThreadMarker,
    default_name: &str,
    resume: Option<&ResumeCreation>,
) -> Option<NewAccount> {
    let resuming = resume.is_some();
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(if resuming {
        "Continue development account setup"
    } else {
        "Create a development account"
    }));
    alert.setInformativeText(&NSString::from_str(if resuming {
        "Enter the password you originally chose. Glass did not save it. Glass checks the retained account identity and completes unfinished preparation without resetting existing credentials."
    } else {
        "Glass creates a standard macOS account. Choose its short name and password; you will use them for its first login. Glass does not save the password. The account and its files remain yours when isolation is disabled."
    }));
    alert.addButtonWithTitle(&NSString::from_str(if resuming {
        "Continue setup"
    } else {
        "Create account"
    }));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    let view = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 360.0, 118.0));
    let name = NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(0.0, 82.0, 360.0, 26.0));
    name.setPlaceholderString(Some(&NSString::from_str("Account short name")));
    name.setStringValue(&NSString::from_str(if default_name.is_empty() {
        "glassdev"
    } else {
        default_name
    }));
    name.setEnabled(!resuming);
    let password = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        rect(0.0, 43.0, 360.0, 26.0),
    );
    password.setPlaceholderString(Some(&NSString::from_str("New account password")));
    let confirm = NSSecureTextField::initWithFrame(
        NSSecureTextField::alloc(mtm),
        rect(0.0, 4.0, 360.0, 26.0),
    );
    confirm.setPlaceholderString(Some(&NSString::from_str("Confirm password")));
    view.addSubview(&name);
    view.addSubview(&password);
    view.addSubview(&confirm);
    alert.setAccessoryView(Some(&view));
    let result = loop {
        if alert.runModal() != 1000 {
            break None;
        }
        let name = name.stringValue().to_string();
        let password_text = Zeroizing::new(password.stringValue().to_string());
        let confirmation = Zeroizing::new(confirm.stringValue().to_string());
        let valid_name = name.len() <= 64
            && name.as_bytes().first().is_some_and(u8::is_ascii_alphabetic)
            && name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_');
        if valid_name
            && !password_text.is_empty()
            && password_text.len() <= 1024
            && !password_text.contains('\0')
            && *password_text == *confirmation
        {
            break Some(NewAccount {
                name,
                password: password_text,
                previous_attempt: resume.map(|saved| saved.attempt.clone()),
            });
        }
        alert.setInformativeText(&NSString::from_str("Start the name with an ASCII letter and use at most 64 letters, digits or underscores. Enter matching, nonempty passwords (at most 1024 UTF-8 bytes). macOS also checks this Mac's password policy."));
    };
    password.setStringValue(&NSString::from_str(""));
    confirm.setStringValue(&NSString::from_str(""));
    result
}

pub enum SetupChoice {
    SeparateAccount,
    SharedDesktop,
    Cancel,
}

pub fn choose_environment() -> Result<SetupChoice, String> {
    let mtm = MainThreadMarker::new().ok_or("Glass setup must run on the AppKit thread")?;
    let alert = NSAlert::new(mtm);
    alert.setMessageText(&NSString::from_str(
        "Where should Glass run development applications?",
    ));
    alert.setInformativeText(&NSString::from_str("A separate standard macOS account keeps agent applications and input off your desktop. Glass guides its setup. Shared-desktop applications use your current desktop and permissions."));
    alert.addButtonWithTitle(&NSString::from_str("Off your desktop…"));
    alert.addButtonWithTitle(&NSString::from_str("On your desktop"));
    alert.addButtonWithTitle(&NSString::from_str("Cancel"));
    Ok(match alert.runModal() {
        1000 => SetupChoice::SeparateAccount,
        1001 => SetupChoice::SharedDesktop,
        _ => SetupChoice::Cancel,
    })
}

define_class!(
    // SAFETY: NSObject with a main-thread-only close policy retained by the window host.
    #[unsafe(super(NSObject))]
    #[thread_kind = MainThreadOnly]
    #[name = "GlassSetupWindowDelegate"]
    #[ivars = Rc<dyn Fn() -> bool>]
    struct SetupDelegate;
    unsafe impl NSObjectProtocol for SetupDelegate {}
    unsafe impl NSWindowDelegate for SetupDelegate {
        #[unsafe(method(windowShouldClose:))]
        fn window_should_close(&self, _sender: &NSWindow) -> bool {
            (self.ivars())()
        }
        #[unsafe(method(windowWillClose:))]
        fn window_will_close(&self, _notification: &NSNotification) {
            NSApplication::sharedApplication(self.mtm()).abortModal();
        }
    }
);

fn rect(x: f64, y: f64, width: f64, height: f64) -> CGRect {
    CGRect {
        origin: CGPoint { x, y },
        size: CGSize { width, height },
    }
}

fn button(
    mtm: MainThreadMarker,
    title: &str,
    frame: CGRect,
    target: &ButtonTarget,
) -> Retained<NSButton> {
    // SAFETY: target implements fire: and is retained by run_setup for the modal loop.
    let button = unsafe {
        NSButton::buttonWithTitle_target_action(
            &NSString::from_str(title),
            Some(target),
            Some(sel!(fire:)),
            mtm,
        )
    };
    button.setFrame(frame);
    button
}

/// Show setup without terminating an existing menu-bar server when the window closes.
pub fn run_setup(actions: SetupActions) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Glass setup must run on the AppKit thread")?;
    let app = NSApplication::sharedApplication(mtm);
    let frame = rect(0.0, 0.0, 580.0, 410.0);
    // SAFETY: fresh NSWindow allocation with valid content rectangle and style.
    let window = unsafe {
        NSWindow::initWithContentRect_styleMask_backing_defer(
            NSWindow::alloc(mtm),
            frame,
            NSWindowStyleMask::Titled | NSWindowStyleMask::Closable,
            NSBackingStoreType::Buffered,
            false,
        )
    };
    // SAFETY: the Retained window remains its owner until the modal loop exits.
    unsafe { window.setReleasedWhenClosed(false) };
    window.setTitle(&NSString::from_str("Glass application setup"));
    app.setActivationPolicy(NSApplicationActivationPolicy::Accessory);
    let view = NSView::initWithFrame(NSView::alloc(mtm), frame);
    let introduction = NSTextField::wrappingLabelWithString(
        &NSString::from_str(
            "Keep development applications off your desktop using a separate standard macOS account. Glass handles its worker and checks readiness automatically.",
        ),
        mtm,
    );
    introduction.setFrame(rect(24.0, 327.0, 532.0, 60.0));
    view.addSubview(&introduction);
    let existing_label = NSTextField::labelWithString(
        &NSString::from_str("Or use an existing standard macOS account:"),
        mtm,
    );
    existing_label.setFrame(rect(24.0, 246.0, 532.0, 24.0));
    view.addSubview(&existing_label);
    let account =
        NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(24.0, 216.0, 310.0, 28.0));
    account.setPlaceholderString(Some(&NSString::from_str("Development account name")));
    account.setStringValue(&NSString::from_str(&actions.account));
    view.addSubview(&account);
    let status = NSTextField::wrappingLabelWithString(
        &NSString::from_str(
            "Create a development account here, or enter the short name of an existing standard account. Your credentials stay on this Mac.",
        ),
        mtm,
    );
    status.setFrame(rect(24.0, 78.0, 532.0, 116.0));
    view.addSubview(&status);

    let account_field = account.clone();
    let start_account = Rc::new(std::cell::RefCell::new(None::<EnvironmentStart>));
    let start_choice = start_account.clone();
    let begin_target = ButtonTarget::new(
        mtm,
        Box::new(move || {
            let name = account_field.stringValue().to_string();
            let start = start_choice
                .borrow()
                .as_ref()
                .filter(|account| account.name == name)
                .cloned();
            if let Some(start) = start {
                if !start.needs_password {
                    (actions.on_start)(StartAccount {
                        name,
                        password: None,
                    });
                    return;
                }
                if let Some(request) = start_account_dialog(mtm, &name) {
                    (actions.on_start)(request);
                }
            } else {
                (actions.on_begin)(name);
            }
        }),
    );
    let begin = button(
        mtm,
        "Use existing account",
        rect(340.0, 214.0, 216.0, 32.0),
        &begin_target,
    );
    view.addSubview(&begin);
    let create_field = account.clone();
    let resume_creation = Rc::new(std::cell::RefCell::new(actions.resume_creation));
    let automatic_creation = resume_creation.borrow().is_some();
    let create_resume = resume_creation.clone();
    let create_action: Rc<dyn Fn() -> bool> = Rc::new(move || {
        let name = create_field.stringValue().to_string();
        let resume = create_resume.borrow().clone();
        if let Some(request) = create_account_dialog(
            mtm,
            &name,
            resume.as_ref().filter(|saved| saved.name == name),
        ) {
            create_field.setStringValue(&NSString::from_str(&request.name));
            (actions.on_create)(request);
            true
        } else {
            false
        }
    });
    let click_create = create_action.clone();
    let create_target = ButtonTarget::new(
        mtm,
        Box::new(move || {
            click_create();
        }),
    );
    let create = button(
        mtm,
        "Create account…",
        rect(24.0, 282.0, 210.0, 36.0),
        &create_target,
    );
    view.addSubview(&create);
    let disable_target = ButtonTarget::new(mtm, actions.on_disable);
    let disable = button(
        mtm,
        "Disable isolation",
        rect(24.0, 24.0, 180.0, 32.0),
        &disable_target,
    );
    view.addSubview(&disable);
    let closing_app = app.clone();
    let can_close: Rc<dyn Fn() -> bool> = Rc::from(actions.can_close);
    let close_check = can_close.clone();
    let close_target = ButtonTarget::new(
        mtm,
        Box::new(move || {
            if close_check() {
                closing_app.abortModal();
            }
        }),
    );
    let close = button(mtm, "Close", rect(426.0, 24.0, 130.0, 32.0), &close_target);
    view.addSubview(&close);

    let timer = PollTimer::new(
        mtm,
        Box::new(move || {
            let progress = (actions.poll)();
            status.setStringValue(&NSString::from_str(&progress.message));
            let starting = progress
                .start_account
                .as_ref()
                .is_some_and(|start| start.name == account.stringValue().to_string());
            *start_account.borrow_mut() = progress.start_account;
            *resume_creation.borrow_mut() = progress.resume_creation;
            begin.setTitle(&NSString::from_str(if starting {
                "Start environment…"
            } else {
                "Use existing account"
            }));
            begin.setEnabled(!progress.running);
            create.setEnabled(!progress.running);
            disable.setEnabled(!progress.running);
            account.setEnabled(!progress.running);
            close.setEnabled(!progress.running || progress.can_cancel);
            close.setTitle(&NSString::from_str(if progress.running {
                if progress.can_cancel {
                    "Cancel setup"
                } else {
                    "Finishing…"
                }
            } else {
                "Close"
            }));
        }),
    );
    // SAFETY: this timer is retained and invalidated on the same main run loop after the modal loop.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer.timer, NSModalPanelRunLoopMode) };
    // SAFETY: NSObject initialization follows installation of the retained close policy.
    let allocated = SetupDelegate::alloc(mtm).set_ivars(can_close);
    let delegate: Retained<SetupDelegate> = unsafe { msg_send![super(allocated), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.setContentView(Some(&view));
    window.center();
    window.makeKeyAndOrderFront(None);
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let continue_setup = if let Some(resume) = actions.on_resume {
        resume();
        true
    } else if automatic_creation {
        create_action()
    } else {
        true
    };
    if continue_setup {
        app.runModalForWindow(&window);
    }
    window.orderOut(None);
    drop((
        timer,
        begin_target,
        create_target,
        disable_target,
        close_target,
        delegate,
    ));
    Ok(())
}
