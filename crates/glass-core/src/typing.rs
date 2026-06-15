//! Platform-agnostic text-typing model: the `run_type` driver that types a string one
//! character at a time against any backend's `TypeSink`, committing each keystroke before
//! the next so a client that processes input asynchronously doesn't miss keys.

use std::time::Duration;

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

/// Type `text` against a backend `sink`, one character at a time, sleeping `dwell` *between*
/// characters (so there are `n-1` dwells — none before the first or after the last). Each
/// character is its own committed keystroke; together with the dwell this keeps a string
/// from being delivered faster than the target can drain it.
pub fn run_type<S: TypeSink>(sink: &mut S, text: &str, dwell: Duration) -> crate::Result<()> {
    let mut first = true;
    for c in text.chars() {
        if !first {
            std::thread::sleep(dwell);
        }
        first = false;
        sink.character(c)?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{run_type, TypeSink};
    use crate::Result;
    use std::time::Duration;

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
        // The bug class: runs of adjacent identical characters (and spaces). Each must be
        // emitted as its own keystroke, in order — never collapsed or batched.
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
}
