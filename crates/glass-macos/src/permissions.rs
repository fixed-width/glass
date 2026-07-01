//! TCC preflight. glass needs two grants on macOS: **Screen Recording** (capture +
//! window titles) and **Accessibility** (window move/resize/focus + CGEvent input).
//! Neither can be force-granted (SIP/MDM can't allow Screen Recording); the product
//! holds them via a stable code-signed identity. Here we only *detect* a missing grant
//! and return an actionable error — never a blank frame.

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
}
