//! Environment checks for the macOS accessibility backend ("glass doctor"). The pure
//! `a11y_checks` maps gathered facts to `Check`s and is unit-tested on any host; `probe` makes the
//! one real AX call, on macOS only.

use glass_core::{Check, CheckStatus};

/// The attribute the doctor reads off the system-wide element.
///
/// Do not swap this for a cheaper one. It is *trust-gated*: an untrusted process reading it gets
/// `CannotComplete`, while `AXRole` on the same element answers `Success` (surveyed on macOS 26.5).
/// So this attribute fails exactly when the reader would fail, and a "simpler" one would report
/// green to a process that cannot read a single tree — the fabrication this module exists to remove.
pub const PROBE_ATTRIBUTE: &str = "AXFocusedApplication";

/// `kAXErrorAPIDisabled`: assistive access is off for this process.
const API_DISABLED: i32 = -25211;
/// `kAXErrorCannotComplete`: the call was not answered.
const CANNOT_COMPLETE: i32 = -25204;
/// `kAXErrorNoValue`: the attribute exists but holds nothing.
const NO_VALUE: i32 = -25212;

/// What one system-wide accessibility read did.
///
/// The question is whether the AX stack *answered*, not what it answered.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SystemWideProbe {
    Answered,
    /// `kAXErrorNoValue`: the read was answered with "nothing here" — for [`PROBE_ATTRIBUTE`], no
    /// application is frontmost. A definite reply, so the stack is up.
    NothingFocused,
    /// `kAXErrorAPIDisabled`.
    ApiDisabled,
    /// `kAXErrorCannotComplete`. Ambiguous on its own: an untrusted process gets this, and so does a
    /// trusted one whose read is not answered — which is why [`a11y_checks`] takes the grant.
    DidNotComplete,
    /// Any other `AXError`, carrying the raw code so a field report names it. Includes
    /// `kAXErrorAttributeUnsupported` (-25205), which for [`PROBE_ATTRIBUTE`] means the system-wide
    /// element is not behaving as a working macOS does — not a healthy absence.
    Failed(i32),
}

impl SystemWideProbe {
    /// Classify a raw `AXError` code. Lives here rather than beside the FFI call so it is compiled
    /// and tested on every host, not only on macOS.
    pub fn from_ax_code(code: i32) -> Self {
        match code {
            0 => Self::Answered,
            NO_VALUE => Self::NothingFocused,
            API_DISABLED => Self::ApiDisabled,
            CANNOT_COMPLETE => Self::DidNotComplete,
            other => Self::Failed(other),
        }
    }
}

/// Points at the grant check rather than repeating its remedy, so an untrusted process is one cause
/// with one thing to do.
const GRANT_REMEDY: &str = "grant Accessibility to this binary — see the check below";

/// Map a system-wide read to the `a11y reader` check.
///
/// Takes two facts the caller has already gathered because the probe alone cannot be read
/// correctly. `accessibility_granted`: an untrusted process gets `CannotComplete` (measured on
/// macOS 26.5), the same code a trusted process gets when its read is not answered.
/// `session_locked`: a locked console session is a known cause of accessibility calls misbehaving
/// (`glass-macos`'s input tests record synthetic events being dropped while
/// `CGSSessionScreenIsLocked` is set), and the aggregator already reads the real lock state — so
/// this says the screen is locked when it *is*, rather than inferring it from an error code and
/// contradicting the `display awake` check printed beside it.
///
/// The grant keeps its own check next to this one: this line answers "did the API respond", that
/// one answers "is this binary trusted".
pub fn a11y_checks(
    probe: SystemWideProbe,
    accessibility_granted: bool,
    session_locked: bool,
) -> Vec<Check> {
    let ok = |detail: &str| Check::new("a11y reader", CheckStatus::Ok, detail);
    let fail = |detail: String, remedy: &str| {
        Check::new("a11y reader", CheckStatus::Fail, detail).with_remedy(remedy)
    };

    // An untrusted process cannot read a tree whatever the probe said, so no probe outcome may
    // report this line green: a green reader line above a red grant line is the contradiction the
    // hardcoded `Ok` used to print.
    if !accessibility_granted {
        return vec![fail(
            format!(
                "this process is not trusted, so glass_a11y_snapshot / glass_a11y_marks / \
                 glass_click_element / glass_set_value will fail (system-wide read: {})",
                describe(probe)
            ),
            GRANT_REMEDY,
        )];
    }

    vec![match probe {
        SystemWideProbe::Answered => ok("AXUIElement reader answered a system-wide read"),
        SystemWideProbe::NothingFocused => {
            ok("AXUIElement reader answered a system-wide read (no application is frontmost)")
        }
        // Locked first: it explains every failing code below it, and it is measured rather than
        // guessed, so an operator is never sent to log out over a screen they can just unlock.
        p if session_locked => Check::new(
            "a11y reader",
            CheckStatus::Warn,
            format!(
                "the console session is locked, and the system-wide read did not succeed ({}) — \
                 accessibility is unreliable while locked",
                describe(p)
            ),
        )
        .with_remedy("unlock the session and re-run (see the `display awake` check)"),
        SystemWideProbe::ApiDisabled => fail(
            format!(
                "assistive access is off for this process (AXError {API_DISABLED}), so the \
                 accessibility tools will fail"
            ),
            "enable assistive access for this binary in System Settings > Privacy & Security > \
             Accessibility, then restart it",
        ),
        SystemWideProbe::DidNotComplete => fail(
            format!(
                "the accessibility API did not answer (AXError {CANNOT_COMPLETE}) — this process is \
                 trusted and the session is unlocked, so the accessibility stack is not responding"
            ),
            "log out and back in; if it persists, report this with the `glass doctor` output",
        ),
        SystemWideProbe::Failed(code) => fail(
            format!("system-wide accessibility read failed (AXError {code})"),
            "log out and back in; if it persists, report this AXError code with the `glass doctor` \
             output",
        ),
    }]
}

/// The probe outcome in a half-sentence, for details that report it as evidence rather than as the
/// finding itself.
fn describe(probe: SystemWideProbe) -> String {
    match probe {
        SystemWideProbe::Answered => "answered".into(),
        SystemWideProbe::NothingFocused => format!("AXError {NO_VALUE}, nothing frontmost"),
        SystemWideProbe::ApiDisabled => format!("AXError {API_DISABLED}"),
        SystemWideProbe::DidNotComplete => format!("AXError {CANNOT_COMPLETE}"),
        SystemWideProbe::Failed(code) => format!("AXError {code}"),
    }
}

/// One `AXUIElementCopyAttributeValue` of [`PROBE_ATTRIBUTE`] against the system-wide element: no
/// target application, no mutation, and bounded by an explicit AX messaging timeout.
///
/// macOS-only on purpose, with no off-macOS stub: a stub would have to invent an answer, which is
/// the fabrication this module exists to remove. Host unit tests drive [`a11y_checks`] directly.
#[cfg(target_os = "macos")]
pub fn probe() -> SystemWideProbe {
    crate::ffi::probe_system_wide()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn reader(probe: SystemWideProbe, granted: bool, locked: bool) -> Check {
        a11y_checks(probe, granted, locked).remove(0)
    }

    const EVERY_PROBE: [SystemWideProbe; 5] = [
        SystemWideProbe::Answered,
        SystemWideProbe::NothingFocused,
        SystemWideProbe::ApiDisabled,
        SystemWideProbe::DidNotComplete,
        SystemWideProbe::Failed(-25200),
    ];

    #[test]
    fn the_probe_attribute_is_the_trust_gated_one() {
        // The whole check rests on this string: `AXRole` answers for an untrusted process, so
        // swapping to it would report green to a process that cannot read a tree.
        assert_eq!(PROBE_ATTRIBUTE, "AXFocusedApplication");
    }

    #[test]
    fn every_ax_code_classifies_to_its_own_outcome() {
        assert_eq!(SystemWideProbe::from_ax_code(0), SystemWideProbe::Answered);
        assert_eq!(
            SystemWideProbe::from_ax_code(NO_VALUE),
            SystemWideProbe::NothingFocused
        );
        assert_eq!(
            SystemWideProbe::from_ax_code(API_DISABLED),
            SystemWideProbe::ApiDisabled
        );
        assert_eq!(
            SystemWideProbe::from_ax_code(CANNOT_COMPLETE),
            SystemWideProbe::DidNotComplete
        );
        assert_eq!(
            SystemWideProbe::from_ax_code(-25200),
            SystemWideProbe::Failed(-25200)
        );
    }

    #[test]
    fn an_unsupported_attribute_is_not_treated_as_a_healthy_absence() {
        // -25205 means the system-wide element does not carry the attribute at all, which a working
        // macOS never reports for `AXFocusedApplication` — unlike -25212, which means "nothing is
        // frontmost". Folding the two (the tree walk's `is_absent_error` does) would report a
        // malformed AX stack as healthy.
        assert_eq!(
            SystemWideProbe::from_ax_code(-25205),
            SystemWideProbe::Failed(-25205)
        );
        assert_eq!(
            reader(SystemWideProbe::from_ax_code(-25205), true, false).status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn an_answering_ax_stack_is_ok() {
        assert_eq!(
            reader(SystemWideProbe::Answered, true, false).status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn nothing_frontmost_still_proves_the_stack_answered() {
        let c = reader(SystemWideProbe::NothingFocused, true, false);
        assert_eq!(c.status, CheckStatus::Ok);
        // The detail is the arm's whole point: collapsing it into `Answered` would lose the reason
        // the read came back empty.
        assert!(c.detail.contains("frontmost"), "{}", c.detail);
    }

    #[test]
    fn an_untrusted_process_never_reports_a_green_reader() {
        // The probe cannot rescue an untrusted process: it holds no grant, so no tree read can
        // succeed whatever the system-wide element answered.
        for p in EVERY_PROBE {
            let c = reader(p, false, false);
            assert_eq!(c.status, CheckStatus::Fail, "{p:?} -> {c:?}");
            assert_eq!(c.remedy.as_deref(), Some(GRANT_REMEDY), "{p:?}");
        }
    }

    #[test]
    fn a_locked_session_is_a_warning_naming_the_lock_not_a_failure() {
        // Transient and not a configuration defect: failing here would exit(1) on a Mac driving
        // another backend with its screen locked. The lock is read, not inferred from the code.
        let c = reader(SystemWideProbe::DidNotComplete, true, true);
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("locked"), "{}", c.detail);
        assert!(
            c.remedy.as_deref().unwrap().contains("unlock"),
            "{:?}",
            c.remedy
        );
    }

    #[test]
    fn an_unlocked_trusted_process_that_gets_no_answer_blames_the_stack() {
        // Same AXError, three causes; with the grant held and the session unlocked, the remaining
        // one is the stack. Sending this operator to unlock an unlocked screen — or to a grant they
        // already hold — is the diagnostic failing at its one job.
        let c = reader(SystemWideProbe::DidNotComplete, true, false);
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(
            !c.detail.to_lowercase().contains("not trusted"),
            "{}",
            c.detail
        );
        assert!(
            c.detail.contains(&CANNOT_COMPLETE.to_string()),
            "{}",
            c.detail
        );
        assert_ne!(c.remedy.as_deref(), Some(GRANT_REMEDY));
    }

    #[test]
    fn a_granted_process_with_assistive_access_off_is_not_sent_to_the_grant_check() {
        // The grant line beside this one reads "granted", so pointing at it would be a dead end.
        let c = reader(SystemWideProbe::ApiDisabled, true, false);
        assert_eq!(c.status, CheckStatus::Fail);
        assert_ne!(c.remedy.as_deref(), Some(GRANT_REMEDY));
        assert!(c.detail.contains(&API_DISABLED.to_string()), "{}", c.detail);
    }

    #[test]
    fn an_unrecognized_error_still_reports_its_code() {
        let c = reader(SystemWideProbe::Failed(-25208), true, false);
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(c.detail.contains("-25208"), "{}", c.detail);
    }

    #[test]
    fn no_check_this_module_emits_is_a_dead_end() {
        // `CheckStatus::Fail` is documented as carrying a remedy; a red line with no next step is
        // worst exactly where the operator knows least.
        for p in EVERY_PROBE {
            for granted in [true, false] {
                for locked in [true, false] {
                    let c = reader(p, granted, locked);
                    if c.status != CheckStatus::Ok {
                        assert!(
                            c.remedy.is_some(),
                            "{p:?} granted={granted} locked={locked} -> {c:?}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn the_reader_line_never_duplicates_the_grant_lines_remedy_text() {
        // The grant check owns the System Settings pane; this line points at it instead, so an
        // untrusted operator is given one thing to do rather than two.
        for p in EVERY_PROBE {
            let c = reader(p, false, false);
            assert!(
                !c.remedy.as_deref().unwrap().contains("System Settings"),
                "{p:?} -> {:?}",
                c.remedy
            );
        }
    }
}
