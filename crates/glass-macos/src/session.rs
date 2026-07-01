//! Session lock / display-sleep / no-session detection for `glass doctor` (and any
//! future capture-time diagnostic). An asleep or locked console display sets the
//! window server session's `CGSSessionScreenIsLocked` key; ScreenCaptureKit capture
//! and CGEvent input silently degrade while it's set — with no blank-frame-shaped
//! error of their own to catch — so `doctor` surfaces it directly rather than leaving
//! an agent to debug a mysterious capture failure. `caffeinate -d`, run in the console
//! session, keeps the display awake without needing sudo.
//!
//! There's a second, distinct failure mode: `CGSessionCopyCurrentDictionary` returns
//! NULL when there's no active graphical (Aqua) login session on the *console* at
//! all — before anyone has logged in, after everyone has logged out, or on a headless
//! boot with auto-login off. Note this is a console-wide fact, not one scoped to the
//! calling process: it isn't "returns NULL whenever you're on a bare SSH shell" — a
//! bare-SSH `doctor` run on a box where an account IS logged in at the console still
//! sees that account's real (locked/unlocked) session dict, same as a GUI-launched
//! process would (verified by hand: `doctor` run over plain SSH against the `mini`
//! host, logged-in-but-unattended, reported the true unlocked state, not NoSession).
//! What NULL actually signals is "capture/input have nothing to attach to *regardless
//! of who calls this*" — which is not "a present, unlocked session" and so
//! [`SessionState`] keeps it as its own variant rather than folding it into
//! `Unlocked` the way an early version of this predicate did.

use objc2_core_foundation::{CFBoolean, CFDictionary, CFNumber, CFRetained, CFString, CFType};

const SCREEN_IS_LOCKED_KEY: &str = "CGSSessionScreenIsLocked";

// `CGSessionCopyCurrentDictionary` is a long-standing (if formally undocumented)
// CoreGraphics entry point — the standard way idle-time/lock-state tools read the
// window server's session dictionary. `objc2-core-graphics` doesn't bind it, so it's
// declared manually here, the same way `permissions.rs` declares
// `CGPreflightScreenCaptureAccess`/`AXIsProcessTrusted`.
//
// The return type is written as the parameterized `*mut CFDictionary<CFString,
// CFType>` rather than an untyped `*mut c_void` (this is the first FFI declaration in
// the crate to do so). That's sound: `CFDictionary<K, V>` is a zero-cost phantom
// wrapper around the same `CFDictionaryRef` C ABI regardless of `K`/`V` — the type
// parameters only narrow what `objc2-core-foundation`'s safe accessors (`get`,
// `from_slices`, ...) let Rust assume about the *values*, they don't change the
// pointer's layout or the call's ABI. `CFType` is `objc2-core-foundation`'s top type
// (every CF object downcasts to it), so `CFType` values are always a safe upper bound
// for "whatever this dictionary actually holds" — declaring the precise
// `CFDictionary<CFString, CFType>` here (instead of a bare pointer + a manual cast at
// every call site) pushes the unsafety to one declaration instead of many.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGSessionCopyCurrentDictionary() -> *mut CFDictionary<CFString, CFType>;
}

/// The three states the console session can be in. Collapsing all of these to a
/// single `bool` (as an earlier version of this predicate did) hides
/// [`SessionState::NoSession`] behind "unlocked", which would let a box with nobody
/// logged in at the console sail through `doctor` with a clean bill of health while
/// capture/input silently fail. Keeping the three states distinct lets callers —
/// [`session_state`]'s only consumer today is `glass-mcp`'s doctor — report each
/// honestly.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SessionState {
    /// A console session is logged in and its display is unlocked/awake.
    Unlocked,
    /// A console session is logged in but its display is locked/asleep
    /// (`CGSSessionScreenIsLocked` is set) — recoverable in place with `caffeinate -d`.
    Locked,
    /// No account is logged in at the console at all (before first login, after
    /// logging out, or a headless boot with auto-login off) — capture/input need an
    /// actual GUI login, not just an unlocked one.
    NoSession,
}

/// True if the console session's display is locked/asleep. Convenience wrapper over
/// [`session_state`] for callers that only care about the locked/not-locked
/// distinction; note it reports [`SessionState::NoSession`] as "not locked" — callers
/// that need to tell "nobody's logged in at the console" apart from "logged in and
/// unlocked" (e.g. `doctor`) should match on [`session_state`] directly instead.
pub fn session_locked() -> bool {
    matches!(session_state(), SessionState::Locked)
}

/// The full three-way session state (see [`SessionState`]).
pub fn session_state() -> SessionState {
    dict_reports_state(copy_session_dictionary().as_deref())
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

/// Pure: map a session dictionary (or its absence) to a [`SessionState`]. Fully safe
/// (no OS calls), so it's unit-tested directly against synthetic dictionaries and a
/// missing one. A `None` dictionary is [`SessionState::NoSession`] (see the module
/// docs) — everything else means *some* window-server session is attached, so it's
/// `Locked`/`Unlocked` depending on the key. Apple's documented type for the key is
/// `CFBoolean`; a `CFNumber` fallback costs nothing and keeps this robust if that ever
/// changes. A value of neither type is indeterminate — fail safe as `Locked` rather than
/// silently reporting `Unlocked`, since `doctor` treats `Unlocked` as "capture/input
/// should work" and a false-negative there just means a redundant "recover with
/// `caffeinate -d`" hint, while a false-positive `Unlocked` would hide a real capture/
/// input failure behind a clean bill of health.
fn dict_reports_state(dict: Option<&CFDictionary<CFString, CFType>>) -> SessionState {
    let Some(dict) = dict else { return SessionState::NoSession };
    let key = CFString::from_str(SCREEN_IS_LOCKED_KEY);
    let Some(value) = dict.get(&key) else { return SessionState::Unlocked };
    let locked = if let Some(b) = value.downcast_ref::<CFBoolean>() {
        b.as_bool()
    } else if let Some(n) = value.downcast_ref::<CFNumber>() {
        n.as_i64().unwrap_or(0) != 0
    } else {
        true
    };
    if locked { SessionState::Locked } else { SessionState::Unlocked }
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
    fn null_dict_is_no_session() {
        // The NULL-dict case: nobody logged in at the console at all — distinct from
        // a present-but-unlocked session (see the module docs for why this is a
        // console-wide fact, not a "ran over SSH" one).
        assert_eq!(dict_reports_state(None), SessionState::NoSession);
    }

    #[test]
    fn missing_key_is_unlocked() {
        let d = CFDictionary::<CFString, CFType>::empty();
        assert_eq!(dict_reports_state(Some(&d)), SessionState::Unlocked);
    }

    #[test]
    fn true_boolean_value_is_locked() {
        let v: &CFType = unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue");
        let d = dict_with(SCREEN_IS_LOCKED_KEY, v);
        assert_eq!(dict_reports_state(Some(&d)), SessionState::Locked);
    }

    #[test]
    fn false_boolean_value_is_unlocked() {
        let v: &CFType = unsafe { kCFBooleanFalse }.expect("kCFBooleanFalse");
        let d = dict_with(SCREEN_IS_LOCKED_KEY, v);
        assert_eq!(dict_reports_state(Some(&d)), SessionState::Unlocked);
    }

    #[test]
    fn an_unrelated_key_does_not_count_as_locked() {
        let v: &CFType = unsafe { kCFBooleanTrue }.expect("kCFBooleanTrue");
        let d = dict_with("SomeOtherSessionKey", v);
        assert_eq!(dict_reports_state(Some(&d)), SessionState::Unlocked);
    }

    #[test]
    fn unrecognized_value_type_fails_safe_as_locked() {
        // If the key's value is ever neither `CFBoolean` nor `CFNumber` (Apple's
        // documented type, plus our defensive fallback), that's an indeterminate read —
        // not evidence the session is unlocked. Fail safe as `Locked` rather than
        // silently reporting `Unlocked`, which would tell `doctor` capture/input should
        // work when it's genuinely unknown whether they will. A `CFString` stands in for
        // "some type we don't recognize" (`CFString` derefs to `CFType`, same as every
        // other CF type here).
        let v = CFString::from_str("not a bool or a number");
        let d = dict_with(SCREEN_IS_LOCKED_KEY, &v);
        assert_eq!(dict_reports_state(Some(&d)), SessionState::Locked);
    }

    #[test]
    fn session_locked_matches_session_state() {
        // Thin-wrapper invariant: `session_locked()` is exactly `session_state() ==
        // Locked`, for whatever state this box happens to be in.
        assert_eq!(session_locked(), session_state() == SessionState::Locked);
    }

    #[test]
    fn session_state_runs_the_real_ffi_path_without_panicking() {
        // Environment-gated like `permissions::preflight_matches_the_two_predicates` —
        // there's no independent oracle for the live session state on whatever box this
        // runs on, so this just proves the real FFI path (framework call + CFRetained +
        // dict lookup) completes without panicking or leaking a null deref.
        let _ = session_state();
    }
}
