//! Platform-agnostic scroll model: the `run_scroll` driver that sequences a wheel scroll — with an
//! optional held modifier — against any backend's `ScrollSink`. A modified scroll holds the modifier
//! across the wheel's frame (the same dwell fix as `run_chord`) so a frame-based client reads
//! `i.modifiers` as held when the wheel arrives.

use std::time::Duration;

/// Dwell between a modified scroll's phases (modifier-down → wheel → modifier-up). A modifier+wheel
/// injected as one burst is drained by a frame-based GUI (egui/winit) into a SINGLE frame, so the
/// frame-aggregate modifier reads as already-released and a `ctrl/shift + wheel` gesture (zoom,
/// shift-to-scroll-horizontally) never sees the modifier held. Holding it across separate frames —
/// like hardware — fixes it. Mirrors [`crate::chord::CHORD_DWELL`]: ~3 frames at 60Hz; ≥1 at 20Hz.
pub const SCROLL_DWELL: Duration = Duration::from_millis(50);

/// The per-backend primitives that [`run_scroll`] sequences. Each emitting method is **self-committed**
/// (it performs the backend's commit barrier before returning — X11 `XFlush`, Wayland frame+settle,
/// Windows one `SendInput` per call), so `run_scroll` owns only ordering and the wall-clock dwell.
pub trait ScrollSink {
    /// Press (`down == true`) or release the scroll's held modifier keys. Called by [`run_scroll`]
    /// only for a modified scroll (the plain path emits the wheel directly).
    fn modifiers(&mut self, down: bool) -> crate::Result<()>;
    /// Position the pointer and emit the wheel (vertical then horizontal) at that point.
    fn wheel(&mut self) -> crate::Result<()>;
}

/// Drive a scroll against a backend `sink`. A *plain* scroll (`has_modifiers == false`) emits the
/// wheel directly — the hot, latency-sensitive path takes no dwell. A *modified* scroll holds the
/// modifier → **dwell** so the GUI registers it → emit the wheel (modifier still held) → **dwell** →
/// release: the same frame-aware sequencing as [`crate::run_chord`], so `i.modifiers` reads held in
/// the wheel's frame. Releasing the modifier strictly after the wheel's frame is what lets a handler
/// gating on `i.modifiers.ctrl` see it.
pub fn run_scroll<S: ScrollSink>(sink: &mut S, has_modifiers: bool) -> crate::Result<()> {
    if !has_modifiers {
        return sink.wheel();
    }
    sink.modifiers(true)?;
    std::thread::sleep(SCROLL_DWELL);
    sink.wheel()?;
    std::thread::sleep(SCROLL_DWELL);
    sink.modifiers(false)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{run_scroll, ScrollSink, SCROLL_DWELL};
    use crate::Result;

    #[derive(Debug, PartialEq)]
    enum Call {
        Mods(bool),
        Wheel,
    }

    #[derive(Default)]
    struct RecordingSink {
        calls: Vec<Call>,
    }
    impl ScrollSink for RecordingSink {
        fn modifiers(&mut self, down: bool) -> Result<()> {
            self.calls.push(Call::Mods(down));
            Ok(())
        }
        fn wheel(&mut self) -> Result<()> {
            self.calls.push(Call::Wheel);
            Ok(())
        }
    }

    #[test]
    fn modified_scroll_holds_modifier_across_the_wheel_then_releases() {
        use Call::*;
        let mut sink = RecordingSink::default();
        run_scroll(&mut sink, true).unwrap();
        // The order is the fix: the modifier is pressed before, and released strictly AFTER, the
        // wheel — so a frame-based client sees `i.modifiers` held in the wheel's frame.
        assert_eq!(sink.calls, vec![Mods(true), Wheel, Mods(false)]);
    }

    #[test]
    fn plain_scroll_emits_the_wheel_with_no_modifier_traffic() {
        use Call::*;
        let mut sink = RecordingSink::default();
        run_scroll(&mut sink, false).unwrap();
        // No modifier to hold: just the wheel via the early-return branch (which has no dwell).
        assert_eq!(sink.calls, vec![Wheel]);
    }

    #[test]
    fn modified_scroll_sleeps_both_dwells() {
        // The two inter-phase dwells are the only wall-clock cost, so elapsed >= 2*SCROLL_DWELL
        // proves both holds happen (thread::sleep never returns early, so this can't flake on the
        // >= side). The plain path's no-dwell is pinned by the call-sequence test above.
        use std::time::Instant;
        let mut sink = RecordingSink::default();
        let started = Instant::now();
        run_scroll(&mut sink, true).unwrap();
        assert!(
            started.elapsed() >= SCROLL_DWELL * 2,
            "a modified scroll must sleep both phase dwells (only {:?} elapsed)",
            started.elapsed()
        );
    }
}
