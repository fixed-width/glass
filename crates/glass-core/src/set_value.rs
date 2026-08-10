//! Judging whether a `set_value` write actually took, from the value read back afterwards.
//!
//! Every backend that writes a value faces the same problem: the platform can accept the write
//! without error and change nothing. A UIA `SetValue` on an egui read-only `TextEdit` returns
//! success and leaves the buffer alone; an AX write to a read-only-in-practice editable does the
//! same; a tap-and-type on a mobile field can land somewhere else entirely. Reporting `Ok` on any
//! of those is a false success — the agent believes a field holds text it does not hold.
//!
//! Two rules live here, because two kinds of write need different evidence.
//!
//! An *atomic platform write* (UIA `SetValue`, AX `AXValue`) either happens or does not, and may be
//! reformatted on the way in — a slider set to `"50"` reads back `"50.0"`. So a value that merely
//! differs from the old one is evidence it landed: [`read_back_confirms`]. The macOS and Windows
//! readers had each grown their own copy of that rule; it is theirs.
//!
//! A *typed write* (tap, select-all, delete, type) is not atomic. A dropped key, a `maxLength`, an
//! input filter or autocorrect all leave the field holding something that is neither the request nor
//! the old value, and calling that success is the very failure this module exists to prevent. So a
//! typed write must read back exactly what was asked: [`typed_text_landed`]. Android's on-device
//! service reader reached the same conclusion independently (`a11y_service.rs`).
//!
//! Two write paths deliberately do not use either rule: the AT-SPI (Linux) reader trusts the
//! toolkit's own `set_text_contents` answer without reading back, and Android's service reader keeps
//! its own exact-match loop.

/// Whether a `set_value` write took, judged from the value read back.
///
/// It took iff the read-back equals the request OR differs from the pre-set value. The second half
/// matters because a real write may be reformatted on the way in — a slider set to `"50"` reads
/// back `"50.0"` — while a write that silently did nothing reads back exactly what was there
/// before.
fn set_value_took(before: &str, after: &str, requested: &str) -> bool {
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

/// Whether a read-back shows the element holding exactly what it held before the write — the one
/// outcome that means the write never took effect, as against arriving and being transformed.
///
/// The caller passes a reading it actually took, with an absent value already normalized to `""`:
/// a mapper that drops empty values reports an empty field as no value, and a backend whose
/// read-back *failed* has no reading to compare and must not ask.
///
/// What a backend's own explanation of a write hangs on — attach it here and nowhere else, or a
/// field that transformed the text is handed a remedy for one that never received it.
pub fn write_took_no_effect(observed: &str, before: Option<&str>) -> bool {
    before.is_some_and(|b| b == observed)
}

/// Whether a *typed* write landed, judged from the value read back.
///
/// Exact match, unlike [`read_back_confirms`]: keystrokes can land partially, and a field holding
/// `"worl"` after `"world"` was typed has not received the write, even though it differs from what
/// was there before.
///
/// `read_back` is `None` when the element reports no value, which for a text field means it is
/// empty — the `uiautomator` and `idb` mappers both drop an empty value — so it compares as `""`.
///
/// The cost of the strictness: a field that reformats what it is given (a phone number becoming
/// `"(123) 456-7890"`) reports the write as not applied even though it landed. That is the safer
/// direction — [`crate::GlassError::AxValueNotApplied`] names the value it holds, whereas a false
/// success has an agent asserting against a screen that never changed.
///
/// Not for a clear: see [`typed_clear_landed`].
pub fn typed_text_landed(read_back: Option<&str>, requested: &str) -> bool {
    read_back.unwrap_or("") == requested
}

/// Whether a *clear* — a typed write of `""` — landed, judged from the value read back.
///
/// Empty only. Do not weaken this to "empty, or the value changed": the tap that begins a clear can
/// itself change a field that reformats on focus — a currency field showing `"$1,234.00"` unfocused
/// and `"1234.00"` while editing — so "changed" would confirm a clear whose delete never fired.
///
/// The cost is a false *failure*: on the dogfood AVD an emptied `EditText` reports its placeholder as
/// its text (`text="Search settings"`, and `uiautomator` exposes no separate hint attribute), so a
/// clear that worked reads as not applied. [`crate::GlassError::AxValueNotApplied`] names the
/// placeholder it read back; the other direction would have an agent believing an unchanged field
/// was cleared.
pub fn typed_clear_landed(read_back: Option<&str>) -> bool {
    read_back.unwrap_or("").is_empty()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_field_still_holding_what_it_held_took_no_effect_from_the_write() {
        assert!(write_took_no_effect("hello", Some("hello")));
    }

    #[test]
    fn a_field_holding_anything_else_took_something_from_the_write() {
        // Including a transformation: an iOS field that autocapitalized the text it was given
        // received every keystroke, so a backend's "the tap may have missed" must not attach.
        assert!(!write_took_no_effect("Hello", Some("hello")));
    }

    #[test]
    fn a_read_back_with_no_baseline_cannot_show_a_write_took_no_effect() {
        // `None` is a pre-write read the backend could not take. Treating it as a match would
        // attach an explanation of a write that never landed to one nobody can place.
        assert!(!write_took_no_effect("hello", None));
        assert!(!write_took_no_effect("", None));
    }

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
    fn a_typed_write_must_read_back_exactly() {
        assert!(typed_text_landed(Some("world"), "world"));
        // A dropped keystroke: differs from the old value, but the write did not land.
        assert!(!typed_text_landed(Some("worl"), "world"));
        assert!(!typed_text_landed(Some("hello"), "world"));
    }

    #[test]
    fn a_clear_is_confirmed_only_by_an_empty_field() {
        assert!(typed_clear_landed(None));
        assert!(typed_clear_landed(Some("")));
        // A clear that did not fire.
        assert!(!typed_clear_landed(Some("glass")));
        // The cost, pinned as a decision: a platform that reports an emptied field's placeholder
        // reads as not applied.
        assert!(!typed_clear_landed(Some("Search settings")));
    }

    #[test]
    fn a_typed_write_of_text_still_needs_an_exact_match() {
        assert!(typed_text_landed(None, ""));
        assert!(!typed_text_landed(None, "world"));
    }

    #[test]
    fn an_unchanged_value_does_not_confirm_an_atomic_write() {
        // The arm with no false case before: a read-back equal to the baseline and unequal to the
        // request is the no-op this predicate exists to catch.
        assert!(!read_back_confirms(Some("hello"), Some("hello"), "world"));
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
