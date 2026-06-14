//! Platform-agnostic key-chord model: the `run_chord` driver that sequences a modifier+key chord
//! against any backend's `ChordSink`, with the inter-phase dwell that makes a frame-based client
//! register the modifier as *held across the key's frame*.

use std::time::Duration;

/// Dwell between a chord's phases (modifier-down → key → modifier-up). A synthetic chord injected as
/// one burst is drained by a frame-based GUI (egui/winit) into a SINGLE frame, so the frame-aggregate
/// modifier reads as already-released and the universal `key_pressed(K) && i.modifiers` hotkey idiom
/// never fires. Holding the modifier across separate frames — like hardware, which holds it across
/// many — fixes it. ~3 frames at 60Hz; ≥1 at 20Hz.
pub const CHORD_DWELL: Duration = Duration::from_millis(50);

/// The per-backend primitives that [`run_chord`] sequences. Each emitting method is **self-committed**
/// (it performs the backend's commit barrier before returning — X11 `XFlush`, Wayland frame+settle,
/// Windows one `SendInput` per call), so `run_chord` owns only ordering and the wall-clock dwell.
/// `modifiers` presses/releases ALL the chord's modifiers at once; `key` presses/releases its single
/// key.
pub trait ChordSink {
    /// Press (`down == true`) or release all the chord's modifier keys.
    fn modifiers(&mut self, down: bool) -> crate::Result<()>;
    /// Press (`down == true`) or release the chord's key.
    fn key(&mut self, down: bool) -> crate::Result<()>;
}

/// Drive a chord against a backend `sink` — the single shared definition of the timing fix: hold the
/// modifier(s) → **dwell** so the GUI registers them → tap the key → **dwell** so the key-press frame
/// (modifier still held) is processed → release the modifier(s). Releasing the modifier strictly
/// after the key's frame is what lets `key_pressed(K) && i.modifiers` hold.
pub fn run_chord<S: ChordSink>(sink: &mut S) -> crate::Result<()> {
    sink.modifiers(true)?;
    std::thread::sleep(CHORD_DWELL);
    sink.key(true)?;
    sink.key(false)?;
    std::thread::sleep(CHORD_DWELL);
    sink.modifiers(false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{run_chord, ChordSink, CHORD_DWELL};
    use crate::Result;

    #[derive(Debug, PartialEq)]
    enum Call {
        Mods(bool),
        Key(bool),
    }

    #[derive(Default)]
    struct RecordingSink {
        calls: Vec<Call>,
    }
    impl ChordSink for RecordingSink {
        fn modifiers(&mut self, down: bool) -> Result<()> {
            self.calls.push(Call::Mods(down));
            Ok(())
        }
        fn key(&mut self, down: bool) -> Result<()> {
            self.calls.push(Call::Key(down));
            Ok(())
        }
    }

    #[test]
    fn holds_modifier_across_the_key_then_releases() {
        use Call::*;
        let mut sink = RecordingSink::default();
        run_chord(&mut sink).unwrap();
        // The order is the fix: the modifier is pressed before, and released strictly AFTER, the key
        // — so a frame-based client sees `key_pressed && modifiers` hold in the key's frame.
        assert_eq!(sink.calls, vec![Mods(true), Key(true), Key(false), Mods(false)]);
    }

    #[test]
    fn run_chord_sleeps_both_dwells() {
        // The two inter-phase dwells are the only wall-clock cost, so elapsed >= 2*CHORD_DWELL proves
        // both holds happen (thread::sleep never returns early, so this can't flake on the >= side).
        use std::time::Instant;
        let mut sink = RecordingSink::default();
        let started = Instant::now();
        run_chord(&mut sink).unwrap();
        assert!(
            started.elapsed() >= CHORD_DWELL * 2,
            "run_chord must sleep both phase dwells (only {:?} elapsed)",
            started.elapsed()
        );
    }
}
