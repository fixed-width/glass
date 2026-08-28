//! Platform-agnostic drag model: path interpolation, pacing, and the `run_drag`
//! driver that sequences a drag against any backend's `DragSink`.

use std::time::Duration;

use crate::{Deadline, GlassError};

/// Points to warp the pointer through for a straight drag from `from` to `to`,
/// linearly interpolated at ~1px along the dominant axis (capped at
/// `MAX_STEPS`). Endpoints are exact (`path[0] == from`, `path[last] == to`); a
/// zero-length drag yields a single point.
pub(crate) fn drag_path(from: (i32, i32), to: (i32, i32)) -> Vec<(i32, i32)> {
    const MAX_STEPS: i64 = 512;
    let dx = (to.0 - from.0) as i64;
    let dy = (to.1 - from.1) as i64;
    let dist = dx.abs().max(dy.abs());
    if dist == 0 {
        return vec![from];
    }
    let steps = dist.min(MAX_STEPS);
    (0..=steps)
        .map(|i| {
            let x = from.0 as i64 + dx * i / steps;
            let y = from.1 as i64 + dy * i / steps;
            (x as i32, y as i32)
        })
        .collect()
}

/// Resample a straight drag from `from` to `to` into a bounded set of waypoints
/// spread over `duration_ms`, returning the waypoints (first == `from`, last ==
/// `to`, monotonic) and the delay to sleep between consecutive waypoints. Pacing
/// the motion over wall-clock time lets a frame-based client (egui/winit) sample
/// the path across multiple repaint frames instead of coalescing it into one
/// (which loses the path, and under continuous repaint drops the drag entirely).
pub(crate) fn drag_schedule(
    from: (i32, i32),
    to: (i32, i32),
    duration_ms: u64,
) -> (Vec<(i32, i32)>, Duration) {
    const STEP_MS: u64 = 16; // ~one 60Hz frame
    const MIN_WAYPOINTS: usize = 10;
    let full = drag_path(from, to);
    if full.len() <= 1 {
        return (full, Duration::ZERO);
    }
    // Target ~one waypoint per frame of the requested duration, but never fewer
    // than MIN_WAYPOINTS, and never more points than the path actually has.
    let target = (duration_ms / STEP_MS).max(1) as usize;
    let n = target.max(MIN_WAYPOINTS).min(full.len());
    let last = full.len() - 1;
    let waypoints: Vec<(i32, i32)> = (0..n).map(|i| full[i * last / (n - 1)]).collect();
    let step = Duration::from_millis(duration_ms / n as u64);
    (waypoints, step)
}

/// A fully-timed straight-line drag: the waypoints to trace (window-relative,
/// `first == from`, `last == to`), the pacing sleep between consecutive
/// waypoints, and the dwell to hold at the destination before releasing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DragGesture {
    pub waypoints: Vec<(i32, i32)>,
    pub step: Duration,
    pub dwell: Duration,
}

impl DragGesture {
    /// Plan a drag from `from` to `to` over `duration_ms`: `drag_schedule` for the
    /// waypoints + pacing, plus the fixed endpoint-dwell policy.
    pub fn plan(from: (i32, i32), to: (i32, i32), duration_ms: u64) -> Self {
        /// Hold at the destination before release, ~3 frames at 60Hz. Guarantees the
        /// app renders at least one frame with the pointer AT the endpoint and the
        /// button still down before the release, so the drop lands on target.
        const DWELL_MS: u64 = 48;
        let (waypoints, step) = drag_schedule(from, to, duration_ms);
        Self {
            waypoints,
            step,
            dwell: Duration::from_millis(DWELL_MS),
        }
    }
}

/// The per-backend primitives that `run_drag` sequences. Each method that emits
/// events is **self-committed**: it performs the backend's commit barrier before
/// returning (X11 `XFlush`; Wayland `frame` + roundtrip + settle; Windows one
/// `SendInput` per call). `modifiers` is the exception — it emits and commits
/// nothing when the gesture carries no modifiers. `run_drag` therefore owns only
/// ordering and wall-clock pacing.
pub trait DragSink {
    /// Place the pointer at the start point, ensuring the surface under it will
    /// receive the subsequent press/motion (backends needing a focus-assert nudge
    /// do it here). Called once, first.
    fn place(&mut self, x: i32, y: i32) -> crate::Result<()>;
    /// Emit a pointer-motion event to `(x, y)`. `run_drag` only calls this between
    /// `button(true)` and `button(false)`, so the motion carries the held button.
    fn move_to(&mut self, x: i32, y: i32) -> crate::Result<()>;
    /// Press (`down == true`) or release the drag button.
    fn button(&mut self, down: bool) -> crate::Result<()>;
    /// Press or release the gesture's modifiers. Always called by `run_drag`
    /// (once down before the press, once up after the release); the backend
    /// no-ops when the gesture carries no modifiers.
    fn modifiers(&mut self, down: bool) -> crate::Result<()>;
}

/// Drive a planned drag without a caller deadline.
pub fn run_drag<S: DragSink>(sink: &mut S, gesture: &DragGesture) -> crate::Result<()> {
    run_drag_by(sink, gesture, Deadline::UNBOUNDED)
}

fn require_time(deadline: Deadline, started: bool) -> crate::Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    if started {
        Err(GlassError::caller_deadline_elapsed("drag"))
    } else {
        Err(GlassError::deadline_not_started("drag"))
    }
}

fn sleep_by(deadline: Deadline, requested: Duration) -> crate::Result<()> {
    require_time(deadline, true)?;
    let sleep_for = deadline.remaining().unwrap_or(requested).min(requested);
    std::thread::sleep(sleep_for);
    require_time(deadline, true)
}

fn attach_cleanup_failure(
    mut primary: GlassError,
    cleanup: GlassError,
    release: &str,
) -> GlassError {
    let note = format!("; cleanup failed while releasing {release}: {cleanup}");
    match &mut primary {
        GlassError::Backend(message) | GlassError::Bounded { message, .. } => {
            message.push_str(&note);
        }
        _ => eprintln!("glass-core: {primary}{note}"),
    }
    primary
}

fn combine_cleanup(
    primary: crate::Result<()>,
    cleanup: crate::Result<()>,
    release: &str,
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
        Err(cleanup) => attach_cleanup_failure(primary, cleanup, "the drag"),
    }
}

fn cleanup_drag<S: DragSink>(
    sink: &mut S,
    button_down: bool,
    modifiers_down: bool,
) -> crate::Result<()> {
    // Releases are safety cleanup, not continued gesture dispatch. Once a press may
    // have landed they must still be attempted after expiry, in dependency order.
    let button = if button_down {
        sink.button(false)
    } else {
        Ok(())
    };
    if modifiers_down {
        combine_cleanup(button, sink.modifiers(false), "the drag modifiers")
    } else {
        button
    }
}

/// Drive a planned drag against a backend `sink`, stopping at `deadline`.
///
/// Every gesture event is checked immediately before and after dispatch, and
/// pacing sleeps never exceed the remaining caller budget. If a failure occurs
/// after a button or modifier press may have landed, releases are attempted in
/// button-then-modifier order. A cleanup dispatch can itself fail, so native input
/// may remain held after an unsuccessful release attempt.
pub fn run_drag_by<S: DragSink>(
    sink: &mut S,
    gesture: &DragGesture,
    deadline: Deadline,
) -> crate::Result<()> {
    let (start, rest) = gesture
        .waypoints
        .split_first()
        .expect("a drag gesture always has at least one waypoint");
    let end = *gesture.waypoints.last().expect("non-empty waypoints");

    require_time(deadline, false)?;
    sink.place(start.0, start.1)?;
    require_time(deadline, true)?;

    require_time(deadline, true)?;
    let modifiers_down = true;
    if let Err(error) = sink.modifiers(true) {
        let cleanup = cleanup_drag(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_drag(sink, false, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    require_time(deadline, true)?;
    let button_down = true;
    if let Err(error) = sink.button(true) {
        let cleanup = cleanup_drag(sink, button_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_drag(sink, button_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    for &(px, py) in rest {
        if let Err(error) = sleep_by(deadline, gesture.step) {
            let cleanup = cleanup_drag(sink, button_down, modifiers_down);
            return Err(preserve_primary_after_cleanup(error, cleanup));
        }
        if let Err(error) = sink.move_to(px, py) {
            let cleanup = cleanup_drag(sink, button_down, modifiers_down);
            return Err(preserve_primary_after_cleanup(error, cleanup));
        }
        if let Err(error) = require_time(deadline, true) {
            let cleanup = cleanup_drag(sink, button_down, modifiers_down);
            return Err(preserve_primary_after_cleanup(error, cleanup));
        }
    }

    if let Err(error) = sleep_by(deadline, gesture.dwell) {
        let cleanup = cleanup_drag(sink, button_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = sink.move_to(end.0, end.1) {
        let cleanup = cleanup_drag(sink, button_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_drag(sink, button_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }

    if let Err(error) = require_time(deadline, true) {
        let cleanup = cleanup_drag(sink, button_down, modifiers_down);
        return Err(preserve_primary_after_cleanup(error, cleanup));
    }
    let button_result = sink.button(false);
    let button_deadline = require_time(deadline, true);

    // Always release modifiers after the button release attempt. This ordering is
    // part of the gesture contract even when the first cleanup event fails.
    let modifier_pre_deadline = require_time(deadline, true);
    let modifier_result = sink.modifiers(false);
    let modifier_deadline = require_time(deadline, true);

    let before_modifier_cleanup = button_result
        .and(button_deadline)
        .and(modifier_pre_deadline);
    combine_cleanup(
        before_modifier_cleanup,
        modifier_result,
        "the drag modifiers",
    )
    .and(modifier_deadline)
}

#[cfg(test)]
mod tests {
    use super::drag_path;
    use super::drag_schedule;
    use std::time::Duration;

    #[test]
    fn plan_has_positive_dwell_and_exact_endpoints() {
        let g = super::DragGesture::plan((0, 0), (1000, 0), 200);
        assert!(
            g.dwell > Duration::ZERO,
            "every drag must dwell before release"
        );
        assert_eq!(g.waypoints.first(), Some(&(0, 0)));
        assert_eq!(g.waypoints.last(), Some(&(1000, 0)));
        assert_eq!(g.step, super::drag_schedule((0, 0), (1000, 0), 200).1);
    }

    #[test]
    fn horizontal_path_steps_one_pixel() {
        assert_eq!(
            drag_path((0, 0), (5, 0)),
            vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]
        );
    }

    #[test]
    fn diagonal_path_is_one_to_one() {
        assert_eq!(
            drag_path((0, 0), (3, 3)),
            vec![(0, 0), (1, 1), (2, 2), (3, 3)]
        );
    }

    #[test]
    fn backward_path_interpolates_with_exact_endpoints() {
        let p = drag_path((5, 5), (0, 5));
        assert_eq!(p.first(), Some(&(5, 5)));
        assert_eq!(p.last(), Some(&(0, 5)));
        assert_eq!(p.len(), 6);
    }

    #[test]
    fn zero_length_is_single_point() {
        assert_eq!(drag_path((2, 2), (2, 2)), vec![(2, 2)]);
    }

    #[test]
    fn long_drag_is_capped_with_exact_endpoints() {
        let p = drag_path((0, 0), (10_000, 0));
        assert_eq!(p.len(), 513); // MAX_STEPS = 512 -> 513 points
        assert_eq!(p.first(), Some(&(0, 0)));
        assert_eq!(p.last(), Some(&(10_000, 0)));
    }

    #[test]
    fn schedule_endpoints_exact_and_bounded() {
        // Long drag, 200ms: ~200/16 = 12 waypoints, endpoints exact.
        let (wp, step) = drag_schedule((0, 0), (1000, 0), 200);
        assert_eq!(wp.first(), Some(&(0, 0)));
        assert_eq!(wp.last(), Some(&(1000, 0)));
        assert_eq!(wp.len(), 12, "200ms / 16ms ≈ 12 waypoints");
        assert_eq!(step, Duration::from_millis(200 / 12));
        assert!(wp.windows(2).all(|w| w[1].0 >= w[0].0)); // monotonic x
    }

    #[test]
    fn schedule_min_waypoints_floor() {
        // Tiny duration still yields at least MIN_WAYPOINTS (10) for a long path.
        let (wp, _) = drag_schedule((0, 0), (1000, 0), 1);
        assert_eq!(wp.len(), 10);
    }

    #[test]
    fn schedule_short_drag_uses_all_points() {
        // 5px path has 6 points (< MIN_WAYPOINTS) -> use all 6, endpoints exact.
        let (wp, _) = drag_schedule((0, 0), (5, 0), 200);
        assert_eq!(wp, vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]);
    }

    #[test]
    fn schedule_more_waypoints_for_longer_duration() {
        let (short, _) = drag_schedule((0, 0), (1000, 0), 100);
        let (long, _) = drag_schedule((0, 0), (1000, 0), 400);
        assert!(
            long.len() > short.len(),
            "longer duration must yield more waypoints: {} vs {}",
            long.len(),
            short.len()
        );
    }

    #[test]
    fn schedule_zero_length_single_point_no_delay() {
        let (wp, step) = drag_schedule((2, 2), (2, 2), 200);
        assert_eq!(wp, vec![(2, 2)]);
        assert_eq!(step, Duration::ZERO);
    }
}

#[cfg(test)]
mod run_drag_tests {
    use super::{DragGesture, DragSink, run_drag, run_drag_by};
    use crate::{BoundDispatch, BoundKind, Deadline, GlassError, Result};
    use std::time::{Duration, Instant};

    #[derive(Debug, PartialEq)]
    enum Call {
        Place(i32, i32),
        Move(i32, i32),
        Button(bool),
        Mods(bool),
    }

    #[derive(Default)]
    struct RecordingSink {
        calls: Vec<Call>,
    }
    impl DragSink for RecordingSink {
        fn place(&mut self, x: i32, y: i32) -> Result<()> {
            self.calls.push(Call::Place(x, y));
            Ok(())
        }
        fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
            self.calls.push(Call::Move(x, y));
            Ok(())
        }
        fn button(&mut self, down: bool) -> Result<()> {
            self.calls.push(Call::Button(down));
            Ok(())
        }
        fn modifiers(&mut self, down: bool) -> Result<()> {
            self.calls.push(Call::Mods(down));
            Ok(())
        }
    }

    fn gesture(waypoints: Vec<(i32, i32)>) -> DragGesture {
        DragGesture {
            waypoints,
            step: Duration::ZERO,
            dwell: Duration::ZERO,
        }
    }

    #[test]
    fn ends_with_endpoint_reassert_then_release() {
        use Call::*;
        let mut sink = RecordingSink::default();
        run_drag(&mut sink, &gesture(vec![(0, 0), (5, 0), (10, 0)])).unwrap();
        assert_eq!(
            sink.calls,
            vec![
                Place(0, 0),
                Mods(true),
                Button(true),
                Move(5, 0),
                Move(10, 0), // last paced waypoint
                Move(10, 0), // re-assert at the exact endpoint, after the dwell
                Button(false),
                Mods(false),
            ]
        );
    }

    #[test]
    fn zero_length_drag_is_inplace_drag() {
        // A single-waypoint gesture is a press + release at the same point (with the
        // dwell/re-assert collapsed onto the start) — an in-place drag, not a click event.
        use Call::*;
        let mut sink = RecordingSink::default();
        run_drag(&mut sink, &gesture(vec![(3, 3)])).unwrap();
        assert_eq!(
            sink.calls,
            vec![
                Place(3, 3),
                Mods(true),
                Button(true),
                Move(3, 3),
                Button(false),
                Mods(false)
            ],
        );
    }

    #[test]
    fn run_drag_sleeps_the_dwell() {
        // The dwell is the hold at the destination that makes a frame-based GUI register
        // the drop at the endpoint. With `step == 0` the dwell is the only wall-clock cost,
        // so a run that elapses >= dwell proves the hold actually happens (`thread::sleep`
        // never returns early, so this can't flake on the `>=` side). The `_reassert_`
        // test pins that the re-assert/release come after it.
        use std::time::Instant;
        let g = DragGesture {
            waypoints: vec![(0, 0), (10, 0)],
            step: Duration::ZERO,
            dwell: Duration::from_millis(25),
        };
        let mut sink = RecordingSink::default();
        let started = Instant::now();
        run_drag(&mut sink, &g).unwrap();
        assert!(
            started.elapsed() >= Duration::from_millis(25),
            "run_drag must sleep the dwell (only {:?} elapsed)",
            started.elapsed()
        );
    }

    #[test]
    fn run_drag_by_spent_deadline_emits_no_events() {
        let mut sink = RecordingSink::default();
        let deadline = Deadline::at(Instant::now() - Duration::from_millis(1));

        let error = run_drag_by(&mut sink, &gesture(vec![(0, 0), (10, 0)]), deadline).unwrap_err();

        assert!(sink.calls.is_empty());
        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn run_drag_by_deadline_expiring_mid_sequence_stops_before_the_next_event() {
        use Call::*;
        let gesture = DragGesture {
            waypoints: vec![(0, 0), (5, 0), (10, 0)],
            step: Duration::from_millis(250),
            dwell: Duration::ZERO,
        };
        let mut full_sink = RecordingSink::default();
        run_drag(&mut full_sink, &gesture).unwrap();
        let full_sequence = full_sink.calls;

        let mut sink = RecordingSink::default();
        let started = Instant::now();
        let error = run_drag_by(&mut sink, &gesture, Deadline::from_millis(10)).unwrap_err();

        assert!(sink.calls.len() < full_sequence.len());
        assert_eq!(
            sink.calls,
            vec![
                Place(0, 0),
                Mods(true),
                Button(true),
                Button(false),
                Mods(false)
            ]
        );
        assert!(
            started.elapsed() < gesture.step,
            "the inter-waypoint sleep must be capped by the deadline"
        );
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn run_drag_by_unbounded_wrapper_preserves_the_legacy_sequence() {
        use Call::*;
        let mut sink = RecordingSink::default();

        run_drag(&mut sink, &gesture(vec![(0, 0), (5, 0), (10, 0)])).unwrap();

        assert_eq!(
            sink.calls,
            vec![
                Place(0, 0),
                Mods(true),
                Button(true),
                Move(5, 0),
                Move(10, 0),
                Move(10, 0),
                Button(false),
                Mods(false),
            ]
        );
    }

    #[test]
    fn run_drag_by_expiry_after_a_sink_event_is_may_have_dispatched() {
        struct SlowPlaceSink(RecordingSink);

        impl DragSink for SlowPlaceSink {
            fn place(&mut self, x: i32, y: i32) -> Result<()> {
                self.0.place(x, y)?;
                std::thread::sleep(Duration::from_millis(20));
                Ok(())
            }

            fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
                self.0.move_to(x, y)
            }

            fn button(&mut self, down: bool) -> Result<()> {
                self.0.button(down)
            }

            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)
            }
        }

        let mut sink = SlowPlaceSink(RecordingSink::default());
        let error = run_drag_by(
            &mut sink,
            &gesture(vec![(0, 0), (10, 0)]),
            Deadline::from_millis(5),
        )
        .unwrap_err();

        assert_eq!(sink.0.calls, vec![Call::Place(0, 0)]);
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn run_drag_by_attempts_modifier_cleanup_when_button_release_fails() {
        struct FailingReleaseSink(RecordingSink);

        impl DragSink for FailingReleaseSink {
            fn place(&mut self, x: i32, y: i32) -> Result<()> {
                self.0.place(x, y)
            }

            fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
                self.0.move_to(x, y)
            }

            fn button(&mut self, down: bool) -> Result<()> {
                self.0.button(down)?;
                if down {
                    Ok(())
                } else {
                    Err(GlassError::Backend("button release failed".into()))
                }
            }

            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)?;
                if down {
                    Ok(())
                } else {
                    Err(GlassError::Backend("modifier cleanup failed".into()))
                }
            }
        }

        let mut sink = FailingReleaseSink(RecordingSink::default());
        let error = run_drag_by(
            &mut sink,
            &gesture(vec![(0, 0), (10, 0)]),
            Deadline::UNBOUNDED,
        )
        .unwrap_err();

        assert!(error.to_string().contains("button release failed"));
        assert!(error.to_string().contains("modifier cleanup failed"));
        assert_eq!(sink.0.calls.last(), Some(&Call::Mods(false)));
    }

    #[test]
    fn run_drag_by_preserves_deadline_provenance_when_cleanup_fails() {
        struct SlowModifierCleanupSink(RecordingSink);

        impl DragSink for SlowModifierCleanupSink {
            fn place(&mut self, x: i32, y: i32) -> Result<()> {
                self.0.place(x, y)
            }

            fn move_to(&mut self, x: i32, y: i32) -> Result<()> {
                self.0.move_to(x, y)
            }

            fn button(&mut self, down: bool) -> Result<()> {
                self.0.button(down)
            }

            fn modifiers(&mut self, down: bool) -> Result<()> {
                self.0.modifiers(down)?;
                if down {
                    std::thread::sleep(Duration::from_millis(20));
                    Ok(())
                } else {
                    Err(GlassError::Backend("modifier cleanup failed".into()))
                }
            }
        }

        let mut sink = SlowModifierCleanupSink(RecordingSink::default());
        let error = run_drag_by(
            &mut sink,
            &gesture(vec![(0, 0), (10, 0)]),
            Deadline::from_millis(5),
        )
        .unwrap_err();

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(crate::Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        assert!(error.to_string().contains("modifier cleanup failed"));
    }
}
