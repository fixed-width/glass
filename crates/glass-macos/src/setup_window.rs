//! Account setup surface; enrollment and readiness remain in the host.

use objc2::rc::Retained;
use objc2::runtime::{NSObject, NSObjectProtocol, ProtocolObject};
use objc2::{DefinedClass, MainThreadMarker, MainThreadOnly, define_class, msg_send, sel};
use objc2_app_kit::{
    NSAlert, NSApplication, NSApplicationActivationPolicy, NSBackingStoreType, NSBox, NSBoxType,
    NSButton, NSColor, NSFont, NSModalPanelRunLoopMode, NSScreen, NSScrollView, NSSecureTextField,
    NSTextField, NSTitlePosition, NSView, NSWindow, NSWindowDelegate, NSWindowStyleMask,
};
use objc2_core_foundation::{CGPoint, CGRect, CGSize};
use objc2_foundation::{NSNotification, NSRunLoop, NSString};
use std::rc::Rc;
use zeroize::Zeroizing;

use crate::onboarding_window::{ButtonTarget, PollTimer};

#[derive(Clone)]
pub struct SetupProgress {
    pub permission_check_error: Option<String>,
    pub permission_grants: Option<[bool; 2]>,
    pub creation_blocker: Option<String>,
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

    fn display_message(&self, edited_account: &str) -> String {
        let message = match &self.permission_check_error {
            Some(error) => format!(
                "Permission check failed: {error}. Retrying automatically.\n\n{}",
                self.message
            ),
            None => self.message.clone(),
        };
        if self.account == edited_account {
            return message;
        }
        let context = if self.account.is_empty() {
            "Current setup".to_owned()
        } else {
            format!("Setup for {}", self.account)
        };
        let guidance = if self.creation_blocker.is_some() {
            "Changing the name does not change enrollment. Resolve the enrolled setup above before selecting another account."
        } else {
            "The selected name changed. Choose Use existing account to check it, or Create account… to create a new one."
        };
        format!("{context}: {message}\n\n{guidance}")
    }

    /// Whether the wizard may open account creation.
    pub fn can_create(&self) -> bool {
        !self.running && self.creation_blocker.is_none()
    }

    /// Clear account-dependent actions only after successful enrollment removal.
    pub fn isolation_disabled(&mut self) {
        self.permission_check_error = None;
        self.permission_grants = None;
        self.creation_blocker = None;
        self.start_account = None;
        self.resume_creation = None;
        self.step = SetupStep::Account;
    }

    fn permission_display(&self, initial: [bool; 2]) -> ([bool; 2], bool) {
        (
            self.permission_grants.unwrap_or(initial),
            self.permission_grants.is_none(),
        )
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
    StartEnvironment,
    Checking,
    Ready,
}

impl SetupStep {
    fn heading(self) -> &'static str {
        match self {
            Self::Account => "Step 1 of 5 — Choose a development account",
            Self::SignIn => "Step 2 of 5 — Log in manually and finish macOS setup",
            Self::Permissions => "Step 4 of 5 — Allow Glass permissions",
            Self::ReturnToOwner => "Step 3 of 5 — Log out and return to your main account",
            Self::StartEnvironment => "Step 5 of 5 — Start the environment",
            Self::Checking => "Checking setup…",
            Self::Ready => "Ready — Your agent can use the development desktop",
        }
    }

    fn active_index(self) -> Option<usize> {
        match self {
            Self::Account => Some(0),
            Self::SignIn => Some(1),
            Self::ReturnToOwner => Some(2),
            Self::Permissions => Some(3),
            Self::StartEnvironment | Self::Ready => Some(4),
            Self::Checking => None,
        }
    }

    fn row_title(self, index: usize, title: &str) -> String {
        let current = match self {
            Self::Account => Some(0),
            Self::SignIn => Some(1),
            Self::Permissions => Some(3),
            Self::ReturnToOwner => Some(2),
            Self::StartEnvironment => Some(4),
            Self::Ready => Some(5),
            Self::Checking => None,
        };
        let marker = match current {
            Some(current) if index < current && self != Self::StartEnvironment => "✓ ",
            Some(current) if index == current => "→ ",
            _ => "",
        };
        format!("{marker}{}. {title}", index + 1)
    }
}

struct InlineStatusLayout {
    login_space: f64,
    permission_space: f64,
    status_space: f64,
    status_anchor: f64,
}

impl InlineStatusLayout {
    fn new(active: usize, controls_height: f64, status_height: f64, login_height: f64) -> Self {
        let login_space = (login_height - 74.0).max(0.0);
        let permission_space = controls_height + 18.0;
        let content_bottom = [404.0, 290.0, 200.0, 130.0, 0.0][active];
        Self {
            login_space,
            permission_space,
            status_space: status_height + 12.0,
            status_anchor: content_bottom - 6.0
                + if active == 0 { login_space } else { 0.0 }
                + if active < 3 { permission_space } else { 0.0 },
        }
    }

    fn status_shift(&self, y: f64) -> f64 {
        if y >= self.status_anchor {
            self.status_space
        } else {
            0.0
        }
    }

    fn guide_y(&self, y: f64) -> f64 {
        let y = y + if y > 290.0 { self.login_space } else { 0.0 };
        let y = y + if y >= 124.0 {
            self.permission_space
        } else {
            0.0
        };
        y + self.status_shift(y) + 12.0
    }

    fn controls_y(&self) -> f64 {
        130.0 + self.status_shift(130.0) + 12.0
    }

    fn status_y(&self) -> f64 {
        self.status_anchor + 20.0
    }

    fn height(&self) -> f64 {
        558.0 + self.login_space + self.permission_space + self.status_space
    }
}

pub struct SetupActions {
    pub permission_rows: Vec<crate::onboarding_window::GrantRow>,
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
    alert.addButtonWithTitle(&NSString::from_str("Start Environment"));
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
        "Glass creates a standard macOS account. Choose its short name and password. After creation, log in to that account manually once and finish macOS's welcome screens until its desktop appears. Then log out of that account and return to your normal account. Choose Start Environment here to start its private desktop. Glass does not save the password."
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
    alert.setInformativeText(&NSString::from_str("A separate standard macOS account keeps application windows off your desktop, and prevents interference with your cursor and clipboard. A wizard will walk you through the setup steps."));
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

fn sign_in_instructions(_account: &str) -> String {
    "Skip to step 4 if the development account already has desktop access.\n\nSelect Apple menu → \"Lock screen\" (do not fully log out of your main account), then log in to the developer account. Proceed through all of the macOS first-use welcome screens, until its desktop is available. No further actions are required in the developer account.".into()
}

fn start_instructions(_account: &str) -> String {
    "Select \"Start Environment...\" below and enter the development account's password. Complete any administrator prompt that appears. Once this window shows Ready status, setup is complete. You will need to \"Start Environment...\" after any reboot.".into()
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
    let scroll = NSScrollView::initWithFrame(
        NSScrollView::alloc(mtm),
        rect(0.0, 108.0, 720.0, height - 120.0),
    );
    scroll.setHasVerticalScroller(true);
    scroll.setAutohidesScrollers(true);
    scroll.setDrawsBackground(false);
    let document = NSView::initWithFrame(NSView::alloc(mtm), rect(0.0, 0.0, 720.0, 650.0));
    let highlight = NSBox::initWithFrame(NSBox::alloc(mtm), rect(12.0, 0.0, 696.0, 100.0));
    highlight.setBoxType(NSBoxType::Custom);
    highlight.setTitlePosition(NSTitlePosition::NoTitle);
    highlight.setBorderWidth(1.0);
    highlight.setCornerRadius(6.0);
    highlight.setTransparent(false);
    highlight.setBorderColor(&NSColor::controlAccentColor());
    highlight.setFillColor(&NSColor::controlAccentColor().colorWithAlphaComponent(0.05));
    document.addSubview(&highlight);
    let setup_notice = instruction_label(
        mtm,
        &document,
        "Start here in your normal account. A separate macOS account keeps agent applications off your desktop. A new account needs one manual login for macOS setup. Follow these steps in order; authentication and setup waits have no time limit.",
        rect(24.0, 576.0, 672.0, 60.0),
    );
    let titles = [
        ("Choose or create an account here", 550.0),
        ("Log in manually and finish macOS setup", 474.0),
        ("Log out and return to your main account", 366.0),
        ("Allow Glass permissions in your main account", 276.0),
        ("Start the environment", 200.0),
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
        "Log out of the developer account (you can fully log out), then log back in to your main account.",
        rect(24.0, 304.0, 672.0, 56.0),
    );
    instruction_label(
        mtm,
        &document,
        "Select \"Open Settings\" for each missing permission. Approve GlassMcp, then proceed to step 5. Glass checks approvals here automatically; ? means a check is pending.",
        rect(24.0, 228.0, 672.0, 40.0),
    );
    let returning = instruction_label(
        mtm,
        &document,
        &start_instructions(&actions.account),
        rect(24.0, 104.0, 672.0, 88.0),
    );
    let status = instruction_label(
        mtm,
        &document,
        "Create an account, or enter an existing account's short name and choose Use existing account.",
        rect(24.0, 26.0, 672.0, 140.0),
    );
    let initial_permission_grants = std::array::from_fn(|index| {
        actions
            .permission_rows
            .get(index)
            .is_some_and(|row| row.granted)
    });
    let permission_controls = Rc::new(crate::onboarding_window::GrantControls::new(
        mtm,
        actions.permission_rows,
        672.0,
    ));
    permission_controls
        .view
        .setFrameOrigin(CGPoint { x: 24.0, y: 8.0 });
    status.removeFromSuperview();
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
        "Start Environment…",
        rect(218.0, 64.0, 230.0, 32.0),
        &start_target,
    );
    start_button.setEnabled(false);
    view.addSubview(&start_button);

    let create_field = account.clone();
    let resume_creation = Rc::new(std::cell::RefCell::new(actions.resume_creation));
    let automatic_creation = resume_creation.borrow().is_some();
    let create_resume = resume_creation.clone();
    let create_poll = poll.clone();
    let create_action: Rc<dyn Fn() -> bool> = Rc::new(move || {
        if !create_poll().can_create() {
            return false;
        }
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

    for child in document.subviews().into_iter().skip(1) {
        let frame = child.frame();
        child.setFrameOrigin(CGPoint {
            x: frame.origin.x,
            y: frame.origin.y - 104.0,
        });
    }
    document.setFrameSize(CGSize {
        width: 720.0,
        height: 546.0,
    });
    let guide_frames: Vec<_> = document
        .subviews()
        .into_iter()
        .skip(1)
        .map(|child| {
            let frame = child.frame();
            (child, frame)
        })
        .collect();
    document.addSubview(&permission_controls.view);
    document.addSubview(&status);
    let refresh_document = document.clone();
    let refresh_highlight = highlight.clone();
    let refresh_scroll = scroll.clone();
    let active_step = std::cell::Cell::new(0);
    let last_active = std::cell::Cell::new(None::<usize>);
    let refresh: Rc<dyn Fn()> = Rc::new(move || {
        let progress = poll();
        let name = account.stringValue().to_string();
        let step = progress.display_step(&name);
        let active = step.active_index().unwrap_or(active_step.get());
        active_step.set(active);
        let changed_step = last_active.replace(Some(active)) != Some(active);
        let visible = refresh_document.visibleRect();
        let visible_top = visible.origin.y + visible.size.height;
        let at_top = visible_top >= refresh_document.frame().size.height - 1.0;
        let old_frame = refresh_highlight.frame();
        let follows_active =
            (visible_top - old_frame.origin.y - old_frame.size.height - 8.0).abs() < 2.0;
        let (grants, unconfirmed) = progress.permission_display(initial_permission_grants);
        permission_controls.update(grants, progress.can_cancel);
        if unconfirmed {
            permission_controls.show_unconfirmed(progress.can_cancel);
        }
        update_label(&status, &progress.display_message(&name));
        let status_height = crate::onboarding_window::text_height(&status, 656.0);
        let login_height = crate::onboarding_window::text_height(&sign_in, 672.0).max(74.0);
        let layout = InlineStatusLayout::new(
            active,
            permission_controls.layout(),
            status_height,
            login_height,
        );
        for (child, frame) in &guide_frames {
            child.setFrameOrigin(CGPoint {
                x: frame.origin.x,
                y: layout.guide_y(frame.origin.y),
            });
        }
        sign_in.setFrameSize(CGSize {
            width: 672.0,
            height: login_height,
        });
        permission_controls.view.setFrameOrigin(CGPoint {
            x: 24.0,
            y: layout.controls_y(),
        });
        status.setFrame(rect(40.0, layout.status_y(), 656.0, status_height));
        refresh_document.setFrameSize(CGSize {
            width: 720.0,
            height: layout.height().max(refresh_scroll.frame().size.height),
        });
        let title = step_labels[active].frame();
        let top = title.origin.y + title.size.height + if active == 0 { 2.0 } else { 6.0 };
        let bottom = layout.status_y() - 6.0;
        let active_frame = rect(12.0, bottom, 696.0, top - bottom);
        refresh_highlight.setFrame(active_frame);
        refresh_highlight.setTitle(&NSString::from_str(step.heading()));
        let viewport_height = refresh_scroll.frame().size.height;
        let scroll_top = if changed_step {
            Some(if active == 0 {
                refresh_document.frame().size.height
            } else {
                top + 8.0
            })
        } else if at_top {
            Some(refresh_document.frame().size.height)
        } else if follows_active {
            Some(top + 8.0)
        } else {
            None
        };
        if let Some(top) = scroll_top {
            refresh_document.scrollPoint(CGPoint {
                x: 0.0,
                y: (top - viewport_height).max(0.0),
            });
        }

        update_label(&setup_notice, progress.creation_blocker.as_deref().unwrap_or("Start here in your normal account. A separate macOS account keeps agent applications off your desktop. A new account needs one manual login for macOS setup. Follow these steps in order; authentication and setup waits have no time limit."));
        for (index, (label, (title, _))) in step_labels.iter().zip(titles.iter()).enumerate() {
            update_label(label, &step.row_title(index, title));
            let font = if index == active {
                NSFont::boldSystemFontOfSize(14.0)
            } else {
                NSFont::systemFontOfSize(13.0)
            };
            let color = if index == active {
                NSColor::controlAccentColor()
            } else {
                NSColor::secondaryLabelColor()
            };
            label.setFont(Some(&font));
            label.setTextColor(Some(&color));
        }
        update_label(&sign_in, &sign_in_instructions(&name));
        update_label(&returning, &start_instructions(&name));
        let starting = progress
            .start_account
            .as_ref()
            .is_some_and(|start| start.name == name);
        let can_continue = progress.can_continue(&name);
        let can_create = progress.can_create();
        *start_account.borrow_mut() = progress.start_account;
        *resume_creation.borrow_mut() = progress.resume_creation;
        start_button.setEnabled(!progress.running && starting);
        continue_button.setEnabled(can_continue);
        begin.setEnabled(!progress.running);
        create.setEnabled(can_create);
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
    refresh();
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
    fn inline_status_and_global_permission_controls_do_not_overlap_steps() {
        for active in 0..5 {
            for controls_height in [60.0, 180.0] {
                for status_height in [24.0, 140.0, 400.0] {
                    let layout =
                        InlineStatusLayout::new(active, controls_height, status_height, 120.0);
                    let body_bottom = if active == 3 {
                        layout.controls_y()
                    } else {
                        layout.guide_y([404.0, 290.0, 200.0, 130.0, 0.0][active])
                    };
                    assert!(layout.status_y() + status_height < body_bottom);
                    assert!(layout.status_y() > 6.0);
                    if active < 4 {
                        let next_title = [370.0, 262.0, 172.0, 96.0][active];
                        assert!(layout.status_y() > layout.guide_y(next_title) + 22.0);
                    }
                    assert!(layout.controls_y() + controls_height < layout.guide_y(124.0));
                    assert!(layout.controls_y() > layout.guide_y(96.0) + 22.0);
                    assert!(layout.guide_y(472.0) + 60.0 < layout.height());
                }
            }
        }
    }

    #[test]
    fn failed_permission_refresh_is_visible_without_erasing_setup_status() {
        let progress = SetupProgress {
            permission_check_error: Some("probe timed out".into()),
            permission_grants: None,
            creation_blocker: None,
            account: "account_a".into(),
            message: "Start the initialized desktop".into(),
            step: SetupStep::SignIn,
            running: false,
            can_cancel: true,
            start_account: Some(EnvironmentStart {
                name: "account_a".into(),
                needs_password: true,
            }),
            resume_creation: None,
        };
        let message = progress.display_message("account_a");
        assert!(
            message.contains("probe timed out")
                && message.contains("Start the initialized desktop")
        );
        assert!(progress.permission_display([false, false]).1);
        assert!(
            progress
                .display_message("account_b")
                .contains("probe timed out")
        );
    }

    #[test]
    fn app_permissions_do_not_depend_on_account_selection_or_setup_phase() {
        let mut progress = SetupProgress {
            permission_check_error: None,
            permission_grants: None,
            creation_blocker: Some("enrolled".into()),
            account: "account_a".into(),
            message: String::new(),
            step: SetupStep::SignIn,
            running: false,
            can_cancel: true,
            start_account: Some(EnvironmentStart {
                name: "account_a".into(),
                needs_password: true,
            }),
            resume_creation: None,
        };
        assert_eq!(
            progress.permission_display([true, false]),
            ([true, false], true)
        );
        progress.account = "account_b".into();
        progress.start_account = None;
        progress.step = SetupStep::Ready;
        assert_eq!(
            progress.permission_display([true, false]),
            ([true, false], true)
        );
        progress.permission_grants = Some([true, true]);
        assert_eq!(
            progress.permission_display([false, false]),
            ([true, true], false)
        );
        assert!(
            !SetupStep::StartEnvironment
                .row_title(3, "Permissions")
                .contains('✓')
        );
        assert!(SetupStep::Ready.row_title(4, "Start").contains('✓'));
    }

    #[test]
    fn editing_selection_preserves_disable_failure_and_its_enrolled_account() {
        let progress = SetupProgress {
            permission_check_error: None,
            permission_grants: None,
            creation_blocker: Some("account_a is enrolled".into()),
            account: "account_a".into(),
            message: "Could not disable isolation: graphical domain is unavailable".into(),
            step: SetupStep::Account,
            running: false,
            can_cancel: true,
            start_account: None,
            resume_creation: None,
        };
        let message = progress.display_message("account_b");
        assert!(message.contains("Setup for account_a"));
        assert!(message.contains("Could not disable isolation: graphical domain is unavailable"));
        assert!(!message.contains("Create account…"));
        assert!(!progress.can_create());
        assert!(!progress.can_continue("account_b"));
    }

    #[test]
    fn enrolled_account_blocks_password_dialog_even_after_editing_selection() {
        let mut progress = SetupProgress {
            permission_check_error: None,
            permission_grants: None,
            creation_blocker: Some("account_a is enrolled. Disable isolation first.".into()),
            account: "account_a".into(),
            message: String::new(),
            step: SetupStep::Ready,
            running: false,
            can_cancel: true,
            start_account: None,
            resume_creation: None,
        };
        assert!(!progress.can_create());
        assert_eq!(progress.display_step("account_b"), SetupStep::Account);
        assert!(!progress.can_create());
        progress.isolation_disabled();
        assert!(progress.can_create());
        progress.running = true;
        assert!(!progress.can_create());
    }

    #[test]
    fn rejected_selection_cannot_rebind_another_accounts_ready_state() {
        let mut progress = SetupProgress {
            permission_check_error: None,
            permission_grants: None,
            creation_blocker: None,
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
