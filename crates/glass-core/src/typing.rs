//! Platform-agnostic text-typing model: the `run_type` driver that types a string one
//! character at a time against any backend's `TypeSink`, pacing each keystroke with a dwell
//! so a target that processes input asynchronously drains one before the next arrives.

use std::time::Duration;

/// Default dwell between consecutive typed characters. On Windows, injecting
/// `KEYEVENTF_UNICODE` keystrokes faster than the target drains its input queue triggers a
/// race *downstream* of glass: the OS resolves queued `VK_PACKET` events from a shared slot,
/// so a backed-up run of characters all collapse to the *last* value written (`"aaa bbb ccc"`
/// typed as `"aaa ccccccc"`; identical-character runs are unaffected). glass injects the
/// correct keystrokes with zero drops — the corruption is in the OS/target, and a
/// per-character dwell long enough for it to keep up avoids it. Tunable per host on Windows
/// via `GLASS_TYPE_DWELL_MS` (raise on a slow/loaded box, lower for speed). 60ms is the
/// measured-reliable floor on a Win11 interactive desktop (30ms still dropped a character
/// ~1/3 of the time on strings with adjacent identical characters).
pub const TYPE_DWELL: Duration = Duration::from_millis(60);

/// The per-backend primitive that [`run_type`] sequences. `character` is **self-committed**
/// (it performs the backend's commit barrier before returning — Windows one `SendInput` per
/// call), so `run_type` owns only the per-character ordering and the inter-character dwell.
pub trait TypeSink {
    /// Press and release one character — `code_units` is its UTF-16 encoding (one unit for a
    /// BMP char, two for a surrogate pair). The pair is committed together so a non-BMP
    /// character is never split across the dwell.
    fn character(&mut self, code_units: &[u16]) -> crate::Result<()>;
}

/// Type `text` against a backend `sink`, one character at a time, sleeping `dwell` *between*
/// characters (so there are `n-1` dwells — none before the first or after the last). Each
/// character is emitted as its own committed injection; the dwell is what keeps a string from
/// being delivered faster than the target can drain it.
pub fn run_type<S: TypeSink>(sink: &mut S, text: &str, dwell: Duration) -> crate::Result<()> {
    let mut buf = [0u16; 2];
    let mut first = true;
    for c in text.chars() {
        if !first {
            std::thread::sleep(dwell);
        }
        first = false;
        sink.character(c.encode_utf16(&mut buf))?;
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
        chars: Vec<Vec<u16>>,
    }
    impl TypeSink for RecordingSink {
        fn character(&mut self, code_units: &[u16]) -> Result<()> {
            self.chars.push(code_units.to_vec());
            Ok(())
        }
    }

    #[test]
    fn emits_each_character_individually_including_adjacent_duplicates() {
        // The bug class: adjacent identical characters. Each must be emitted as its own
        // character, in order — never collapsed or batched.
        let mut sink = RecordingSink::default();
        run_type(&mut sink, "aab", Duration::ZERO).unwrap();
        assert_eq!(sink.chars, vec![vec![b'a' as u16], vec![b'a' as u16], vec![b'b' as u16]]);
    }

    #[test]
    fn keeps_a_surrogate_pair_in_one_commit() {
        // U+1D11E (𝄞) is a non-BMP char: two UTF-16 units that must stay in one committed
        // injection, not be split across the inter-character dwell.
        let mut sink = RecordingSink::default();
        run_type(&mut sink, "𝄞", Duration::ZERO).unwrap();
        assert_eq!(sink.chars, vec![vec![0xD834, 0xDD1E]]);
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
