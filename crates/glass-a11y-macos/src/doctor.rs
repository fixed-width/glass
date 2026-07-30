//! Environment checks for the macOS accessibility backend ("glass doctor"). The pure
//! `a11y_checks` maps gathered facts to `Check`s and is unit-tested on any host; `probe` makes the
//! one real AX call, on macOS only.

use glass_core::{Check, CheckStatus};

/// The attribute the doctor reads off the system-wide element.
///
/// Do not swap this for a cheaper one: it is *access-gated*, so a binary macOS will not let read the
/// tree gets `CannotComplete` from it while `AXRole` on the same element answers `Success` (surveyed
/// on macOS 26.5) — a simpler attribute reports green to a process that cannot read anything.
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
    /// `kAXErrorCannotComplete`. Ambiguous on its own: a binary macOS denies gets this, and so does
    /// one whose read is genuinely not answered — and `AXIsProcessTrusted` does not separate them
    /// (see [`a11y_checks`]).
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

/// The console session's state, as the aggregator reads it. Mirrors `glass_macos::SessionState`
/// without depending on it: this crate maps outcomes to text, and the platform crate owns the read.
///
/// A *locked* session is deliberately not distinguished from an unlocked one: measured on macOS 26.5
/// by locking a host mid-probe, the system-wide read still answers `Success` with
/// `CGSSessionScreenIsLocked` set, so a lock never produces the code it would have qualified. The
/// lock's real consequence — capture and input suppressed — is the `display awake` check's to report.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ConsoleSession {
    /// Somebody is logged in at the console, locked or not.
    LoggedIn,
    /// Nobody is logged in at the console — unmeasured against this probe, unlike the locked case,
    /// and kept because if it does produce an unanswered read, logging in is the only remedy that
    /// helps.
    NoSession,
}

/// Points at the grant check rather than repeating its remedy, so an untrusted process is one cause
/// with one thing to do.
const GRANT_REMEDY: &str = "grant Accessibility to this binary — see the check below";

/// Map a system-wide read to the `a11y reader` check, using two facts the caller has already
/// gathered because the probe cannot be read correctly alone.
///
/// `accessibility_granted` narrows the causes of `CannotComplete` without settling them: measured on
/// one host at one moment, `glass-mcp` read the system-wide element successfully while another
/// binary got that code **with `AXIsProcessTrusted` reporting it as trusted**, so a `true` grant bit
/// does not mean this binary may read.
///
/// `session` distinguishes nobody-at-the-console, where the remedy is to log in rather than to
/// check a grant. A locked session is not a cause at all — see [`ConsoleSession`].
pub fn a11y_checks(
    probe: SystemWideProbe,
    accessibility_granted: bool,
    session: ConsoleSession,
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
        // Only the ambiguous code defers to the session, and only for the one session state that
        // could plausibly cause it: nobody at the console is not a defect to fix, so it warns.
        SystemWideProbe::DidNotComplete if session == ConsoleSession::NoSession => Check::new(
            "a11y reader",
            CheckStatus::Warn,
            format!(
                "the system-wide read was not answered (AXError {CANNOT_COMPLETE}) and no account \
                 is logged in at the console"
            ),
        )
        .with_remedy("log in at the console and re-run (see the `display awake` check)"),
        // Reached only with the grant held, so "go and grant it" would send the operator to a pane
        // where the box is already ticked. A stale TCC record is the case that produces this.
        SystemWideProbe::ApiDisabled => fail(
            format!(
                "assistive access is off for this process (AXError {API_DISABLED}) even though it \
                 holds the Accessibility grant, so the accessibility tools will fail"
            ),
            "remove this binary from System Settings > Privacy & Security > Accessibility, add it \
             again, and restart it",
        ),
        SystemWideProbe::DidNotComplete => fail(
            format!(
                "the accessibility API did not answer (AXError {CANNOT_COMPLETE}) even though this \
                 process reports as trusted — macOS reports trust for a binary whose reads it still \
                 denies, so the likeliest cause is that this exact binary was never granted"
            ),
            "grant Accessibility to the binary you are actually running — a rebuild at another path \
             or under another name is a different binary to macOS — and restart it; if it already \
             holds the grant, log out and back in",
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

    fn reader(probe: SystemWideProbe, granted: bool, session: ConsoleSession) -> Check {
        a11y_checks(probe, granted, session).remove(0)
    }

    const EVERY_SESSION: [ConsoleSession; 2] =
        [ConsoleSession::LoggedIn, ConsoleSession::NoSession];

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
            reader(
                SystemWideProbe::from_ax_code(-25205),
                true,
                ConsoleSession::LoggedIn
            )
            .status,
            CheckStatus::Fail
        );
    }

    #[test]
    fn an_answering_ax_stack_is_ok() {
        assert_eq!(
            reader(SystemWideProbe::Answered, true, ConsoleSession::LoggedIn).status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn nothing_frontmost_still_proves_the_stack_answered() {
        let c = reader(
            SystemWideProbe::NothingFocused,
            true,
            ConsoleSession::LoggedIn,
        );
        assert_eq!(c.status, CheckStatus::Ok);
        // The detail is the arm's whole point: collapsing it into `Answered` would lose the reason
        // the read came back empty.
        assert!(c.detail.contains("frontmost"), "{}", c.detail);
    }

    #[test]
    fn an_untrusted_process_never_reports_a_green_reader() {
        // The probe cannot rescue an untrusted process: it holds no grant, so no tree read can
        // succeed whatever the system-wide element answered. The remedy points at the grant check
        // rather than repeating its System Settings text, so there is one thing to do, not two.
        for p in EVERY_PROBE {
            for session in EVERY_SESSION {
                let c = reader(p, false, session);
                assert_eq!(c.status, CheckStatus::Fail, "{p:?} {session:?} -> {c:?}");
                assert_eq!(c.remedy.as_deref(), Some(GRANT_REMEDY), "{p:?} {session:?}");
            }
        }
    }

    #[test]
    fn a_console_with_nobody_logged_in_is_not_told_the_session_is_unlocked() {
        // `NoSession` used to fold into "unlocked", so the line asserted the opposite of the
        // `display awake` check printed beside it and told a logged-out operator to log out.
        let c = reader(
            SystemWideProbe::DidNotComplete,
            true,
            ConsoleSession::NoSession,
        );
        assert_eq!(c.status, CheckStatus::Warn);
        assert!(c.detail.contains("no account is logged in"), "{}", c.detail);
        assert!(
            c.remedy
                .as_deref()
                .unwrap()
                .contains("log in at the console"),
            "{:?}",
            c.remedy
        );
    }

    #[test]
    fn a_session_state_never_downgrades_a_defect_it_does_not_explain() {
        // The session only qualifies the ambiguous code. An unlocked-session defect stays a defect
        // when the screen happens to be locked — otherwise a rebuilt binary with assistive access
        // off reads as "unlock your screen" and the doctor exits 0.
        for session in EVERY_SESSION {
            for p in [
                SystemWideProbe::ApiDisabled,
                SystemWideProbe::Failed(-25208),
            ] {
                assert_eq!(
                    reader(p, true, session).status,
                    CheckStatus::Fail,
                    "{p:?} {session:?}"
                );
            }
        }
    }

    #[test]
    fn a_locked_console_is_not_a_case_this_check_reports_on() {
        // Measured by locking a macOS 26.5 host mid-probe: the system-wide read still answered, so
        // there is no locked-session outcome to report here. `display awake` carries the lock.
        let c = reader(SystemWideProbe::Answered, true, ConsoleSession::LoggedIn);
        assert_eq!(c.status, CheckStatus::Ok);
    }

    #[test]
    fn an_unlocked_trusted_process_that_gets_no_answer_blames_the_stack() {
        // Same AXError, three causes; with the grant held and the session unlocked, the remaining
        // one is the stack. Sending this operator to unlock an unlocked screen — or to a grant they
        // already hold — is the diagnostic failing at its one job.
        let c = reader(
            SystemWideProbe::DidNotComplete,
            true,
            ConsoleSession::LoggedIn,
        );
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
        let c = reader(SystemWideProbe::ApiDisabled, true, ConsoleSession::LoggedIn);
        assert_eq!(c.status, CheckStatus::Fail);
        assert_ne!(c.remedy.as_deref(), Some(GRANT_REMEDY));
        assert!(c.detail.contains(&API_DISABLED.to_string()), "{}", c.detail);
    }

    #[test]
    fn an_unrecognized_error_still_reports_its_code() {
        let c = reader(
            SystemWideProbe::Failed(-25208),
            true,
            ConsoleSession::LoggedIn,
        );
        assert_eq!(c.status, CheckStatus::Fail);
        assert!(c.detail.contains("-25208"), "{}", c.detail);
    }

    #[test]
    fn no_check_this_module_emits_is_a_dead_end() {
        // `CheckStatus::Fail` is documented as carrying a remedy; a red line with no next step is
        // worst exactly where the operator knows least.
        for p in EVERY_PROBE {
            for granted in [true, false] {
                for session in EVERY_SESSION {
                    let c = reader(p, granted, session);
                    if c.status != CheckStatus::Ok {
                        assert!(
                            c.remedy.is_some(),
                            "{p:?} granted={granted} session={session:?} -> {c:?}"
                        );
                    }
                }
            }
        }
    }
}
