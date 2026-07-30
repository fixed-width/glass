//! Environment checks for the macOS accessibility backend ("glass doctor"). The pure
//! `a11y_checks` maps a gathered probe to `Check`s and is unit-tested on any host; `probe`
//! makes the one real AX call, on macOS only.

use glass_core::{Check, CheckStatus};

/// What one system-wide accessibility read did.
///
/// The question is whether the AX stack *answered*, not what it answered: an absent attribute is a
/// definite reply and counts as healthy, while `DidNotComplete` means the API refused to talk.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemWideProbe {
    Answered,
    /// The call returned "this element has no such attribute" — no application is focused. The
    /// stack answered, so the reader works.
    AttributeAbsent,
    /// `kAXErrorAPIDisabled` (-25211): the API is not available to this process.
    ApiDisabled,
    /// `kAXErrorCannotComplete` (-25204): the API did not answer.
    DidNotComplete,
    Failed(i32),
}

/// `kAXErrorAPIDisabled`, quoted in the check so the reader can search for it.
const API_DISABLED: i32 = -25211;
/// `kAXErrorCannotComplete`.
const CANNOT_COMPLETE: i32 = -25204;

/// Map a system-wide read to the `a11y reader` check, disambiguated by whether this process holds
/// the Accessibility grant.
///
/// The grant is needed because macOS reports both causes with the same code: measured on macOS 26.5,
/// an *untrusted* process reading `AXFocusedApplication` off the system-wide element gets
/// `kAXErrorCannotComplete` (-25204), the same code a trusted process gets when the host is
/// withholding its trees — which is what a locked screen does (measured 2026-07-29). Reporting one
/// remedy for both would send half the readers to the wrong fix, and the grant bit is already
/// gathered, so the two are told apart here rather than left to the operator.
///
/// The grant still gets its own check beside this one: this line answers "did the API respond", that
/// one answers "is this binary trusted".
pub fn a11y_checks(probe: SystemWideProbe, accessibility_granted: bool) -> Vec<Check> {
    vec![match probe {
        SystemWideProbe::Answered => Check::new(
            "a11y reader",
            CheckStatus::Ok,
            "AXUIElement reader answered a system-wide read",
        ),
        SystemWideProbe::AttributeAbsent => Check::new(
            "a11y reader",
            CheckStatus::Ok,
            "AXUIElement reader answered a system-wide read (nothing is focused right now)",
        ),
        SystemWideProbe::ApiDisabled => Check::new(
            "a11y reader",
            CheckStatus::Fail,
            format!(
                "the accessibility API is disabled for this process (AXError {API_DISABLED}) — \
                 glass_a11y_snapshot / glass_a11y_marks / glass_click_element / glass_set_value \
                 will fail"
            ),
        )
        .with_remedy(GRANT_REMEDY),
        SystemWideProbe::DidNotComplete if !accessibility_granted => Check::new(
            "a11y reader",
            CheckStatus::Fail,
            format!(
                "the accessibility API did not answer (AXError {CANNOT_COMPLETE}) — this process is \
                 not trusted, which is what that code means for a binary without the Accessibility \
                 grant"
            ),
        )
        .with_remedy(GRANT_REMEDY),
        SystemWideProbe::DidNotComplete => Check::new(
            "a11y reader",
            CheckStatus::Fail,
            format!(
                "the accessibility API did not answer (AXError {CANNOT_COMPLETE}) — this process is \
                 trusted, so the host is withholding its accessibility trees: a locked screen does \
                 this, as does a wedged accessibility stack"
            ),
        )
        .with_remedy("unlock the screen and re-run; if it was already unlocked, log out and back in"),
        SystemWideProbe::Failed(code) => Check::new(
            "a11y reader",
            CheckStatus::Fail,
            format!("system-wide accessibility read failed (AXError {code})"),
        ),
    }]
}

/// Points at the grant check rather than repeating its remedy, so an ungranted process is one cause
/// with one thing to do.
const GRANT_REMEDY: &str = "grant Accessibility to this binary — see the check below";

/// Probe whether the AXUIElement reader is answering — one `AXUIElementCopyAttributeValue` against
/// the system-wide element, which needs no target application and mutates nothing — and report it
/// beside `accessibility_granted`, which the caller has already gathered.
///
/// macOS-only on purpose, with no off-macOS stub: a stub would have to invent an answer, which is
/// the fabrication this module exists to remove. Host unit tests drive [`a11y_checks`] directly.
#[cfg(target_os = "macos")]
pub fn checks(accessibility_granted: bool) -> Vec<Check> {
    a11y_checks(probe(), accessibility_granted)
}

/// The gathered probe on its own, for an aggregator that assembles this line beside checks of its
/// own (`glass-mcp` pairs it with the Accessibility grant).
#[cfg(target_os = "macos")]
pub fn probe() -> SystemWideProbe {
    crate::ffi::probe_system_wide()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answering_ax_stack_is_ok() {
        assert_eq!(
            a11y_checks(SystemWideProbe::Answered, true)[0].status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn an_absent_attribute_still_proves_the_stack_answered() {
        // No focused application is a legitimate answer, not a broken stack: the call returned a
        // definite "no such attribute" rather than refusing to talk.
        assert_eq!(
            a11y_checks(SystemWideProbe::AttributeAbsent, true)[0].status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn a_disabled_api_fails_and_carries_the_error_code() {
        let c = &a11y_checks(SystemWideProbe::ApiDisabled, false)[0];
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.contains("-25211"),
            "detail must carry the AXError code: {}",
            c.detail
        );
    }

    #[test]
    fn a_trusted_process_that_gets_no_answer_names_the_locked_screen() {
        // The failure that cost an hour on 2026-07-29: the AX stack was fine and the screen was
        // locked, and nothing in the diagnostic said so.
        let c = &a11y_checks(SystemWideProbe::DidNotComplete, true)[0];
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.to_lowercase().contains("locked"),
            "detail must name the locked screen: {}",
            c.detail
        );
    }

    #[test]
    fn an_untrusted_process_that_gets_no_answer_names_the_grant_instead() {
        // Same AXError, different cause: macOS 26.5 gives an ungranted binary -25204 too (measured
        // against the real API from an ungranted test binary). Sending that reader to unlock an
        // already-unlocked screen is the diagnostic failing at its one job.
        let c = &a11y_checks(SystemWideProbe::DidNotComplete, false)[0];
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            !c.detail.to_lowercase().contains("locked"),
            "an ungranted process must not be told about the screen: {}",
            c.detail
        );
        assert_eq!(c.remedy.as_deref(), Some(GRANT_REMEDY));
    }

    #[test]
    fn an_unrecognized_error_still_reports_its_code() {
        let c = &a11y_checks(SystemWideProbe::Failed(-25200), true)[0];
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(c.detail.contains("-25200"), "detail: {}", c.detail);
    }

    #[test]
    fn no_variant_reports_the_grant() {
        // The grant is a separate check with a separate remedy; collapsing the two is what made the
        // old hardcoded line useless.
        for p in [
            SystemWideProbe::Answered,
            SystemWideProbe::AttributeAbsent,
            SystemWideProbe::ApiDisabled,
            SystemWideProbe::DidNotComplete,
            SystemWideProbe::Failed(-1),
        ] {
            let c = &a11y_checks(p, false)[0];
            assert!(
                !c.detail.contains("System Settings"),
                "the reader line must not duplicate the grant line's remedy: {}",
                c.detail
            );
        }
    }
}
