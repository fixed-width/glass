//! TCC preflight. glass needs two grants on macOS: **Screen Recording** (capture +
//! window titles) and **Accessibility** (window move/resize/focus + CGEvent input).
//! Neither can be force-granted (SIP/MDM can't allow Screen Recording); the product
//! holds them via a stable code-signed identity (see the validation plan). Here we only
//! *detect* a missing grant and return an actionable error — never a blank frame.

use glass_core::{GlassError, Result};

// Both are plain C functions; no objc2 needed for preflight.
#[link(name = "CoreGraphics", kind = "framework")]
extern "C" {
    fn CGPreflightScreenCaptureAccess() -> bool;
}
#[link(name = "ApplicationServices", kind = "framework")]
extern "C" {
    // Apple declares this `Boolean` (= `unsigned char`), NOT C99 `_Bool`. Binding it as
    // `u8` and comparing `!= 0` avoids the Rust-`bool` validity invariant (only 0/1 are
    // legal bit patterns; any other byte would be instant UB), matching `accessibility-sys`.
    fn AXIsProcessTrusted() -> u8;
}

/// True if this process holds the Screen Recording grant.
pub fn screen_recording_ok() -> bool {
    // SAFETY: `CGPreflightScreenCaptureAccess` is a no-argument C predicate that only
    // reads this process's TCC state; it has no preconditions and no side effects.
    unsafe { CGPreflightScreenCaptureAccess() }
}

/// True if this process is trusted for Accessibility (AX APIs + CGEvent posting).
pub fn accessibility_ok() -> bool {
    // SAFETY: `AXIsProcessTrusted` is a no-argument C predicate over this process's
    // trust state; no preconditions, no side effects. It returns `Boolean` (u8); any
    // nonzero value means trusted.
    unsafe { AXIsProcessTrusted() != 0 }
}

/// Fail fast with an actionable error if either grant is missing. Called at session
/// start before any capture/input is attempted.
pub fn preflight() -> Result<()> {
    if !screen_recording_ok() {
        return Err(GlassError::PermissionDenied {
            which: "Screen Recording".into(),
            remedy: "enable glass in System Settings > Privacy & Security > Screen Recording \
                     (run inside a logged-in session; grant persists for the signed binary)"
                .into(),
        });
    }
    if !accessibility_ok() {
        return Err(GlassError::PermissionDenied {
            which: "Accessibility".into(),
            remedy: "enable glass in System Settings > Privacy & Security > Accessibility".into(),
        });
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preflight_matches_the_two_predicates() {
        // On a box where grants are present, preflight is Ok; where absent, it errors with
        // the missing permission named. Either way the predicates and preflight agree.
        let sr = screen_recording_ok();
        let ax = accessibility_ok();
        match preflight() {
            Ok(()) => assert!(sr && ax, "preflight Ok but a predicate was false"),
            Err(GlassError::PermissionDenied { which, .. }) => {
                assert!(!sr || !ax, "preflight denied but both predicates true");
                assert!(which == "Screen Recording" || which == "Accessibility", "{which}");
            }
            Err(e) => panic!("unexpected error: {e}"),
        }
    }
}
