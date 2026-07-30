//! Environment checks for the macOS accessibility backend ("glass doctor"). The pure
//! `a11y_checks` maps a gathered probe to `Check`s and is unit-tested on any host; `gather`
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

/// Map a system-wide read to the `a11y reader` check.
///
/// Deliberately says nothing about the Accessibility grant — that is its own check, with its own
/// remedy. This one answers "did the API respond", which a granted process can still fail: with
/// `CGSSessionScreenIsLocked` set, every AX tree on the host is withheld (measured 2026-07-29), and
/// a diagnostic that reports only "failed" sends the reader hunting for a bug in glass.
pub fn a11y_checks(probe: SystemWideProbe) -> Vec<Check> {
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
        .with_remedy("grant Accessibility to this binary — see the check below"),
        SystemWideProbe::DidNotComplete => Check::new(
            "a11y reader",
            CheckStatus::Fail,
            format!(
                "the accessibility API did not answer (AXError {CANNOT_COMPLETE}) — a locked \
                 screen withholds every accessibility tree on the host, as does a wedged \
                 accessibility stack"
            ),
        )
        .with_remedy(
            "unlock the screen and re-run; if it was already unlocked, log out and back in",
        ),
        SystemWideProbe::Failed(code) => Check::new(
            "a11y reader",
            CheckStatus::Fail,
            format!("system-wide accessibility read failed (AXError {code})"),
        ),
    }]
}

/// Probe whether the AXUIElement reader is answering: one `AXUIElementCopyAttributeValue` against
/// the system-wide element, which needs no target application and mutates nothing.
///
/// macOS-only on purpose, with no off-macOS stub: a stub would have to invent an answer, which is
/// the fabrication this module exists to remove. Host unit tests drive [`a11y_checks`] directly.
#[cfg(target_os = "macos")]
pub fn checks() -> Vec<Check> {
    a11y_checks(crate::ffi::probe_system_wide())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn an_answering_ax_stack_is_ok() {
        assert_eq!(
            a11y_checks(SystemWideProbe::Answered)[0].status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn an_absent_attribute_still_proves_the_stack_answered() {
        // No focused application is a legitimate answer, not a broken stack: the call returned a
        // definite "no such attribute" rather than refusing to talk.
        assert_eq!(
            a11y_checks(SystemWideProbe::AttributeAbsent)[0].status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn a_disabled_api_fails_and_carries_the_error_code() {
        let c = &a11y_checks(SystemWideProbe::ApiDisabled)[0];
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.contains("-25211"),
            "detail must carry the AXError code: {}",
            c.detail
        );
    }

    #[test]
    fn a_stack_that_did_not_answer_names_the_locked_screen() {
        // The failure that cost an hour on 2026-07-29: the AX stack was fine and the screen was
        // locked, and nothing in the diagnostic said so.
        let c = &a11y_checks(SystemWideProbe::DidNotComplete)[0];
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            c.detail.to_lowercase().contains("locked"),
            "detail must name the locked screen: {}",
            c.detail
        );
    }

    #[test]
    fn an_unrecognized_error_still_reports_its_code() {
        let c = &a11y_checks(SystemWideProbe::Failed(-25200))[0];
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
            let c = &a11y_checks(p)[0];
            assert!(
                !c.detail.contains("System Settings"),
                "the reader line must not duplicate the grant line's remedy: {}",
                c.detail
            );
        }
    }
}
