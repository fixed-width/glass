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
