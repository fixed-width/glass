//! Known limitations. A listed check that fails is `XFail` (expected, not a failure);
//! a listed check that PASSES is `XPass` — reported, so a limitation that has quietly
//! been fixed does not rot the support matrix.

use crate::smoke::report::{CheckOutcome, CheckStatus};

#[derive(Debug, Clone, Copy)]
pub struct LedgerEntry {
    pub backend: &'static str,
    pub check: &'static str,
    pub reason: &'static str,
}

/// X11 has no accepted limitations today. Entries for other backends land with
/// their own increments.
pub const KNOWN_LIMITS: &[LedgerEntry] = &[];

/// Reclassify an outcome against the shipped ledger.
pub fn apply(backend: &str, outcome: CheckOutcome) -> CheckOutcome {
    apply_with(KNOWN_LIMITS, backend, outcome)
}

/// The testable core: reclassify against an explicit ledger.
pub fn apply_with(
    ledger: &[LedgerEntry],
    backend: &str,
    mut outcome: CheckOutcome,
) -> CheckOutcome {
    let Some(entry) = ledger
        .iter()
        .find(|e| e.backend == backend && e.check == outcome.name)
    else {
        return outcome;
    };
    match outcome.status {
        CheckStatus::Fail => {
            outcome.status = CheckStatus::XFail;
            outcome.detail = format!("{} (known limitation: {})", outcome.detail, entry.reason);
        }
        CheckStatus::Pass => {
            outcome.status = CheckStatus::XPass;
            outcome.detail = format!(
                "{} (recorded limitation: {} — now passing)",
                outcome.detail, entry.reason
            );
        }
        _ => {}
    }
    outcome
}

#[cfg(test)]
mod tests {
    use super::*;

    const FIXTURE: &[LedgerEntry] = &[LedgerEntry {
        backend: "ios",
        check: "gesture",
        reason: "single contact only",
    }];

    /// An entry only ever does anything if both its names match exactly what the run
    /// produces — [`apply_with`] compares them with `==`. A typo therefore does not fail
    /// loudly, it just never matches: the limitation everyone believes is accepted hard-fails
    /// the release instead.
    fn validate(ledger: &[LedgerEntry]) -> Result<(), String> {
        let names = crate::smoke::all_check_names();
        for e in ledger {
            if crate::recognized_backend(e.backend).is_none() {
                return Err(format!("{:?} is not a backend glass knows", e.backend));
            }
            if !names.contains(&e.check) {
                return Err(format!(
                    "{:?} is not a check name — the checks are: {}",
                    e.check,
                    names.join(", ")
                ));
            }
        }
        Ok(())
    }

    #[test]
    fn every_shipped_limit_names_a_real_backend_and_a_real_check() {
        validate(KNOWN_LIMITS).expect("the shipped ledger must be reachable");
    }

    #[test]
    fn a_limit_naming_a_check_that_does_not_exist_is_rejected() {
        // The shipped ledger is empty today, so the test above passes vacuously; this one
        // proves the validation would actually catch the plausible typo — `"a11y"` for the
        // real `"a11y snapshot"` — rather than pass on anything.
        let err = validate(&[LedgerEntry {
            backend: "x11",
            check: "a11y",
            reason: "…",
        }])
        .unwrap_err();
        assert!(
            err.contains("a11y snapshot"),
            "must name the real ones: {err}"
        );
    }

    #[test]
    fn a_limit_naming_a_backend_glass_does_not_know_is_rejected() {
        let err = validate(&[LedgerEntry {
            backend: "beos",
            check: "stop",
            reason: "…",
        }])
        .unwrap_err();
        assert!(err.contains("beos"), "got: {err}");
    }

    #[test]
    fn a_listed_failure_becomes_xfail_and_keeps_the_reason() {
        let out = apply_with(
            FIXTURE,
            "ios",
            CheckOutcome::fail(9, "gesture", "two contacts refused"),
        );
        assert_eq!(out.status, CheckStatus::XFail);
        assert!(
            out.detail.contains("single contact only"),
            "got: {}",
            out.detail
        );
    }

    #[test]
    fn a_listed_pass_becomes_xpass() {
        let out = apply_with(
            FIXTURE,
            "ios",
            CheckOutcome::pass(9, "gesture", "two contacts worked"),
        );
        assert_eq!(out.status, CheckStatus::XPass);
    }

    #[test]
    fn an_unlisted_failure_stays_a_failure_and_other_backends_are_untouched() {
        let out = apply_with(FIXTURE, "ios", CheckOutcome::fail(2, "start", "no window"));
        assert_eq!(out.status, CheckStatus::Fail);
        let out = apply_with(
            FIXTURE,
            "x11",
            CheckOutcome::fail(9, "gesture", "two contacts refused"),
        );
        assert_eq!(out.status, CheckStatus::Fail);
    }
}
