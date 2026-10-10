//! Read-only observation of a direct child, with no capture, activation or input.

use std::ffi::c_void;
use std::ptr::NonNull;
use std::sync::{Arc, Mutex};

use glass_core::platform::{AppSpec, ProtectedHostPath};
use glass_core::{Deadline, GlassError, Result};
use objc2_app_kit::NSWorkspace;
use objc2_application_services::{AXError, AXUIElement};
use objc2_core_foundation::{CFArray, CFBoolean, CFRetained, CFString, CFType, CGRect};

use super::native::{Library, NativeApi};
use super::{ProcessIdentity, refusal, require_same_process};
use crate::axwindow::{AxMessageScope, SystemWideAxMessaging};

const MAX_NODES: usize = 128;
const MAX_DEPTH: usize = 16;

#[repr(C)]
#[derive(Default, PartialEq, Eq)]
struct Serial {
    high: u32,
    low: u32,
}
const _: () = assert!(std::mem::size_of::<Serial>() == 8);
const _: () = assert!(std::mem::size_of::<CGRect>() == 32);

type MainConnection = unsafe extern "C" fn() -> i32;
type WindowOwner = unsafe extern "C" fn(i32, u32, *mut i32) -> i32;
type ConnectionPid = unsafe extern "C" fn(i32, *mut i32) -> i32;
type ConnectionSerial = unsafe extern "C" fn(i32, *mut Serial) -> i32;
type SerialPid = unsafe extern "C" fn(*const Serial, *mut i32) -> i32;
type WindowBounds = unsafe extern "C" fn(i32, u32, *mut CGRect) -> i32;
type AxWindowId = unsafe extern "C" fn(*const AXUIElement, *mut u32) -> AXError;

struct WindowApi {
    main_connection: MainConnection,
    window_owner: WindowOwner,
    connection_pid: ConnectionPid,
    connection_serial: ConnectionSerial,
    serial_pid: SerialPid,
    window_bounds: WindowBounds,
    ax_window_id: AxWindowId,
    _libraries: [Library; 3],
}

impl WindowApi {
    fn load() -> Result<Self> {
        let sky = Library::open(c"/System/Library/PrivateFrameworks/SkyLight.framework/SkyLight")?;
        let ax = Library::open(
            c"/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices",
        )?;
        let services = Library::open(c"/System/Library/Frameworks/ApplicationServices.framework/Frameworks/HIServices.framework/HIServices")?;
        // SAFETY: fixed C ABI signatures and symbol names with all library handles retained.
        Ok(unsafe {
            Self {
                main_connection: std::mem::transmute::<*mut c_void, MainConnection>(
                    sky.symbol(c"SLSMainConnectionID")?.as_ptr(),
                ),
                window_owner: std::mem::transmute::<*mut c_void, WindowOwner>(
                    sky.symbol(c"SLSGetWindowOwner")?.as_ptr(),
                ),
                connection_pid: std::mem::transmute::<*mut c_void, ConnectionPid>(
                    sky.symbol(c"SLSConnectionGetPID")?.as_ptr(),
                ),
                connection_serial: std::mem::transmute::<*mut c_void, ConnectionSerial>(
                    sky.symbol(c"SLSGetConnectionPSN")?.as_ptr(),
                ),
                serial_pid: std::mem::transmute::<*mut c_void, SerialPid>(
                    services.symbol(c"GetProcessPID")?.as_ptr(),
                ),
                window_bounds: std::mem::transmute::<*mut c_void, WindowBounds>(
                    sky.symbol(c"SLSGetWindowBounds")?.as_ptr(),
                ),
                ax_window_id: std::mem::transmute::<*mut c_void, AxWindowId>(
                    ax.symbol(c"_AXUIElementGetWindow")?.as_ptr(),
                ),
                _libraries: [sky, ax, services],
            }
        })
    }

    fn window_id(&self, scope: &mut AxMessageScope<'_, '_>, element: &AXUIElement) -> Result<u32> {
        let mut id = 0;
        let error = scope.message("read exact AX window ID", || {
            // SAFETY: live AX reference and writable u32 out-parameter under the AX timeout owner.
            Ok(unsafe { (self.ax_window_id)(element, &mut id) })
        })?;
        if error != AXError::Success || id == 0 {
            return Err(refusal("exact AX window ID is unavailable"));
        }
        Ok(id)
    }

    fn owner_and_bounds(&self, id: u32, expected_pid: i32) -> Result<(i32, [u32; 2], CGRect)> {
        let (mut owner, mut pid, mut serial_pid) = (0, 0, 0);
        let mut serial = Serial::default();
        let mut bounds = CGRect::default();
        // SAFETY: retained native getters and correctly sized initialized out-parameters.
        unsafe {
            let client = (self.main_connection)();
            if id == 0
                || client <= 0
                || (self.window_owner)(client, id, &mut owner) != 0
                || owner <= 0
            {
                return Err(refusal("WindowServer window owner is unavailable"));
            }
            if (self.connection_pid)(owner, &mut pid) != 0
                || pid != expected_pid
                || (self.connection_serial)(owner, &mut serial) != 0
                || serial == Serial::default()
                || (self.serial_pid)(&serial, &mut serial_pid) != 0
                || serial_pid != pid
            {
                return Err(refusal("WindowServer owner does not match the owned child"));
            }
            if (self.window_bounds)(client, id, &mut bounds) != 0 {
                return Err(refusal("WindowServer geometry is unavailable"));
            }
        }
        validate_rect(bounds)?;
        Ok((owner, [serial.high, serial.low], bounds))
    }
}

/// An observed AX node, not a qualified control or an instruction to perform its action.
#[derive(Debug)]
pub struct ControlObservation {
    pub role: String,
    pub identifier: Option<String>,
    pub enabled: Option<bool>,
    pub global_frame_points: [f64; 4],
    pub depth: usize,
    pub parent: Option<usize>,
}

/// Current owner/AX facts; these never constitute an input-admission lease.
#[derive(Debug)]
pub struct TargetObservation {
    pub process: ProcessIdentity,
    pub window_id: u32,
    pub owner_connection: i32,
    pub owner_psn: [u32; 2],
    pub global_frame_points: [f64; 4],
    pub foreground: ProcessIdentity,
    pub controls: Vec<ControlObservation>,
    pub window_lifetime_verified: bool,
    pub configuration_verified: bool,
    pub pixel_geometry_verified: bool,
    pub input_admitted: bool,
}

/// Owns one direct child and its selected AX reference; it never adopts another process/window.
pub struct OwnedTarget {
    launch: Option<crate::process::Launch>,
    kernel: NativeApi,
    windows: WindowApi,
    process: ProcessIdentity,
    foreground: ProcessIdentity,
    selected: Option<CFRetained<AXUIElement>>,
    invalidated: bool,
}

impl OwnedTarget {
    /// Launch without a build, bundle adoption, clipboard shim or permission request.
    pub fn launch(spec: &AppSpec, protected_paths: &[ProtectedHostPath]) -> Result<Self> {
        if spec.build.is_some() || spec.window_hint.is_some() {
            return Err(refusal(
                "target inspection requires a direct launch without build or window hint",
            ));
        }
        require_console_and_permission()?;
        let kernel = NativeApi::load()?;
        let windows = WindowApi::load()?;
        let foreground = foreground(&kernel)?;
        let logs = Arc::new(Mutex::new(Vec::new()));
        let mut launch = crate::process::spawn_window_target(spec, logs, protected_paths)?;
        let identity = i32::try_from(launch.child.id())
            .map_err(|_| refusal("owned child PID is invalid"))
            .and_then(|pid| kernel.process_identity(pid));
        match identity {
            Ok(process) => Ok(Self {
                launch: Some(launch),
                kernel,
                windows,
                process,
                foreground,
                selected: None,
                invalidated: false,
            }),
            Err(error) => match stop_launch(&mut launch) {
                Ok(()) => Err(error.after_dispatch()),
                Err(cleanup) => Err(GlassError::cleanup_failed(
                    "identify owned target",
                    error.after_dispatch(),
                    cleanup,
                )),
            },
        }
    }

    /// Read the sole reported AX window and its bounded subtree without choosing a new window.
    pub fn observe(&mut self, deadline: Deadline) -> Result<TargetObservation> {
        if self.invalidated {
            return Err(refusal("owned target inspection was invalidated"));
        }
        let result = self.observe_inner(deadline);
        if let Err(error) = &result {
            let awaiting_first_window =
                self.selected.is_none() && matches!(error.cause(), GlassError::WindowNotFound);
            if !awaiting_first_window {
                self.invalidated = true;
                self.selected = None;
            }
        }
        result
    }

    fn observe_inner(&mut self, deadline: Deadline) -> Result<TargetObservation> {
        self.check_context()?;
        let kernel = &self.kernel;
        let windows = &self.windows;
        let process = self.process;
        let previous = self.selected.as_deref();
        let (window, id, connection, psn, rect, controls) =
            glass_a11y_macos::messaging_timeout::with_read_only_by(
                &SystemWideAxMessaging,
                deadline,
                |scope| {
                    // SAFETY: positive kernel-verified PID; the created application reference is owned.
                    let app = unsafe { AXUIElement::new_application(process.pid) };
                    let window = sole_window(scope, &app)?;
                    if previous.is_some_and(|previous| previous != &*window) {
                        return Err(refusal("selected AX window was replaced"));
                    }
                    require_pid(scope, &window, process.pid)?;
                    let id = windows.window_id(scope, &window)?;
                    let (connection, psn, rect) = windows.owner_and_bounds(id, process.pid)?;
                    let ax_rect = ax_rect(scope, &window)?;
                    if rect_values(rect)
                        .iter()
                        .zip(rect_values(ax_rect))
                        .any(|(a, b)| (a - b).abs() > 0.5)
                    {
                        return Err(refusal("AX and WindowServer geometry disagree"));
                    }
                    let mut controls = Vec::new();
                    let mut seen = Vec::new();
                    read_tree(
                        scope,
                        &window,
                        &window,
                        process.pid,
                        (0, None),
                        &mut controls,
                        &mut seen,
                    )?;
                    let current = sole_window(scope, &app)?;
                    if current != window || windows.window_id(scope, &window)? != id {
                        return Err(refusal("selected window changed during observation"));
                    }
                    let current_owner = windows.owner_and_bounds(id, process.pid)?;
                    if current_owner != (connection, psn, rect) {
                        return Err(refusal(
                            "window ownership or geometry changed during observation",
                        ));
                    }
                    require_same_process(process, kernel.process_identity(process.pid)?)?;
                    Ok((window, id, connection, psn, rect, controls))
                },
            )?;
        self.check_context().map_err(GlassError::after_dispatch)?;
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed("target observation"));
        }
        self.selected = Some(window);
        Ok(TargetObservation {
            process,
            window_id: id,
            owner_connection: connection,
            owner_psn: psn,
            global_frame_points: rect_values(rect),
            foreground: self.foreground,
            controls,
            window_lifetime_verified: false,
            configuration_verified: false,
            pixel_geometry_verified: false,
            input_admitted: false,
        })
    }

    fn check_context(&mut self) -> Result<()> {
        let launch = self
            .launch
            .as_mut()
            .ok_or_else(|| refusal("owned target is stopped"))?;
        if launch
            .child
            .try_wait()
            .map_err(|_| refusal("owned child liveness is unavailable"))?
            .is_some()
        {
            return Err(refusal("owned child exited"));
        }
        require_console_and_permission()?;
        require_same_process(
            self.process,
            self.kernel.process_identity(self.process.pid)?,
        )?;
        require_same_process(self.foreground, foreground(&self.kernel)?)
    }

    /// Stop only the owned child and end its readers, reporting an unresolved cleanup.
    pub fn stop(&mut self) -> Result<()> {
        self.selected = None;
        self.invalidated = true;
        let Some(mut launch) = self.launch.take() else {
            return Ok(());
        };
        match stop_launch(&mut launch) {
            Ok(()) => Ok(()),
            Err(error) => {
                self.launch = Some(launch);
                Err(error)
            }
        }
    }
}

fn stop_launch(launch: &mut crate::process::Launch) -> Result<()> {
    crate::process::terminate(&mut launch.child);
    match launch.child.try_wait() {
        Ok(Some(_)) => Ok(()),
        _ => Err(GlassError::Backend(
            "owned target cleanup did not confirm exit".into(),
        )),
    }
}

impl Drop for OwnedTarget {
    fn drop(&mut self) {
        if let Err(error) = self.stop() {
            eprintln!("glass-macos target inspection cleanup: {error}");
        }
    }
}

fn require_console_and_permission() -> Result<()> {
    if !crate::permissions::accessibility_granted() {
        return Err(GlassError::PermissionDenied {
            which: "Accessibility".into(),
            remedy: crate::permissions::accessibility_remedy().into(),
        }
        .before_dispatch());
    }
    if !matches!(
        crate::session::session_state(),
        crate::session::SessionState::Unlocked
    ) {
        return Err(refusal("an unlocked console session is required"));
    }
    Ok(())
}

fn foreground(kernel: &NativeApi) -> Result<ProcessIdentity> {
    let app = NSWorkspace::sharedWorkspace()
        .frontmostApplication()
        .ok_or_else(|| refusal("foreground owner is unavailable"))?;
    kernel.process_identity(app.processIdentifier())
}

fn copy(
    scope: &mut AxMessageScope<'_, '_>,
    el: &AXUIElement,
    name: &str,
) -> Result<Option<CFRetained<CFType>>> {
    let attr = CFString::from_str(name);
    let mut raw = std::ptr::null();
    let error = scope.message(name, || {
        // SAFETY: live AX object and correctly typed writable CFTypeRef out-parameter.
        Ok(unsafe { el.copy_attribute_value(&attr, NonNull::from(&mut raw)) })
    })?;
    if error == AXError::AttributeUnsupported || error == AXError::NoValue {
        return Ok(None);
    }
    if error != AXError::Success {
        return Err(GlassError::Backend(format!(
            "read-only AX attribute {name} failed: {error:?}"
        ))
        .before_dispatch());
    }
    let raw =
        NonNull::new(raw.cast_mut()).ok_or_else(|| refusal("AX returned a null attribute"))?;
    // SAFETY: a successful Copy call transfers one owned reference of the verified CF type.
    Ok(Some(unsafe { CFRetained::from_raw(raw) }))
}

fn elements(
    scope: &mut AxMessageScope<'_, '_>,
    el: &AXUIElement,
    name: &str,
    required: bool,
) -> Result<Vec<CFRetained<AXUIElement>>> {
    let Some(value) = copy(scope, el, name)? else {
        return if required {
            Err(refusal("required AX element array is absent"))
        } else {
            Ok(Vec::new())
        };
    };
    let array = value
        .downcast::<CFArray>()
        .map_err(|_| refusal("AX element list has the wrong type"))?;
    if array.len() > MAX_NODES {
        return Err(refusal("AX element list exceeds the observation bound"));
    }
    // SAFETY: AX's copied attribute arrays contain immutable retained CFType references.
    let array: CFRetained<CFArray<CFType>> = unsafe { CFRetained::cast_unchecked(array) };
    array
        .iter()
        .map(|item| {
            item.downcast::<AXUIElement>()
                .map_err(|_| refusal("AX element list contains a foreign type"))
        })
        .collect()
}

fn sole_window(
    scope: &mut AxMessageScope<'_, '_>,
    app: &AXUIElement,
) -> Result<CFRetained<AXUIElement>> {
    let mut windows = elements(scope, app, "AXWindows", true)?;
    if windows.is_empty() {
        return Err(GlassError::WindowNotFound.before_dispatch());
    }
    if windows.len() != 1 {
        return Err(refusal("owned child has competing AX windows"));
    }
    Ok(windows.remove(0))
}

fn require_pid(scope: &mut AxMessageScope<'_, '_>, el: &AXUIElement, expected: i32) -> Result<()> {
    let mut pid = 0;
    let error = scope.message("read AX owner PID", || {
        // SAFETY: live AX element and writable pid_t out-parameter.
        Ok(unsafe { el.pid(NonNull::from(&mut pid)) })
    })?;
    if error != AXError::Success || pid != expected {
        return Err(refusal("AX element belongs to a foreign process"));
    }
    Ok(())
}

fn string(
    scope: &mut AxMessageScope<'_, '_>,
    el: &AXUIElement,
    name: &str,
) -> Result<Option<String>> {
    copy(scope, el, name)?
        .map(|value| {
            let string = value
                .downcast::<CFString>()
                .map_err(|_| refusal("AX string has the wrong type"))?;
            if string.length() > 256 {
                return Err(refusal(
                    "AX identifier or role exceeds the observation bound",
                ));
            }
            Ok(string.to_string())
        })
        .transpose()
}

fn ax_rect(scope: &mut AxMessageScope<'_, '_>, el: &AXUIElement) -> Result<CGRect> {
    let (x, y) = crate::axwindow::ax_position(scope, el).map_err(GlassError::before_dispatch)?;
    let (width, height) =
        crate::axwindow::ax_size(scope, el).map_err(GlassError::before_dispatch)?;
    let rect = CGRect::new(
        objc2_core_foundation::CGPoint { x, y },
        objc2_core_foundation::CGSize { width, height },
    );
    validate_rect(rect)?;
    Ok(rect)
}

fn rect_values(rect: CGRect) -> [f64; 4] {
    [
        rect.origin.x,
        rect.origin.y,
        rect.size.width,
        rect.size.height,
    ]
}

fn validate_rect(rect: CGRect) -> Result<()> {
    if !rect_values(rect).iter().all(|value| value.is_finite())
        || rect.size.width <= 0.0
        || rect.size.height <= 0.0
    {
        return Err(refusal("AX or WindowServer geometry is invalid"));
    }
    Ok(())
}

fn read_tree(
    scope: &mut AxMessageScope<'_, '_>,
    el: &AXUIElement,
    window: &AXUIElement,
    pid: i32,
    position: (usize, Option<usize>),
    controls: &mut Vec<ControlObservation>,
    seen: &mut Vec<CFRetained<AXUIElement>>,
) -> Result<()> {
    let (depth, parent) = position;
    if depth > MAX_DEPTH || controls.len() >= MAX_NODES {
        return Err(refusal("AX subtree is truncated by the observation bound"));
    }
    if seen.iter().any(|old| &**old == el) {
        return Err(refusal("AX subtree contains repeated or cyclic elements"));
    }
    // SAFETY: el is a live reference; retain keeps the AX handle alive for cycle checks.
    seen.push(unsafe { CFRetained::retain(NonNull::from(el)) });
    require_pid(scope, el, pid)?;
    if depth > 0 {
        let owner = copy(scope, el, "AXWindow")?
            .ok_or_else(|| refusal("AX control window binding is absent"))?;
        let owner = owner
            .downcast::<AXUIElement>()
            .map_err(|_| refusal("AX control window has the wrong type"))?;
        if &*owner != window {
            return Err(refusal("AX control belongs to another window"));
        }
    }
    let role = string(scope, el, "AXRole")?.ok_or_else(|| refusal("AX role is absent"))?;
    let identifier = string(scope, el, "AXIdentifier")?;
    if depth == 0 && role != "AXWindow" {
        return Err(refusal("selected AX element is not a top-level window"));
    }
    let enabled = copy(scope, el, "AXEnabled")?
        .map(|value| {
            value
                .downcast::<CFBoolean>()
                .map(|value| value.value())
                .map_err(|_| refusal("AX enabled state has the wrong type"))
        })
        .transpose()?;
    let index = controls.len();
    controls.push(ControlObservation {
        role,
        identifier,
        enabled,
        global_frame_points: rect_values(ax_rect(scope, el)?),
        depth,
        parent,
    });
    for child in elements(scope, el, "AXChildren", false)? {
        read_tree(
            scope,
            &child,
            window,
            pid,
            (depth + 1, Some(index)),
            controls,
            seen,
        )?;
    }
    Ok(())
}
