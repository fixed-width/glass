//! Session lock / display-sleep detection for `glass doctor` (and any future
//! capture-time diagnostic). An asleep or locked console display sets the window
//! server session's `CGSSessionScreenIsLocked` key; ScreenCaptureKit capture and
//! CGEvent input silently degrade while it's set — with no blank-frame-shaped error of
//! their own to catch — so `doctor` surfaces it directly rather than leaving an agent
//! to debug a mysterious capture failure. `caffeinate -d`, run in the console session,
//! keeps the display awake without needing sudo.

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};

const SCREEN_IS_LOCKED_KEY: &str = "CGSSessionScreenIsLocked";

// `CGSessionCopyCurrentDictionary` is a long-standing (if formally undocumented)
// CoreGraphics entry point — the standard way idle-time/lock-state tools read the
// window server's session dictionary. `objc2-core-graphics` doesn't bind it, so it's
// declared manually here, the same way `permissions.rs` declares
// `CGPreflightScreenCaptureAccess`/`AXIsProcessTrusted`.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> *mut CFDictionary<CFString, CFType>;
}

/// True if the console session's display is asleep/locked. This predicate can only
/// prove "locked" — no session dictionary (no window-server session attached) or a
/// dictionary missing the key is reported as unlocked (`false`), matching
/// `CGSSessionScreenIsLocked`'s own convention of only appearing when the screen
/// actually is locked.
pub fn session_locked() -> bool {
    dict_reports_locked(copy_session_dictionary().as_deref())
}

/// `CGSessionCopyCurrentDictionary`, wrapped in a `CFRetained` so it's released
/// automatically. The call follows Core Foundation's Copy/Create ownership rule (an
/// already-retained ref on success), and returns NULL when no window-server session is
/// attached.
fn copy_session_dictionary() -> Option<CFRetained<CFDictionary<CFString, CFType>>> {
    // SAFETY: `CGSessionCopyCurrentDictionary` takes no arguments and only reads
    // window-server session state; a NULL return is documented behavior (no attached
    // session), not a precondition violation.
    let raw = unsafe { CGSessionCopyCurrentDictionary() };
    let nn = std::ptr::NonNull::new(raw)?;
    // SAFETY: the "Copy" naming convention means the caller already owns a +1
    // reference; `CFRetained::from_raw` takes ownership without an extra retain
    // (matches `axwindow.rs`'s `copy_attribute`).
    Some(unsafe { CFRetained::from_raw(nn) })
}

/// Pure: does this session dictionary report the screen as locked? Fully safe (no OS
/// calls), so it's unit-tested directly against synthetic dictionaries. Apple's
/// documented type for this key is `CFBoolean`; a `CFNumber` fallback costs nothing and
/// keeps this robust if that ever changes.
fn dict_reports_locked(dict: Option<&CFDictionary<CFString, CFType>>) -> bool {
    let Some(dict) = dict else { return false };
    let key = CFString::from_str(SCREEN_IS_LOCKED_KEY);
    let Some(value) = dict.get(&key) else { return false };
    if let Some(b) = value.downcast_ref::<CFBoolean>() {
        return b.as_bool();
    }
    if let Some(n) = value.downcast_ref::<CFNumber>() {
        return n.as_i64().unwrap_or(0) != 0;
    }
    false
}

#[cfg(test)]
mod tests {
    use objc2_core_foundation::{kCFBooleanFalse, kCFBooleanTrue};

    use super::*;

    fn dict_with(key: &str, value: &CFType) -> CFRetained<CFDictionary<CFString, CFType>> {
        let k = CFString::from_str(key);
        CFDictionary::from_slices(&[&k], &[value])
    }

    #[test]
    fn no_session_dictionary_is_unlocked() {
        assert!(!dict_reports_locked(None));
    }

    #[test]
    fn missing_key_is_unlocked() {
        let d = CFDictionary::<CFString, CFType>::empty();
        assert!(!dict_reports_locked(Some(&d)));
    }

    #[test]
    fn true_boolean_value_is_locked() {
        let v: &CFType = unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue");
        let d = dict_with(SCREEN_IS_LOCKED_KEY, v);
        assert!(dict_reports_locked(Some(&d)));
    }

    #[test]
    fn false_boolean_value_is_unlocked() {
        let v: &CFType = unsafe { kCFBooleanFalse }.expect("kCFBooleanFalse");
        let d = dict_with(SCREEN_IS_LOCKED_KEY, v);
        assert!(!dict_reports_locked(Some(&d)));
    }

    #[test]
    fn an_unrelated_key_does_not_count_as_locked() {
        let v: &CFType = unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue");
        let d = dict_with("SomeOtherSessionKey", v);
        assert!(!dict_reports_locked(Some(&d)));
    }

    #[test]
    fn session_locked_runs_the_real_ffi_path_without_panicking() {
        // Environment-gated like `permissions::preflight_matches_the_two_predicates` —
        // there's no independent oracle for the live lock state on whatever box this
        // runs on, so this just proves the real FFI path (framework call + CFRetained +
        // dict lookup) completes without panicking or leaking a null deref.
        let _ = session_locked();
    }
}
