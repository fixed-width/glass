//! Account setup surface; enrollment and readiness remain in the host.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSButton,
    NSModalPanelRunLoopMode, NSScreen, NSScrollView, NSSecureTextField, NSTextField, NSView,
    NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSRunLoop, NSString};
use std::rc::Rc;
use zeroize::Zeroizing;

use crate::onboarding_window::{ButtonTarget, PollTimer};

#[derive(Clone)]
pub struct SetupProgress {
    pub account: String,
    pub message: String,
    pub step: SetupStep,
    pub running: bool,
    pub can_cancel: bool,
    pub start_account: Option<EnvironmentStart>,
    pub resume_creation: Option<ResumeCreation>,
}

impl SetupProgress {
    fn display_step(&self, account: &str) -> SetupStep {
        if self.account == account {
            self.step
        } else {
            SetupStep::Account
        }
    }

    fn can_continue(&self, account: &str) -> bool {
        !self.running
            && self.account == account
            && !matches!(self.step, SetupStep::Account | SetupStep::Ready)
    }
}

/// The displayed setup step; readiness is still established by the authenticated host checks.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SetupStep {
    Account,
    SignIn,
    Permissions,
    ReturnToOwner,
    Checking,
    Ready,
}

impl SetupStep {
    fn heading(self) -> &'static str {
        match self {
            Self::Account => "Step 1 of 4 — Choose a development account",
            Self::SignIn => "Step 2 of 4 — Sign in and finish macOS setup",
            Self::Permissions => "Step 3 of 4 — Allow Glass permissions",
            Self::ReturnToOwner => "Step 4 of 4 — Return and finish setup",
            Self::Checking => "Checking setup…",
            Self::Ready => "Ready — Your agent can use the development desktop",
        }
    }

    fn row_title(self, index: usize, title: &str) -> String {
        let current = match self {
            Self::Account => Some(0),
            Self::SignIn => Some(1),
            Self::Permissions => Some(2),
            Self::ReturnToOwner => Some(3),
            Self::Ready => Some(4),
            Self::Checking => None,
        };
        let marker = match current {
            Some(current) if index < current => "✓ ",
            Some(current) if index == current => "→ ",
            _ => "",
        };
        format!("{marker}{}. {title}", index + 1)
    }
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
        "Glass creates a standard macOS account. Choose its short name and password. After creation, sign in to that account manually once and finish macOS's welcome screens and Glass permissions. Then return to your normal account, leaving the development account logged in. Glass does not save the password."
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
    alert.setInformativeText(&NSString::from_str("A separate standard macOS account keeps agent applications and input off your desktop. For a new account, first sign in manually to finish macOS setup and allow Glass permissions. Glass guides each step. Shared-desktop applications use your current desktop and permissions."));
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

fn instruction_label(
    mtm: MainThreadMarker,
    view: &NSView,
    text: &str,
    frame: CGRect,
) -> Retained<NSTextField> {
    let label = NSTextField::wrappingLabelWithString(&NSString::from_str(text), mtm);
    label.setFrame(frame);
    label.setSelectable(true);
    view.addSubview(&label);
    label
}

fn update_label(label: &NSTextField, text: &str) {
    if label.stringValue().to_string() != text {
        label.setStringValue(&NSString::from_str(text));
    }
}

fn sign_in_instructions(account: &str) -> String {
    let account = if account.is_empty() {
        "the development account"
    } else {
        account
    };
    format!(
        "For first use, choose Apple menu → Lock Screen, then use Switch User or the account selector to sign in to {account}. Finish all macOS welcome screens until its desktop appears. Use this login-screen route with VNC too. Start environment below is for an account whose first login is already finished."
    )
}

fn return_instructions(account: &str) -> String {
    let account = if account.is_empty() {
        "the development account"
    } else {
        account
    };
    format!(
        "Use Apple menu → Lock Screen to return to your normal account. Leave {account} logged in; do not log it out. Back here, wait if Glass is checking. If checking is paused, choose Continue setup. Start agent work only after Ready."
    )
}

/// Show setup without terminating an existing menu-bar server when the window closes.
pub fn run_setup(actions: SetupActions) -> Result<(), String> {
    let mtm = MainThreadMarker::new().ok_or("Glass setup must run on the AppKit thread")?;
    let app = NSApplication::sharedApplication(mtm);
    let height = NSScreen::mainScreen(mtm)
        .map(|screen| (screen.visibleFrame().size.height - 64.0).clamp(460.0, 760.0))
        .unwrap_or(760.0);
    let frame = rect(0.0, 0.0, 720.0, height);
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
    let heading = instruction_label(
        mtm,
        &view,
        SetupStep::Account.heading(),
        rect(24.0, height - 44.0, 672.0, 28.0),
    );
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(0.0, 108.0, 720.0, height - 164.0),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    let document = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 720.0, 650.0));
    instruction_label(
        mtm,
        &document,
        "Start here in your normal account. A separate macOS account keeps agent applications off your desktop. A new account needs one manual sign-in for macOS setup. Follow these steps in order; login and permission waits have no time limit.",
        rect(24.0, 576.0, 672.0, 60.0),
    );
    let titles = [
        ("Choose or create an account here", 550.0),
        ("Sign in manually and finish macOS setup", 474.0),
        ("Allow Glass permissions in that account", 366.0),
        ("Return to your normal account and finish", 276.0),
    ];
    let step_labels: Vec<_> = titles
        .iter()
        .enumerate()
        .map(|(index, (title, y))| {
            instruction_label(
                mtm,
                &document,
                &SetupStep::Account.row_title(index, title),
                rect(24.0, *y, 672.0, 22.0),
            )
        })
        .collect();
    let sign_in = instruction_label(
        mtm,
        &document,
        &sign_in_instructions(&actions.account),
        rect(24.0, 394.0, 672.0, 74.0),
    );
    instruction_label(
        mtm,
        &document,
        "Glass opens its permission window there if needed. Allow Accessibility first, then Screen Recording, using Request… and System Settings → Privacy & Security. Wait until Glass confirms both permissions. Existing grants are reused; an absent permission window alone does not mean setup is finished.",
        rect(24.0, 304.0, 672.0, 56.0),
    );
    let returning = instruction_label(
        mtm,
        &document,
        &return_instructions(&actions.account),
        rect(24.0, 210.0, 672.0, 60.0),
    );
    instruction_label(
        mtm,
        &document,
        "What to do now",
        rect(24.0, 174.0, 672.0, 22.0),
    );
    let status = instruction_label(
        mtm,
        &document,
        "Create an account, or enter an existing account's short name and choose Use existing account.",
        rect(24.0, 26.0, 672.0, 140.0),
    );
    scroll.setDocumentView(Some(&document));
    view.addSubview(&scroll);

    let account =
        NSTextField::initWithFrame(NSTextField::alloc(mtm), rect(244.0, 510.0, 240.0, 28.0));
    account.setPlaceholderString(Some(&NSString::from_str("Existing account short name")));
    account.setStringValue(&NSString::from_str(&actions.account));
    document.addSubview(&account);
    instruction_label(mtm, &document, "or", rect(216.0, 512.0, 22.0, 22.0));
    let poll: Rc<dyn Fn() -> SetupProgress> = Rc::from(actions.poll);
    let on_begin: Rc<dyn Fn(String)> = Rc::from(actions.on_begin);
    let begin_field = account.clone();
    let begin_action = on_begin.clone();
    let begin_target = ButtonTarget::new(
        mtm,
        Box::new(move || {
            let name = begin_field.stringValue().to_string();
            begin_action(name);
        }),
    );
    let begin = button(
        mtm,
        "Use existing account",
        rect(496.0, 508.0, 200.0, 32.0),
        &begin_target,
    );
    document.addSubview(&begin);

    let continue_field = account.clone();
    let continue_poll = poll.clone();
    let continue_target = ButtonTarget::new(
        mtm,
        Box::new(move || {
            let name = continue_field.stringValue().to_string();
            if continue_poll().can_continue(&name) {
                on_begin(name);
            }
        }),
    );
    let continue_button = button(
        mtm,
        "Continue setup",
        rect(24.0, 64.0, 180.0, 32.0),
        &continue_target,
    );
    view.addSubview(&continue_button);

    let start_account = Rc::new(std::cell::RefCell::new(None::<EnvironmentStart>));
    let start_choice = start_account.clone();
    let start_field = account.clone();
    let start_target = ButtonTarget::new(
        mtm,
        Box::new(move || {
            let name = start_field.stringValue().to_string();
            let Some(start) = start_choice
                .borrow()
                .as_ref()
                .filter(|account| account.name == name)
                .cloned()
            else {
                return;
            };
            if !start.needs_password {
                (actions.on_start)(StartAccount {
                    name,
                    password: None,
                });
            } else if let Some(request) = start_account_dialog(mtm, &name) {
                (actions.on_start)(request);
            }
        }),
    );
    let start_button = button(
        mtm,
        "Start environment…",
        rect(218.0, 64.0, 230.0, 32.0),
        &start_target,
    );
    start_button.setEnabled(false);
    view.addSubview(&start_button);

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
        rect(24.0, 508.0, 180.0, 32.0),
        &create_target,
    );
    document.addSubview(&create);
    let disable_target = ButtonTarget::new(mtm, actions.on_disable);
    let disable = button(
        mtm,
        "Disable isolation",
        rect(24.0, 20.0, 180.0, 32.0),
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
    let close = button(mtm, "Close", rect(556.0, 20.0, 140.0, 32.0), &close_target);
    view.addSubview(&close);

    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let progress = poll();
        let name = account.stringValue().to_string();
        let matches = progress.account == name;
        let step = progress.display_step(&name);
        update_label(&heading, step.heading());
        for (index, (label, (title, _))) in step_labels.iter().zip(titles.iter()).enumerate() {
            update_label(label, &step.row_title(index, title));
        }
        update_label(&sign_in, &sign_in_instructions(&name));
        update_label(&returning, &return_instructions(&name));
        if matches {
            update_label(&status, &progress.message);
        } else {
            update_label(
                &status,
                "The account name changed. Choose Use existing account to check it, or Create account… to create a new one.",
            );
        }
        let starting = progress
            .start_account
            .as_ref()
            .is_some_and(|start| start.name == name);
        let can_continue = progress.can_continue(&name);
        *start_account.borrow_mut() = progress.start_account;
        *resume_creation.borrow_mut() = progress.resume_creation;
        start_button.setEnabled(!progress.running && starting);
        continue_button.setEnabled(can_continue);
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
    });
    refresh();
    let on_tick = refresh.clone();
    let timer = PollTimer::new(mtm, Box::new(move || on_tick()));
    // SAFETY: this timer is retained and invalidated on the same main run loop after the modal loop.
    unsafe { NSRunLoop::mainRunLoop().addTimer_forMode(&timer.timer, NSModalPanelRunLoopMode) };
    // SAFETY: NSObject initialization follows installation of the retained close policy.
    let allocated = SetupDelegate::alloc(mtm).set_ivars(can_close);
    let delegate: Retained<SetupDelegate> = unsafe { msg_send![super(allocated), init] };
    window.setDelegate(Some(ProtocolObject::from_ref(&*delegate)));
    window.setContentView(Some(&view));
    window.center();
    window.makeKeyAndOrderFront(None);
    document.scrollPoint(CGPoint {
        x: 0.0,
        y: (650.0 - scroll.frame().size.height).max(0.0),
    });
    #[allow(deprecated)]
    app.activateIgnoringOtherApps(true);
    let continue_setup = if let Some(resume) = actions.on_resume {
        resume();
        refresh();
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
        continue_target,
        start_target,
        create_target,
        disable_target,
        close_target,
        delegate,
    ));
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejected_selection_cannot_rebind_another_accounts_ready_state() {
        let mut progress = SetupProgress {
            account: "account_a".into(),
            message: String::new(),
            step: SetupStep::SignIn,
            running: true,
            can_cancel: true,
            start_account: None,
            resume_creation: None,
        };
        assert!(!progress.can_continue("account_b"));
        progress.running = false;
        progress.step = SetupStep::Ready;
        assert_eq!(progress.display_step("account_b"), SetupStep::Account);
        assert!(!progress.can_continue("account_b"));
        assert_eq!(progress.display_step("account_a"), SetupStep::Ready);
    }
}
