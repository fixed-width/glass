//! TCC preflight. glass needs two grants on macOS: **Screen Recording** (capture +
//! window titles) and **Accessibility** (window move/resize/focus + CGEvent input).
//! Neither can be force-granted (SIP/MDM can't allow Screen Recording); the product
//! holds them via a stable code-signed identity. Here we only *detect* a missing grant
//! and return an actionable error — never a blank frame.

use std::ptr::NonNull;

use objc2_core_foundation::{kCFBooleanTrue, CFBoolean, CFDictionary, CFRetained, CFString, CFType};

use glass_core::Result;

/// The two macOS TCC grants glass needs. A local enum keeps the permission names in
/// one place (no stringly-typed drift) and lets the preflight test assert the specific
/// missing grant. Converted to a `String` only at the `GlassError` boundary — the shared
/// `glass-core` error stays platform-agnostic.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Permission {
    ScreenRecording,
    Accessibility,
}

impl Permission {
    fn label(self) -> &'static str {
        match self {
            Permission::ScreenRecording => "Screen Recording",
            Permission::Accessibility => "Accessibility",
        }
    }
    fn remedy(self) -> &'static str {
        match self {
            Permission::ScreenRecording => "enable glass in System Settings > Privacy & Security > Screen Recording (run inside a logged-in session; grant persists for the signed binary)",
            Permission::Accessibility => "enable glass in System Settings > Privacy & Security > Accessibility",
        }
    }
    fn denied(self) -> glass_core::GlassError {
        glass_core::GlassError::PermissionDenied { which: self.label().into(), remedy: self.remedy().into() }
    }

    /// Like [`Permission::denied`], but appends a caller-supplied diagnostic (e.g. the
    /// raw `NSError` ScreenCaptureKit reported for a TCC decline) to the remedy text, so
    /// the agent sees both the actionable fix and the underlying OS-reported reason.
    /// `pub(crate)` (unlike `denied`) so other modules — e.g. `scwindow`'s
    /// `SCShareableContent` preflight — can reuse this wording instead of hand-rolling
    /// their own remedy string.
    pub(crate) fn denied_with_detail(self, detail: impl std::fmt::Display) -> glass_core::GlassError {
        glass_core::GlassError::PermissionDenied {
            which: self.label().into(),
            remedy: format!("{} (underlying error: {detail})", self.remedy()),
        }
    }
}

// Both are plain C functions; no objc2 needed for preflight.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    // Apple declares this post-10.15 API with C99 `bool`, guaranteed to be 0/1 — unlike
    // the legacy `Boolean`/`u8` ABI on `AXIsProcessTrusted` below.
    fn CGPreflightScreenCaptureAccess() -> bool;
}
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    // Apple declares this `Boolean` (= `unsigned char`), NOT C99 `_Bool`. Binding it as
    // `u8` and comparing `!= 0` avoids the Rust-`bool` validity invariant (only 0/1 are
    // legal bit patterns; any other byte would be instant UB), matching `accessibility-sys`.
    fn AXIsProcessTrusted() -> u8;
}

/// True if this process holds the Screen Recording grant. `pub` (not just
/// `pub(crate)`) so `glass-mcp`'s `doctor` can report the grant without duplicating
/// this FFI call.
pub fn screen_recording_granted() -> bool {
    // SAFETY: `CGPreflightScreenCaptureAccess` is a no-argument C predicate that only
    // reads this process's TCC state; it has no preconditions and no side effects.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// True if this process is trusted for Accessibility (AX APIs + CGEvent posting).
/// `pub` for the same reason as [`screen_recording_granted`].
pub fn accessibility_granted() -> bool {
    // SAFETY: `AXIsProcessTrusted` is a no-argument C predicate over this process's
    // trust state; no preconditions, no side effects. It returns `Boolean` (u8); any
    // nonzero value means trusted.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// The exact remedy text for a missing Screen Recording grant — shared by
/// [`preflight`]'s `PermissionDenied` error and `glass-mcp`'s `doctor`, so the two
/// never drift apart.
pub fn screen_recording_remedy() -> &'static str {
    Permission::ScreenRecording.remedy()
}

/// The exact remedy text for a missing Accessibility grant — see
/// [`screen_recording_remedy`].
pub fn accessibility_remedy() -> &'static str {
    Permission::Accessibility.remedy()
}

/// Fail fast with an actionable error if either grant is missing. Called at session
/// start before any capture/input is attempted.
pub(crate) fn preflight() -> Result<()> {
    if !screen_recording_granted() {
        return Err(Permission::ScreenRecording.denied());
    }
    if !accessibility_granted() {
        return Err(Permission::Accessibility.denied());
    }
    Ok(())
}

/// The System Settings deep-link for the Screen Recording privacy pane. Pure (no OS
/// call), so both `doctor`'s `remedy_action` and the `setup` command can use it without
/// either one triggering a permission prompt themselves.
pub fn screen_recording_pane_url() -> &'static str {
    "x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
}

/// The System Settings deep-link for the Accessibility privacy pane. See
/// [`screen_recording_pane_url`].
pub fn accessibility_pane_url() -> &'static str {
    "x-apple.systempreferences:com.apple.preference.security?Privacy_Accessibility"
}

/// Open a System Settings pane (or any URL) via the `open` command-line tool. Returns an
/// error the caller can surface to the agent; never panics.
pub fn open_pane(url: &str) -> Result<()> {
    let status = std::process::Command::new("open")
        .arg(url)
        .status()
        .map_err(|e| glass_core::GlassError::Backend(format!("open {url}: {e}")))?;
    if status.success() {
        Ok(())
    } else {
        Err(glass_core::GlassError::Backend(format!("open {url} exited {status}")))
    }
}

// --- Guided setup: prompting requests --------------------------------------------------
//
// Everything above this point only *reads* TCC state (`screen_recording_granted`,
// `accessibility_granted`, `preflight`) — safe to call from `doctor` on every run. The two
// functions below are the opposite: they actively trigger the OS consent flow (a system
// dialog, on first request). They exist only for the future interactive `setup` command
// and must never be called from `preflight`/`doctor`, which must stay non-prompting.

#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    // Triggers the Screen Recording consent flow (adds glass to the privacy pane if it
    // isn't already listed, and shows the system dialog on first request) and returns
    // the current grant state. Post-10.15 API, C99 `bool` ABI — like
    // `CGPreflightScreenCaptureAccess` above.
    fn CGRequestScreenCaptureAccess() -> bool;
}
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    // With an options dict carrying `kAXTrustedCheckOptionPrompt = true`, shows the
    // Accessibility consent dialog (when not already trusted) and returns the current
    // trust state. `Boolean` (u8), not C99 `bool` — see `AXIsProcessTrusted` above for
    // why that distinction matters for the return-value binding.
    fn AXIsProcessTrustedWithOptions(options: *const CFDictionary) -> u8;
}

/// Trigger the Screen Recording consent flow (shows the system dialog and adds glass to
/// the Privacy & Security > Screen Recording pane) and report whether the grant is
/// currently held. Only ever called from the `setup` command — `preflight`/`doctor` must
/// never prompt; they call [`screen_recording_granted`] instead.
pub fn request_screen_recording() -> bool {
    // SAFETY: no-argument C call. Unlike `CGPreflightScreenCaptureAccess`, this one's
    // documented behavior is to prompt; it has no other preconditions and its only
    // effects are the system dialog plus reading this process's TCC state.
    unsafe { CGRequestScreenCaptureAccess() }
}

/// Trigger the Accessibility consent dialog (via `kAXTrustedCheckOptionPrompt`) and
/// report whether this process is currently trusted. Only ever called from the `setup`
/// command — `preflight`/`doctor` must never prompt; they call [`accessibility_granted`]
/// instead.
pub fn request_accessibility() -> bool {
    let key = CFString::from_str("AXTrustedCheckOptionPrompt");
    // SAFETY: `kCFBooleanTrue` is a framework-owned singleton, always live for the
    // process's lifetime; reading the extern static is a plain global read (same idiom
    // `axwindow::ax_set_main` and `session.rs`'s tests use).
    let true_boolean: Option<&CFBoolean> = unsafe { kCFBooleanTrue };
    // `None` is not a real-world case (the constant is always present on real
    // CoreFoundation builds) but falling back to the non-prompting predicate instead of
    // panicking keeps this function total.
    let Some(true_boolean) = true_boolean else { return accessibility_granted() };
    let prompt: &CFType = true_boolean;
    let dict: CFRetained<CFDictionary<CFString, CFType>> = CFDictionary::from_slices(&[&key], &[prompt]);
    // SAFETY: `AXIsProcessTrustedWithOptions` takes a CFDictionaryRef; `dict` stays alive
    // for the whole call (it is dropped only when this function returns, after the FFI
    // call has returned), and the only effects are the documented consent prompt plus
    // reading this process's trust state (`Boolean`/u8 return, same ABI reasoning as
    // `AXIsProcessTrusted` above).
    unsafe { AXIsProcessTrustedWithOptions(NonNull::from(&*dict).cast().as_ptr()) != 0 }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::GlassError;

    #[test]
    fn preflight_matches_the_two_predicates() {
        // On a box where grants are present, preflight is Ok; where absent, it errors with
        // the missing permission named. Either way the predicates and preflight agree.
        let sr = screen_recording_granted();
        let ax = accessibility_granted();
        match preflight() {
            Ok(()) => assert!(sr && ax, "preflight Ok but a predicate was false"),
            Err(GlassError::PermissionDenied { which, .. }) => {
                assert!(!sr || !ax, "preflight denied but both predicates true");
                // preflight checks Screen Recording first, so the specific predicate
                // that failed pins which grant the error must name.
                let expected = if !sr { Permission::ScreenRecording } else { Permission::Accessibility };
                assert_eq!(which, expected.label());
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }

    #[test]
    fn screen_recording_pane_url_points_at_the_screen_capture_anchor() {
        assert!(screen_recording_pane_url().contains("Privacy_ScreenCapture"));
    }

    #[test]
    fn accessibility_pane_url_points_at_the_accessibility_anchor() {
        assert!(accessibility_pane_url().contains("Privacy_Accessibility"));
    }

    // `request_screen_recording`/`request_accessibility` are deliberately not exercised
    // here: unlike every predicate above, they have a real side effect (they can pop the
    // OS consent dialog), which is unsafe to trigger unattended in CI — a headless runner
    // has no user to click through it, and a real one shouldn't have its TCC state
    // mutated by every test run. Their FFI plumbing is verified by hand against the
    // granted mini (see the workspace's macOS de-risking notes); `open_pane` is likewise
    // side-effecting (launches System Settings) and not called here for the same reason.
}
