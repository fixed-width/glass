//! Dynamically loaded, read-only process and event ABI checks. No posting API is loaded.

use std::ffi::{CStr, c_char, c_int, c_void};
use std::ptr::NonNull;

use objc2_core_foundation::CGPoint;
use objc2_core_graphics::{
    CGEvent, CGEventField, CGEventFlags, CGEventSource, CGEventSourceStateID, CGEventType,
    CGMouseButton, CGScrollEventUnit,
};

use glass_core::Result;

use super::{ProcessIdentity, process_identity, refusal, require_same_process};

const WINDOW_FIELD: CGEventField = CGEventField(51);
const CONNECTION_FIELD: CGEventField = CGEventField(52);

const _: () = assert!(std::mem::size_of::<CGPoint>() == 16);

type SetWindowLocation = unsafe extern "C" fn(*const CGEvent, CGPoint);
type GetWindowLocation = unsafe extern "C" fn(*const CGEvent) -> CGPoint;
type ProcessInfo = unsafe extern "C" fn(i32, i32, u64, *mut c_void, i32) -> i32;

#[link(name = "System")]
unsafe extern "C" {
    fn dlopen(path: *const c_char, mode: c_int) -> *mut c_void;
    fn dlsym(handle: *mut c_void, name: *const c_char) -> *mut c_void;
    fn dlclose(handle: *mut c_void) -> c_int;
}

struct Library(NonNull<c_void>);

impl Library {
    fn open(path: &CStr) -> Result<Self> {
        // SAFETY: NUL-terminated absolute framework path; RTLD_NOW | RTLD_LOCAL.
        NonNull::new(unsafe { dlopen(path.as_ptr(), 2 | 4) })
            .map(Self)
            .ok_or_else(|| refusal("required native library is unavailable"))
    }

    fn symbol(&self, name: &CStr) -> Result<NonNull<c_void>> {
        // SAFETY: this retained dlopen handle and the NUL-terminated symbol name are live.
        NonNull::new(unsafe { dlsym(self.0.as_ptr(), name.as_ptr()) })
            .ok_or_else(|| refusal("required native symbol is unavailable"))
    }
}

impl Drop for Library {
    fn drop(&mut self) {
        // SAFETY: each successful dlopen owns one reference; all symbols die before its owner.
        unsafe { dlclose(self.0.as_ptr()) };
    }
}

struct NativeApi {
    set_window_location: SetWindowLocation,
    get_window_location: GetWindowLocation,
    process_info: ProcessInfo,
    _libraries: [Library; 2],
}

impl NativeApi {
    fn load() -> Result<Self> {
        let graphics =
            Library::open(c"/System/Library/Frameworks/CoreGraphics.framework/CoreGraphics")?;
        let system = Library::open(c"/usr/lib/libSystem.B.dylib")?;
        // SAFETY: fixed C ABI names/signatures with library handles retained by NativeApi.
        let api = unsafe {
            Self {
                set_window_location: std::mem::transmute::<*mut c_void, SetWindowLocation>(
                    graphics.symbol(c"CGEventSetWindowLocation")?.as_ptr(),
                ),
                get_window_location: std::mem::transmute::<*mut c_void, GetWindowLocation>(
                    graphics.symbol(c"CGEventGetWindowLocation")?.as_ptr(),
                ),
                process_info: std::mem::transmute::<*mut c_void, ProcessInfo>(
                    system.symbol(c"proc_pidinfo")?.as_ptr(),
                ),
                _libraries: [graphics, system],
            }
        };
        Ok(api)
    }

    fn process_identity(&self, pid: i32) -> Result<ProcessIdentity> {
        if pid <= 0 {
            return Err(refusal("process identity requires a positive PID"));
        }
        // PROC_PIDUNIQIDENTIFIERINFO: XNU's fixed 56-byte, 8-byte-aligned record.
        #[repr(C, align(8))]
        struct Record([u8; 56]);
        let mut record = Record([0; 56]);
        // SAFETY: valid PID/flavor and writable output buffer with the C ABI's exact size/alignment.
        let count = unsafe { (self.process_info)(pid, 17, 0, record.0.as_mut_ptr().cast(), 56) };
        if count != 56 {
            return Err(refusal("kernel process identity response is incomplete"));
        }
        process_identity(pid, &record.0)
    }

    fn check_event_abi(&self) -> Result<()> {
        let source = CGEventSource::new(CGEventSourceStateID::Private)
            .ok_or_else(|| refusal("private event source is unavailable"))?;
        let source_id = CGEventSource::source_state_id(Some(&source));
        if source_id == CGEventSourceStateID::CombinedSessionState
            || source_id == CGEventSourceStateID::HIDSystemState
        {
            return Err(refusal("event source uses shared state"));
        }
        let down = CGEvent::new_mouse_event(
            Some(&source),
            CGEventType::LeftMouseDown,
            CGPoint { x: 19.25, y: 31.5 },
            CGMouseButton::Left,
        )
        .ok_or_else(|| refusal("unposted mouse event cannot be allocated"))?;
        let up = CGEvent::new_mouse_event(
            Some(&source),
            CGEventType::LeftMouseUp,
            CGPoint { x: 19.25, y: 31.5 },
            CGMouseButton::Left,
        )
        .ok_or_else(|| refusal("unposted mouse event cannot be allocated"))?;
        for (event, kind) in [
            (&*down, CGEventType::LeftMouseDown),
            (&*up, CGEventType::LeftMouseUp),
        ] {
            CGEvent::set_integer_value_field(Some(event), CGEventField::MouseEventClickState, 1);
            if CGEvent::r#type(Some(event)) != kind
                || CGEvent::location(Some(event)) != (CGPoint { x: 19.25, y: 31.5 })
                || CGEvent::integer_value_field(Some(event), CGEventField::MouseEventButtonNumber)
                    != 0
                || CGEvent::integer_value_field(Some(event), CGEventField::MouseEventClickState)
                    != 1
            {
                return Err(refusal("unposted left-click payload round trip failed"));
            }
        }
        let scroll =
            CGEvent::new_scroll_wheel_event2(Some(&source), CGScrollEventUnit::Line, 1, 3, 0, 0)
                .ok_or_else(|| refusal("unposted wheel event cannot be allocated"))?;
        if CGEvent::r#type(Some(&scroll)) != CGEventType::ScrollWheel
            || CGEvent::integer_value_field(Some(&scroll), CGEventField::ScrollWheelEventDeltaAxis1)
                != 3
            || CGEvent::integer_value_field(Some(&scroll), CGEventField::ScrollWheelEventDeltaAxis2)
                != 0
            || CGEvent::integer_value_field(
                Some(&scroll),
                CGEventField::ScrollWheelEventIsContinuous,
            ) != 0
        {
            return Err(refusal("unposted vertical line-scroll payload is invalid"));
        }
        for event in [down, up, scroll] {
            self.check_event(&event, source_id)?;
        }
        Ok(())
    }

    fn check_event(&self, event: &CGEvent, source_id: CGEventSourceStateID) -> Result<()> {
        CGEvent::set_flags(Some(event), CGEventFlags::empty());
        CGEvent::set_integer_value_field(Some(event), WINDOW_FIELD, 123);
        CGEvent::set_integer_value_field(Some(event), CONNECTION_FIELD, 456);
        CGEvent::set_integer_value_field(Some(event), CGEventField::EventSourceUserData, 789);
        let point = CGPoint { x: 11.25, y: 13.5 };
        // SAFETY: the C CGPoint setter/getter access only this live, owned, unposted CGEvent.
        let location = unsafe {
            (self.set_window_location)(event, point);
            (self.get_window_location)(event)
        };
        if location != point {
            return Err(refusal("unposted window-coordinate ABI round trip failed"));
        }
        if CGEvent::integer_value_field(Some(event), WINDOW_FIELD) != 123
            || CGEvent::integer_value_field(Some(event), CONNECTION_FIELD) != 456
        {
            return Err(refusal("unposted window-routing field round trip failed"));
        }
        if CGEvent::integer_value_field(Some(event), CGEventField::EventSourceUserData) != 789 {
            return Err(refusal("unposted event correlation tag round trip failed"));
        }
        // Private selects a new state table; its returned ID is opaque, not necessarily -1.
        if CGEvent::integer_value_field(Some(event), CGEventField::EventSourceStateID)
            != i64::from(source_id.0)
        {
            return Err(refusal(
                "unposted event private source state is unavailable",
            ));
        }
        if !CGEvent::flags(Some(event)).is_empty() {
            return Err(refusal("unposted event retains unexpected modifiers"));
        }
        Ok(())
    }
}

/// Diagnostic evidence only. This is neither session support nor an input-admission lease.
#[derive(Debug)]
pub struct Readiness {
    pub process: ProcessIdentity,
    pub accessibility_granted: bool,
    pub screen_recording_granted: bool,
    pub event_abi_verified: bool,
    pub input_admitted: bool,
}

/// Probe the current process and unposted events without capture, activation or input.
pub fn probe_readiness() -> Result<Readiness> {
    let api = NativeApi::load()?;
    let pid = i32::try_from(std::process::id()).map_err(|_| refusal("current PID is invalid"))?;
    let before = api.process_identity(pid)?;
    api.check_event_abi()?;
    require_same_process(before, api.process_identity(pid)?)?;
    Ok(Readiness {
        process: before,
        accessibility_granted: crate::permissions::accessibility_granted(),
        screen_recording_granted: crate::permissions::screen_recording_granted(),
        event_abi_verified: true,
        input_admitted: false,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_missing_symbol_or_library_refuses_before_any_native_actuation() {
        let library = Library::open(c"/usr/lib/libSystem.B.dylib").unwrap();
        let error = library
            .symbol(c"glass_deliberately_missing_window_input_symbol")
            .unwrap_err();
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
        assert!(Library::open(c"/glass-deliberately-missing-native-library").is_err());
    }

    #[test]
    fn native_kernel_identity_distinguishes_an_owned_child_and_refuses_an_exited_one() {
        let api = NativeApi::load().unwrap();
        let pid = i32::try_from(std::process::id()).unwrap();
        let current = api.process_identity(pid).unwrap();
        require_same_process(current, api.process_identity(pid).unwrap()).unwrap();
        let mut child = std::process::Command::new("/bin/sleep")
            .arg("60")
            .spawn()
            .unwrap();
        let child_pid = i32::try_from(child.id()).unwrap();
        let identity = api.process_identity(child_pid);
        let _ = child.kill();
        child.wait().unwrap();
        let identity = identity.unwrap();
        assert_eq!(identity.parent_lifetime, current.lifetime);
        assert_ne!(identity.lifetime, current.lifetime);
        if let Ok(reused) = api.process_identity(child_pid) {
            assert_ne!(reused.lifetime, identity.lifetime);
        }
    }

    #[test]
    fn native_exec_changes_the_image_version_without_changing_process_lifetime() {
        use std::io::Write;
        use std::process::Stdio;
        use std::time::{Duration, Instant};

        let api = NativeApi::load().unwrap();
        let mut child = std::process::Command::new("/bin/sh")
            .args(["-c", "read gate; exec /bin/sleep 60"])
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let pid = i32::try_from(child.id()).unwrap();
        let before = api.process_identity(pid);
        let write = child.stdin.take().unwrap().write_all(b"continue\n");
        let deadline = Instant::now() + Duration::from_secs(2);
        let mut after = None;
        if let Ok(before) = before.as_ref() {
            while Instant::now() < deadline {
                if let Ok(current) = api.process_identity(pid)
                    && current.exec_version != before.exec_version
                {
                    after = Some(current);
                    break;
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        let _ = child.kill();
        child.wait().unwrap();
        write.unwrap();
        let before = before.unwrap();
        let after = after.expect("owned child never completed exec");
        assert_eq!(before.lifetime, after.lifetime);
        assert_ne!(before.exec_version, after.exec_version);
        assert_ne!(before.executable_uuid, after.executable_uuid);
        assert!(require_same_process(before, after).is_err());
    }

    #[test]
    #[ignore = "requires a logged-in macOS WindowServer; creates unposted events only"]
    fn native_readiness_does_not_authorize_input() {
        let report = probe_readiness().unwrap();
        assert!(report.event_abi_verified);
        assert!(!report.input_admitted);
    }
}
