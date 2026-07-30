//! Judging whether a `set_value` write actually took, from the value read back afterwards.
//!
//! Every backend that writes a value faces the same problem: the platform can accept the write
//! without error and change nothing. A UIA `SetValue` on an egui read-only `TextEdit` returns
//! success and leaves the buffer alone; an AX write to a read-only-in-practice editable does the
//! same; a tap-and-type on a mobile field can land somewhere else entirely. Reporting `Ok` on any
//! of those is a false success — the agent believes a field holds text it does not hold.
//!
//! So the judgement lives here, once, rather than in each reader: three of them had already grown
//! their own copy of it.

/// Whether a `set_value` write took, judged from the value read back.
///
/// It took iff the read-back equals the request OR differs from the pre-set value. The second half
/// matters because a real write may be reformatted on the way in — a slider set to `"50"` reads
/// back `"50.0"` — while a write that silently did nothing reads back exactly what was there
/// before.
pub fn set_value_took(before: &str, after: &str, requested: &str) -> bool {
    after == requested || after != before
}

/// Whether a read-back can *confirm* the write took. `read_back` is the value read afterwards
/// (`None` when that read failed or the value was absent); `before` is the pre-write baseline
/// (`None` when that read failed — the baseline is unknown).
///
/// Confirms only when it can prove the write landed:
/// - a failed post-write read is inconclusive and never confirms;
/// - with a known baseline it delegates to [`set_value_took`];
/// - with an unknown baseline only an exact match with the request confirms, because "differs from
///   before" means nothing without a baseline to differ from.
///
/// That last rule is the guard against a *failed read* passing for a change: a reader that collapsed
/// both reads to `""` once reported false success for exactly that reason.
pub fn read_back_confirms(read_back: Option<&str>, before: Option<&str>, requested: &str) -> bool {
    match (read_back, before) {
        (None, _) => false,
        (Some(after), Some(before)) => set_value_took(before, after, requested),
        (Some(after), None) => after == requested,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_write_that_changed_nothing_did_not_take() {
        // A read-only editable that accepts the write and keeps its value.
        assert!(!set_value_took("#000000", "#000000", "#12AA34"));
    }

    #[test]
    fn a_read_back_equal_to_the_request_took() {
        assert!(set_value_took("#000000", "#12AA34", "#12AA34"));
    }

    #[test]
    fn a_reformatted_value_still_took() {
        // A slider set to "50" may read back "50.0" — changed from before, so it landed.
        assert!(set_value_took("0", "50.0", "50"));
    }

    #[test]
    fn writing_the_value_it_already_holds_counts_as_taken() {
        // Nothing can distinguish this from a no-op, and reporting failure for a write that asked
        // for the value already present would be the more misleading answer.
        assert!(set_value_took("50", "50", "50"));
    }

    #[test]
    fn a_failed_post_read_never_confirms() {
        // Inconclusive is not success: the caller reports `AxValueNotApplied` rather than guess.
        assert!(!read_back_confirms(None, Some("hello"), "world"));
    }

    #[test]
    fn a_change_from_a_known_baseline_confirms() {
        assert!(read_back_confirms(Some("50.0"), Some("0"), "50"));
        assert!(read_back_confirms(Some("0.0"), Some("0"), "0"));
    }

    #[test]
    fn a_mere_difference_cannot_confirm_when_the_baseline_is_unknown() {
        // The regression this rule exists for: a failed pre-read defaulting to "" made a no-op that
        // reads back its real value look "changed", and the write was reported as successful.
        assert!(!read_back_confirms(Some("hello"), None, "world"));
    }

    #[test]
    fn an_exact_match_confirms_even_with_an_unknown_baseline() {
        assert!(read_back_confirms(Some("world"), None, "world"));
    }
}
