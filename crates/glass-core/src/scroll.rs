//! Platform-agnostic scroll model: the `run_scroll` driver that sequences a wheel scroll — with an
//! optional held modifier — against any backend's `ScrollSink`. A modified scroll holds the modifier
//! across the wheel's frame (the same dwell fix as `run_chord`) so a frame-based client reads
//! `i.modifiers` as held when the wheel arrives.

use std::time::Duration;

use crate::{Deadline, GlassError};

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

/// Drive a scroll without a caller deadline.
pub fn run_scroll<S: ScrollSink>(sink: &mut S, has_modifiers: bool) -> crate::Result<()> {
    run_scroll_by(sink, has_modifiers, Deadline::UNBOUNDED)
}

fn require_time(deadline: Deadline, started: bool) -> crate::Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    if started {
        Err(GlassError::caller_deadline_elapsed("scroll"))
    } else {
        Err(GlassError::deadline_not_started("scroll"))
    }
}

fn sleep_by(deadline: Deadline, requested: Duration) -> crate::Result<()> {
    require_time(deadline, true)?;
    let sleep_for = deadline.remaining().unwrap_or(requested).min(requested);
    std::thread::sleep(sleep_for);
    require_time(deadline, true)
}

fn attach_cleanup_failure(primary: GlassError, cleanup: GlassError) -> GlassError {
    GlassError::input_cleanup_failed("releasing the scroll modifiers", primary, cleanup)
}

fn preserve_primary_after_cleanup(primary: GlassError, cleanup: crate::Result<()>) -> GlassError {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => attach_cleanup_failure(primary, cleanup),
    }
}

fn cleanup_modifiers<S: ScrollSink>(sink: &mut S) -> crate::Result<()> {
    // A release is mandatory safety cleanup once modifier-down may have landed,
    // even when the deadline has elapsed or the wheel dispatch failed.
    sink.modifiers(false)
}

/// Drive a scroll against a backend `sink`, stopping at `deadline`.
///
/// A plain scroll emits only the wheel. A modified scroll keeps the legacy
/// modifier-down → dwell → wheel → dwell → modifier-up sequence while bounding
/// both dwells and checking the deadline around every normal event dispatch.
pub fn run_scroll_by<S: ScrollSink>(
    sink: &mut S,
    has_modifiers: bool,
    deadline: Deadline,
) -> crate::Result<()> {
    require_time(deadline, false)?;
    if !has_modifiers {
        sink.wheel()?;
        return require_time(deadline, true);
    }

    let modifiers_down = true;
    if let Err(error) = sink.modifiers(true) {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    if let Err(error) = sleep_by(deadline, SCROLL_DWELL) {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = sink.wheel() {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    if let Err(error) = sleep_by(deadline, SCROLL_DWELL) {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, modifiers_down) {
        let cleanup = cleanup_modifiers(sink);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    sink.modifiers(false)?;
    require_time(deadline, true)
}

#[cfg(test)]
mod tests {
    use super::{SCROLL_DWELL, ScrollSink, run_scroll, run_scroll_by};
    use crate::{BoundDispatch, BoundKind, Deadline, GlassError, Result};
    use std::time::{Duration, Instant};

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

    #[test]
    fn run_scroll_by_spent_deadline_emits_no_events() {
        let mut sink = RecordingSink::default();
        let deadline = Deadline::at(Instant::now() - Duration::from_millis(1));

        let error = run_scroll_by(&mut sink, true, deadline).unwrap_err();

        assert!(sink.calls.is_empty());
        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn run_scroll_by_deadline_expiring_mid_sequence_stops_before_the_next_event() {
        use Call::*;
        let mut full_sink = RecordingSink::default();
        run_scroll(&mut full_sink, true).unwrap();
        let full_sequence = full_sink.calls;

        let mut sink = RecordingSink::default();
        let started = Instant::now();
        let error = run_scroll_by(&mut sink, true, Deadline::from_millis(10)).unwrap_err();

        assert!(sink.calls.len() < full_sequence.len());
        assert_eq!(sink.calls, vec![Mods(true), Mods(false)]);
        assert!(
            started.elapsed() < SCROLL_DWELL,
            "the modifier dwell must be capped by the deadline"
        );
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn run_scroll_by_unbounded_wrapper_preserves_the_legacy_sequence() {
        use Call::*;
        let mut sink = RecordingSink::default();

        run_scroll(&mut sink, true).unwrap();

        assert_eq!(sink.calls, vec![Mods(true), Wheel, Mods(false)]);
    }

    #[test]
    fn run_scroll_by_expiry_after_a_sink_event_is_may_have_dispatched() {
        struct SlowWheelSink(RecordingSink);

        impl ScrollSink for SlowWheelSink {
            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)
            }

            fn wheel(&mut self) -> Result<()> {
                self.0.wheel()?;
                std::thread::sleep(Duration::from_millis(20));
                Ok(())
            }
        }

        let mut sink = SlowWheelSink(RecordingSink::default());
        let error = run_scroll_by(&mut sink, false, Deadline::from_millis(5)).unwrap_err();

        assert_eq!(sink.0.calls, vec![Call::Wheel]);
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn run_scroll_by_attempts_modifier_cleanup_when_wheel_fails() {
        struct FailingWheelSink(RecordingSink);

        impl ScrollSink for FailingWheelSink {
            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)?;
                if down {
                    Ok(())
                } else {
                    Err(GlassError::Backend("modifier cleanup failed".into()))
                }
            }

            fn wheel(&mut self) -> Result<()> {
                self.0.wheel()?;
                Err(GlassError::Backend("wheel failed".into()))
            }
        }

        let mut sink = FailingWheelSink(RecordingSink::default());
        let error = run_scroll_by(&mut sink, true, Deadline::UNBOUNDED).unwrap_err();

        assert!(error.to_string().contains("wheel failed"));
        assert!(error.to_string().contains("modifier cleanup failed"));
        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("scroll and modifier release failures must stay structured");
        };
        assert_eq!(operation, "releasing the scroll modifiers");
        assert!(matches!(*primary, GlassError::Backend(message) if message == "wheel failed"));
        assert!(
            matches!(*cleanup, GlassError::Backend(message) if message == "modifier cleanup failed")
        );
        assert_eq!(sink.0.calls.last(), Some(&Call::Mods(false)));
    }

    #[test]
    fn run_scroll_by_preserves_deadline_provenance_when_cleanup_fails() {
        struct SlowModifierCleanupSink(RecordingSink);

        impl ScrollSink for SlowModifierCleanupSink {
            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)?;
                if down {
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                } else {
                    Err(GlassError::Backend("modifier cleanup failed".into()))
                }
            }

            fn wheel(&mut self) -> Result<()> {
                self.0.wheel()
            }
        }

        let mut sink = SlowModifierCleanupSink(RecordingSink::default());
        let error = run_scroll_by(&mut sink, true, Deadline::from_millis(5)).unwrap_err();

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("modifier cleanup failed"));
    }
}
