//! Platform-agnostic text-typing model: the `run_type` driver that types a string one
//! character at a time against any backend's `TypeSink`, committing each keystroke before
//! the next so a client that processes input asynchronously doesn't miss keys.

use std::time::Duration;

use crate::{Deadline, GlassError};

/// Default dwell between consecutive typed characters. Used by the Windows backend (tunable
/// via `GLASS_TYPE_DWELL_MS`): injecting `KEYEVENTF_UNICODE` keystrokes faster than the
/// target drains its queue races a downstream OS bug that collapses a run of characters to
/// the last one (`"aaa bbb ccc"` → `"aaa ccccccc"`). 60ms is the measured-reliable floor on
/// a Win11 desktop. The Linux backends pace by committing each keystroke (X11 `XFlush` /
/// Wayland roundtrip) rather than by sleeping, so they pass a shorter dwell.
pub const TYPE_DWELL: Duration = Duration::from_millis(60);

/// The per-backend primitive that [`run_type`] sequences. `character` must be
/// **self-committed**: it performs the backend's commit barrier before returning — Windows
/// one `SendInput`, X11 `XFlush`, Wayland a compositor roundtrip — so each keystroke is
/// delivered before the next. A picky or heavy client (e.g. a browser) silently drops
/// keystrokes that are merely queued and committed once at the end.
pub trait TypeSink {
    /// Press and release one character, committing before returning.
    fn character(&mut self, c: char) -> crate::Result<()>;
}

/// Type `text` without a caller deadline.
pub fn run_type<S: TypeSink>(sink: &mut S, text: &str, dwell: Duration) -> crate::Result<()> {
    run_type_by(sink, text, dwell, Deadline::UNBOUNDED)
}

fn require_time(deadline: Deadline, started: bool) -> crate::Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    if started {
        Err(GlassError::caller_deadline_elapsed("typing"))
    } else {
        Err(GlassError::deadline_not_started("typing"))
    }
}

fn sleep_by(deadline: Deadline, requested: Duration) -> crate::Result<()> {
    require_time(deadline, true)?;
    let sleep_for = deadline.remaining().unwrap_or(requested).min(requested);
    std::thread::sleep(sleep_for);
    require_time(deadline, true)
}

/// Type individually committed characters until `deadline`, with `dwell` only between them.
pub fn run_type_by<S: TypeSink>(
    sink: &mut S,
    text: &str,
    dwell: Duration,
    deadline: Deadline,
) -> crate::Result<()> {
    require_time(deadline, false)?;
    let mut first = true;
    let mut started = false;
    for c in text.chars() {
        if !first {
            sleep_by(deadline, dwell)?;
        }
        first = false;
        require_time(deadline, started)?;
        started = true;
        sink.character(c)?;
        require_time(deadline, true)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{TypeSink, run_type, run_type_by};
    use crate::{BoundDispatch, BoundKind, Deadline, Result};
    use std::time::{Duration, Instant};

    #[derive(Default)]
    struct RecordingSink {
        chars: Vec<char>,
    }
    impl TypeSink for RecordingSink {
        fn character(&mut self, c: char) -> Result<()> {
            self.chars.push(c);
            Ok(())
        }
    }

    #[test]
    fn emits_each_character_in_order_including_adjacent_duplicates() {
        // Adjacent duplicates and spaces must remain distinct keystrokes.
        let mut sink = RecordingSink::default();
        run_type(&mut sink, "aab c", Duration::ZERO).unwrap();
        assert_eq!(sink.chars, vec!['a', 'a', 'b', ' ', 'c']);
    }

    #[test]
    fn passes_each_char_whole_including_non_bmp() {
        // run_type splits on `char`, never bytes/code units — a non-BMP character (U+1D11E)
        // reaches the sink as a single `char`, so a backend can't split it mid-keystroke.
        let mut sink = RecordingSink::default();
        run_type(&mut sink, "a𝄞b", Duration::ZERO).unwrap();
        assert_eq!(sink.chars, vec!['a', '𝄞', 'b']);
    }

    #[test]
    fn empty_text_emits_nothing() {
        let mut sink = RecordingSink::default();
        run_type(&mut sink, "", Duration::ZERO).unwrap();
        assert!(sink.chars.is_empty());
    }

    #[test]
    fn dwells_the_given_duration_between_characters() {
        // For n chars there are n-1 inter-character dwells; elapsed >= (n-1)*dwell proves the
        // passed dwell is honored (thread::sleep never returns early, so this can't flake on
        // the >= side).
        use std::time::Instant;
        let dwell = Duration::from_millis(10);
        let mut sink = RecordingSink::default();
        let started = Instant::now();
        run_type(&mut sink, "abcd", dwell).unwrap(); // 4 chars -> 3 dwells
        assert!(
            started.elapsed() >= dwell * 3,
            "run_type must dwell the given duration between characters (only {:?} elapsed)",
            started.elapsed()
        );
    }

    #[test]
    fn run_type_by_spent_deadline_emits_no_events() {
        let mut sink = RecordingSink::default();
        let deadline = Deadline::at(Instant::now() - Duration::from_millis(1));

        let error = run_type_by(&mut sink, "abc", Duration::ZERO, deadline).unwrap_err();

        assert!(sink.chars.is_empty());
        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn run_type_by_deadline_expiring_mid_sequence_stops_before_the_next_event() {
        let mut full_sink = RecordingSink::default();
        run_type(&mut full_sink, "abc", Duration::ZERO).unwrap();
        let full_sequence = full_sink.chars;

        let dwell = Duration::from_millis(250);
        let mut sink = RecordingSink::default();
        let started = Instant::now();
        let error = run_type_by(&mut sink, "abc", dwell, Deadline::from_millis(10)).unwrap_err();

        assert!(sink.chars.len() < full_sequence.len());
        assert_eq!(sink.chars, vec!['a']);
        assert!(
            started.elapsed() < dwell,
            "the inter-character sleep must be capped by the deadline"
        );
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn run_type_by_unbounded_wrapper_preserves_the_legacy_sequence() {
        let mut sink = RecordingSink::default();

        run_type(&mut sink, "aab c", Duration::ZERO).unwrap();

        assert_eq!(sink.chars, vec!['a', 'a', 'b', ' ', 'c']);
    }

    #[test]
    fn run_type_by_expiry_after_a_sink_event_is_may_have_dispatched() {
        struct SlowSink(RecordingSink);

        impl TypeSink for SlowSink {
            fn character(&mut self, c: char) -> Result<()> {
                self.0.character(c)?;
                std::thread::sleep(Duration::from_millis(20));
                Ok(())
            }
        }

        let mut sink = SlowSink(RecordingSink::default());
        let error =
            run_type_by(&mut sink, "abc", Duration::ZERO, Deadline::from_millis(5)).unwrap_err();

        assert_eq!(sink.0.chars, vec!['a']);
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }
}
