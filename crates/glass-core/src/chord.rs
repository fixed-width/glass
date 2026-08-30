//! Platform-agnostic key-chord model: the `run_chord` driver that sequences a modifier+key chord
//! against any backend's `ChordSink`, with the inter-phase dwell that makes a frame-based client
//! register the modifier as *held across the key's frame*.

use std::time::Duration;

use crate::{Deadline, GlassError};

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

/// Drive a chord without a caller deadline.
pub fn run_chord<S: ChordSink>(sink: &mut S) -> crate::Result<()> {
    run_chord_by(sink, Deadline::UNBOUNDED)
}

fn require_time(deadline: Deadline, started: bool) -> crate::Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    if started {
        Err(GlassError::caller_deadline_elapsed("key chord"))
    } else {
        Err(GlassError::deadline_not_started("key chord"))
    }
}

fn sleep_by(deadline: Deadline, requested: Duration) -> crate::Result<()> {
    require_time(deadline, true)?;
    let sleep_for = sleep_duration(deadline.remaining(), requested);
    std::thread::sleep(sleep_for);
    require_time(deadline, true)
}

fn sleep_duration(remaining: Option<Duration>, requested: Duration) -> Duration {
    remaining.unwrap_or(requested).min(requested)
}

fn attach_cleanup_failure(
    primary: GlassError,
    cleanup: GlassError,
    release: &'static str,
) -> GlassError {
    GlassError::input_cleanup_failed(release, primary, cleanup)
}

fn combine_cleanup(
    primary: crate::Result<()>,
    cleanup: crate::Result<()>,
    release: &'static str,
) -> crate::Result<()> {
    match (primary, cleanup) {
        (Err(primary), Err(cleanup)) => Err(attach_cleanup_failure(primary, cleanup, release)),
        (Err(primary), Ok(())) => Err(primary),
        (Ok(()), Err(cleanup)) => Err(cleanup),
        (Ok(()), Ok(())) => Ok(()),
    }
}

fn preserve_primary_after_cleanup(primary: GlassError, cleanup: crate::Result<()>) -> GlassError {
    match cleanup {
        Ok(()) => primary,
        Err(cleanup) => attach_cleanup_failure(primary, cleanup, "releasing the chord"),
    }
}

fn cleanup_chord<S: ChordSink>(
    sink: &mut S,
    key_down: bool,
    modifiers_down: bool,
) -> crate::Result<()> {
    // A possible press requires key-then-modifier cleanup even after expiry.
    let key = if key_down { sink.key(false) } else { Ok(()) };
    if modifiers_down {
        combine_cleanup(key, sink.modifiers(false), "releasing the chord modifiers")
    } else {
        key
    }
}

/// Drive the legacy modifier-key sequence until `deadline`, attempting key-then-modifier cleanup
/// after any possible press; failed cleanup can leave native input held.
pub fn run_chord_by<S: ChordSink>(sink: &mut S, deadline: Deadline) -> crate::Result<()> {
    require_time(deadline, false)?;

    let modifiers_down = true;
    if let Err(error) = sink.modifiers(true) {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    if let Err(error) = sleep_by(deadline, CHORD_DWELL) {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    let key_down = true;
    if let Err(error) = sink.key(true) {
        let cleanup = cleanup_chord(sink, key_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_chord(sink, key_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_chord(sink, key_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    let key_result = sink.key(false);
    let key_deadline = require_time(deadline, true);
    if let Err(error) = key_result {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = key_deadline {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    if let Err(error) = sleep_by(deadline, CHORD_DWELL) {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_chord(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    sink.modifiers(false)?;
    require_time(deadline, true)
}

#[cfg(test)]
mod tests {
    use super::{CHORD_DWELL, ChordSink, run_chord, run_chord_by, sleep_duration};
    use crate::{BoundDispatch, BoundKind, Deadline, GlassError, Result};
    use std::time::{Duration, Instant};

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
        assert_eq!(
            sink.calls,
            vec![Mods(true), Key(true), Key(false), Mods(false)]
        );
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

    #[test]
    fn run_chord_by_spent_deadline_emits_no_events() {
        let mut sink = RecordingSink::default();
        let deadline = Deadline::at(Instant::now() - Duration::from_millis(1));

        let error = run_chord_by(&mut sink, deadline).unwrap_err();

        assert!(sink.calls.is_empty());
        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn run_chord_by_deadline_expiring_mid_sequence_stops_before_the_next_event() {
        use Call::*;
        let mut full_sink = RecordingSink::default();
        run_chord(&mut full_sink).unwrap();
        let full_sequence = full_sink.calls;

        let mut sink = RecordingSink::default();
        let error = run_chord_by(&mut sink, Deadline::from_millis(10)).unwrap_err();

        assert!(sink.calls.len() < full_sequence.len());
        assert_eq!(sink.calls, vec![Mods(true), Mods(false)]);
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn a_bounded_dwell_uses_only_the_remaining_time() {
        assert_eq!(
            sleep_duration(Some(Duration::from_millis(10)), CHORD_DWELL),
            Duration::from_millis(10)
        );
        assert_eq!(sleep_duration(None, CHORD_DWELL), CHORD_DWELL);
    }

    #[test]
    fn run_chord_by_unbounded_wrapper_preserves_the_legacy_sequence() {
        use Call::*;
        let mut sink = RecordingSink::default();

        run_chord(&mut sink).unwrap();

        assert_eq!(
            sink.calls,
            vec![Mods(true), Key(true), Key(false), Mods(false)]
        );
    }

    #[test]
    fn run_chord_by_expiry_after_a_sink_event_is_may_have_dispatched() {
        struct SlowModifierSink(RecordingSink);

        impl ChordSink for SlowModifierSink {
            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)?;
                if down {
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                } else {
                    Err(GlassError::Backend("modifier cleanup failed".into()))
                }
            }

            fn key(&mut self, down: bool) -> Result<()> {
                self.0.key(down)
            }
        }

        let mut sink = SlowModifierSink(RecordingSink::default());
        let error = run_chord_by(&mut sink, Deadline::from_millis(5)).unwrap_err();

        assert_eq!(sink.0.calls, vec![Call::Mods(true), Call::Mods(false)]);
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("modifier cleanup failed"));
    }

    #[test]
    fn run_chord_by_attempts_modifier_cleanup_when_key_release_fails() {
        struct FailingKeyReleaseSink(RecordingSink);

        impl ChordSink for FailingKeyReleaseSink {
            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)?;
                if down {
                    Ok(())
                } else {
                    Err(GlassError::Backend("modifier cleanup failed".into()))
                }
            }

            fn key(&mut self, down: bool) -> Result<()> {
                self.0.key(down)?;
                if down {
                    Ok(())
                } else {
                    Err(GlassError::Backend("key release failed".into()))
                }
            }
        }

        let mut sink = FailingKeyReleaseSink(RecordingSink::default());
        let error = run_chord_by(&mut sink, Deadline::UNBOUNDED).unwrap_err();

        assert!(error.to_string().contains("key release failed"));
        assert!(error.to_string().contains("modifier cleanup failed"));
        let GlassError::InputCleanupFailed {
            operation,
            primary,
            cleanup,
        } = error
        else {
            panic!("both chord release failures must stay structured");
        };
        assert_eq!(operation, "releasing the chord");
        assert!(
            matches!(*primary, GlassError::Backend(message) if message == "key release failed")
        );
        assert!(
            matches!(*cleanup, GlassError::Backend(message) if message == "modifier cleanup failed")
        );
        assert_eq!(sink.0.calls.last(), Some(&Call::Mods(false)));
    }
}
