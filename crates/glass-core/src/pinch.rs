//! Classify a two-pointer gesture as a pinch, or say why it is not one.
//!
//! Backend-agnostic geometry over [`Segment`]. Unlike [`crate::drag`], which drives a backend
//! through a sink, this module only derives the plan; the backend actuates it.

use crate::platform::Segment;

/// Below this fractional change in finger separation, a gesture is not a scale change.
/// Compared against `|scale - 1.0|` — an absolute deviation from 1, not a percentage.
///
/// Measured against idb companion 1.5.0b3 on iOS 26.5: a requested 2.0 arrived as 1.905 and a
/// requested 0.5 as 0.513 at an 80pt start radius, and the same 2.0 as 1.333 at 8pt. A request
/// smaller than that delivery error cannot be told apart from it.
pub const SCALE_EPSILON: f64 = 0.05;

/// The band around 1 that [`SCALE_EPSILON`] describes, written out rather than computed.
///
/// Do not go back to comparing `(scale - 1.0).abs()` against the epsilon: `1.05 - 1.0` is not
/// `0.05` in `f64`, so that boundary is one no input can land on. `scale` itself reaches exactly
/// `1.05` and `0.95` — 200px separating to 210 or 190. `scale_bounds_match_the_epsilon` keeps
/// them in step.
pub const SCALE_MAX: f64 = 1.05;
/// The lower end of the band; see [`SCALE_MAX`].
pub const SCALE_MIN: f64 = 0.95;

/// A two-finger pinch: where it is centred, how far apart the fingers start, and the factor
/// their separation changes by.
///
/// `#[non_exhaustive]` so [`Pinch::classify`] stays the only way to build one — the field
/// invariants below all come from it.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub struct Pinch {
    /// Midpoint of the two **start** points, window-relative px.
    pub center: (i32, i32),
    /// Half the starting separation, px. Always finite and greater than zero — `classify`
    /// rejects coincident start points, and distinct `i32` points are at least 1px apart.
    pub start_radius: f64,
    /// End separation divided by start separation. Above 1 the fingers spread.
    ///
    /// Always finite and non-negative, and at least [`SCALE_EPSILON`] away from 1. Exactly 0
    /// when both fingers converge on one point, which is a real gesture rather than an error:
    /// idb actuates it as a pinch all the way in.
    pub scale: f64,
}

/// Why a gesture is not a pinch, named specifically enough that a backend's `Unsupported`
/// error can say what it refused instead of only that it refused.
#[derive(Clone, Copy, Debug, PartialEq)]
#[non_exhaustive]
pub enum NotAPinch {
    /// Not exactly two pointers; carries how many there were.
    PointerCount(usize),
    /// Both fingers start at the same point, leaving no separation to scale.
    Coincident,
    /// The separation changed by less than [`SCALE_EPSILON`]; carries the requested factor.
    /// A pinch, but a smaller one than can be delivered faithfully.
    ScaleTooSmall(f64),
    /// Separation held while the pair travelled: a two-finger pan.
    Pan,
    /// Separation held while the axis through the fingers turned.
    Rotation,
    /// Neither finger moved.
    Stationary,
}

impl NotAPinch {
    /// A phrase naming what the caller asked for, for an error a user reads.
    pub fn describe(self) -> String {
        match self {
            NotAPinch::PointerCount(n) => {
                format!("a {n}-pointer gesture; only a two-finger pinch can be actuated")
            }
            NotAPinch::Coincident => {
                "both fingers start at the same point, so there is no separation to scale"
                    .to_string()
            }
            NotAPinch::ScaleTooSmall(scale) => format!(
                "a separation change of {:.1}%, under the {:.0}% that can be delivered \
                 faithfully — ask for a larger change",
                (scale - 1.0).abs() * 100.0,
                SCALE_EPSILON * 100.0
            ),
            NotAPinch::Pan => {
                "a two-finger pan, which is a different gesture from a pinch".to_string()
            }
            NotAPinch::Rotation => {
                "a rotation, which is a different gesture from a pinch".to_string()
            }
            NotAPinch::Stationary => "a gesture whose fingers did not move".to_string(),
        }
    }
}

impl Pinch {
    /// Classify `pointers` as a pinch, or return the reason it is not one.
    ///
    /// A change in separation is what makes a pinch; the axis it happens on does not, because
    /// what a pinch delivers is a scale factor about a centre. An asymmetric pair — one finger
    /// held, the other travelling — is therefore still a pinch.
    pub fn classify(pointers: &[Segment]) -> Result<Pinch, NotAPinch> {
        let [a, b] = pointers else {
            return Err(NotAPinch::PointerCount(pointers.len()));
        };
        // Compared as integers: a zero separation would make `scale` a division by zero, and
        // the coordinates are exact, so this needs no float tolerance.
        if a.from_x == b.from_x && a.from_y == b.from_y {
            return Err(NotAPinch::Coincident);
        }
        let start = separation((a.from_x, a.from_y), (b.from_x, b.from_y));
        let end = separation((a.to_x, a.to_y), (b.to_x, b.to_y));
        let scale = end / start;
        if scale > SCALE_MIN && scale < SCALE_MAX {
            return Err(separation_held(a, b, scale));
        }
        Ok(Pinch {
            center: midpoint((a.from_x, a.from_y), (b.from_x, b.from_y)),
            start_radius: start / 2.0,
            scale,
        })
    }
}

/// What a pair whose separation held actually is: nothing moved, the axis turned, the pair
/// travelled, or a pinch too small to deliver.
///
/// Do not go back to telling rotation from pan by midpoint travel: a rotation about one held
/// finger moves the midpoint exactly as a pan does.
fn separation_held(a: &Segment, b: &Segment, scale: f64) -> NotAPinch {
    let still = |s: &Segment| s.from_x == s.to_x && s.from_y == s.to_y;
    if still(a) && still(b) {
        return NotAPinch::Stationary;
    }
    if axis_turned(a, b) {
        return NotAPinch::Rotation;
    }
    // Separation and axis both held, so the pair is rigid; one that also went nowhere was
    // `Stationary` above.
    if midpoint((a.from_x, a.from_y), (b.from_x, b.from_y))
        != midpoint((a.to_x, a.to_y), (b.to_x, b.to_y))
    {
        return NotAPinch::Pan;
    }
    NotAPinch::ScaleTooSmall(scale)
}

/// Whether the axis through the two fingers turned by more than one pixel of perpendicular
/// travel at this separation.
///
/// One pixel is the finest distinction integer coordinates can express, so a turn below
/// `atan(1 / separation)` is rounding rather than a gesture.
fn axis_turned(a: &Segment, b: &Segment) -> bool {
    let angle = |from: (i32, i32), to: (i32, i32)| {
        let d = |p: i32, q: i32| (i64::from(p) - i64::from(q)) as f64;
        d(to.1, from.1).atan2(d(to.0, from.0))
    };
    let start = angle((a.from_x, a.from_y), (b.from_x, b.from_y));
    let end = angle((a.to_x, a.to_y), (b.to_x, b.to_y));
    let raw = (end - start).abs();
    // The short way round, as a min rather than a `> PI` branch: at exactly PI the arms agree
    // (`TAU - PI == PI`), so that comparison is undecidable by any test.
    let turn = raw.min(std::f64::consts::TAU - raw);
    turn > (1.0 / separation((a.from_x, a.from_y), (b.from_x, b.from_y))).atan()
}

/// Distance between two window-relative points, px. Widened to `i64` before subtracting so
/// extreme coordinates cannot overflow the intermediate, matching [`midpoint`].
fn separation(a: (i32, i32), b: (i32, i32)) -> f64 {
    let d = |p: i32, q: i32| (i64::from(p) - i64::from(q)) as f64;
    d(a.0, b.0).hypot(d(a.1, b.1))
}

/// Midpoint of two window-relative points. Summed as `i64` so extreme coordinates cannot
/// overflow the intermediate.
fn midpoint(a: (i32, i32), b: (i32, i32)) -> (i32, i32) {
    let mid = |p: i32, q: i32| ((i64::from(p) + i64::from(q)) / 2) as i32;
    (mid(a.0, b.0), mid(a.1, b.1))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn seg(from: (i32, i32), to: (i32, i32)) -> Segment {
        Segment {
            from_x: from.0,
            from_y: from.1,
            to_x: to.0,
            to_y: to.1,
        }
    }

    #[test]
    fn symmetric_spread_is_a_pinch_centred_on_the_start_midpoint() {
        let p = Pinch::classify(&[seg((100, 400), (50, 400)), seg((300, 400), (350, 400))])
            .expect("a symmetric spread is a pinch");
        assert_eq!(p.center, (200, 400));
        assert_eq!(p.start_radius, 100.0);
        assert_eq!(p.scale, 1.5);
    }

    #[test]
    fn converging_fingers_scale_below_one() {
        let p = Pinch::classify(&[seg((100, 400), (150, 400)), seg((300, 400), (250, 400))])
            .expect("a converging pair is a pinch");
        assert_eq!(p.scale, 0.5);
    }

    #[test]
    fn one_finger_held_still_is_a_pinch_not_a_pan() {
        // Asymmetric: only the right finger travels. Separation 200 -> 300.
        let p = Pinch::classify(&[seg((100, 400), (100, 400)), seg((300, 400), (400, 400))])
            .expect("an asymmetric spread is still a scale change");
        assert_eq!(p.center, (200, 400));
        assert_eq!(p.scale, 1.5);
    }

    #[test]
    fn a_vertical_pinch_classifies_the_same_as_a_horizontal_one() {
        let p = Pinch::classify(&[seg((400, 100), (400, 50)), seg((400, 300), (400, 350))])
            .expect("axis does not decide pinch-ness");
        assert_eq!(p.center, (400, 200));
        assert_eq!(p.scale, 1.5);
    }

    #[test]
    fn fingers_converging_on_one_point_scale_to_zero() {
        // The starts are distinct, so this is not `Coincident`; idb actuates it fully closed.
        let p = Pinch::classify(&[seg((100, 400), (200, 400)), seg((300, 400), (200, 400))])
            .expect("converging on one point is a pinch, not an error");
        assert_eq!(p.scale, 0.0);
        assert_eq!(p.start_radius, 100.0);
    }

    #[test]
    fn three_pointers_report_their_count() {
        let g = [
            seg((0, 0), (1, 1)),
            seg((2, 2), (3, 3)),
            seg((4, 4), (5, 5)),
        ];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::PointerCount(3)));
    }

    #[test]
    fn one_pointer_reports_its_count() {
        assert_eq!(
            Pinch::classify(&[seg((0, 0), (1, 1))]),
            Err(NotAPinch::PointerCount(1))
        );
    }

    #[test]
    fn fingers_starting_at_one_point_are_coincident() {
        // Scale would be a division by zero, so this must be caught before the ratio.
        let g = [seg((200, 400), (100, 400)), seg((200, 400), (300, 400))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Coincident));
    }

    #[test]
    fn parallel_travel_at_constant_separation_is_a_pan() {
        let g = [seg((100, 400), (100, 200)), seg((300, 400), (300, 200))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Pan));
    }

    #[test]
    fn a_turned_axis_at_constant_separation_is_a_rotation() {
        // The pair swaps ends: separation held, axis turned through pi.
        let g = [seg((100, 400), (300, 400)), seg((300, 400), (100, 400))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Rotation));
    }

    #[test]
    fn a_rotation_about_one_held_finger_is_a_rotation_not_a_pan() {
        // `b` swings a quarter turn about the pinned `a`, so the midpoint travels as a pan's
        // would — only the axis tells them apart.
        let g = [seg((100, 400), (100, 400)), seg((300, 400), (100, 600))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Rotation));
    }

    #[test]
    fn exactly_one_pixel_of_perpendicular_travel_is_not_a_rotation() {
        // `b` moves one pixel perpendicular at a separation of 200, so the turn is
        // `atan2(1, 200)` — bit-identical to the `atan(1 / 200)` tolerance, and the boundary is
        // therefore exactly constructible.
        let g = [seg((100, 400), (100, 400)), seg((300, 400), (300, 401))];
        assert!(
            matches!(Pinch::classify(&g), Err(NotAPinch::ScaleTooSmall(_))),
            "got {:?}",
            Pinch::classify(&g)
        );
    }

    #[test]
    fn two_fingers_that_never_move_are_stationary() {
        let g = [seg((100, 400), (100, 400)), seg((300, 400), (300, 400))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Stationary));
    }

    #[test]
    fn a_spread_under_the_epsilon_is_named_as_a_too_small_pinch() {
        // 200 -> 204 is 2%: a pinch, but smaller than can be delivered.
        let g = [seg((100, 400), (98, 400)), seg((300, 400), (302, 400))];
        let Err(NotAPinch::ScaleTooSmall(scale)) = Pinch::classify(&g) else {
            panic!("expected ScaleTooSmall, got {:?}", Pinch::classify(&g));
        };
        assert_eq!(scale, 1.02);
    }

    #[test]
    fn a_shrink_under_the_epsilon_is_also_a_too_small_pinch() {
        // The converging side of the band: 200 -> 196 is -2%.
        let g = [seg((100, 400), (102, 400)), seg((300, 400), (298, 400))];
        let Err(NotAPinch::ScaleTooSmall(scale)) = Pinch::classify(&g) else {
            panic!("expected ScaleTooSmall");
        };
        assert_eq!(scale, 0.98);
    }

    #[test]
    fn a_spread_just_outside_the_epsilon_is_a_pinch() {
        // `1.05 - 1.0` in f64 is 0.050000000000000044, not below the epsilon — the smallest
        // spread this accepts.
        let p = Pinch::classify(&[seg((100, 400), (95, 400)), seg((300, 400), (305, 400))])
            .expect("just outside the epsilon is a scale change");
        assert_eq!(p.scale, 1.05);
    }

    #[test]
    fn separation_does_not_overflow_on_extreme_coordinates() {
        // Subtracting in i32 panics in debug and wraps to a wrong answer in release.
        let g = [
            seg((i32::MAX, 0), (i32::MAX, 0)),
            seg((i32::MIN, 0), (i32::MIN, 0)),
        ];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Stationary));
        assert_eq!(
            separation((i32::MAX, 0), (i32::MIN, 0)),
            f64::from(u32::MAX),
            "the full i32 span, not a wrapped one"
        );
    }

    #[test]
    fn scale_bounds_match_the_epsilon() {
        // The bounds are literals so the comparison has a reachable boundary; this keeps them
        // honest about the epsilon they claim to be.
        assert!((SCALE_MAX - 1.0 - SCALE_EPSILON).abs() < 1e-12);
        assert!((1.0 - SCALE_MIN - SCALE_EPSILON).abs() < 1e-12);
    }

    #[test]
    fn a_shrink_to_exactly_the_lower_bound_is_a_pinch() {
        // 200 -> 190 is exactly 0.95 in f64, the boundary the band excludes.
        let p = Pinch::classify(&[seg((100, 400), (105, 400)), seg((300, 400), (295, 400))])
            .expect("exactly the lower bound is a scale change");
        assert_eq!(p.scale, SCALE_MIN);
    }

    #[test]
    fn a_pan_along_a_vertical_axis_is_a_pan() {
        // The axis is pi/2 rather than 0, so a sum in place of the difference no longer reads
        // as no turn at all.
        let g = [seg((100, 400), (150, 400)), seg((100, 600), (150, 600))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Pan));
    }

    #[test]
    fn a_small_rotation_is_still_a_rotation() {
        // A 0.1 rad swing at separation 200: far above the one-pixel tolerance, far below the
        // one a mistaken `1.0 % sep` or `1.0 * sep` would produce.
        let g = [seg((100, 400), (100, 400)), seg((300, 400), (299, 420))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Rotation));
    }

    #[test]
    fn an_axis_crossing_the_half_turn_mark_turns_the_short_way() {
        // The axis points left, so one pixel of perpendicular travel flips atan2 from +pi to
        // -pi — a raw difference of nearly a full turn. Taken the long way round it reads as a
        // rotation instead of a pan.
        let g = [seg((100, 400), (100, 400)), seg((60, 400), (60, 399))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Pan));
    }

    #[test]
    fn describe_names_what_the_caller_asked_for() {
        assert!(NotAPinch::PointerCount(3).describe().contains('3'));
        assert!(NotAPinch::Pan.describe().contains("pan"));
        assert!(NotAPinch::Rotation.describe().contains("rotation"));
        assert!(NotAPinch::Coincident.describe().contains("same point"));
        assert!(NotAPinch::Stationary.describe().contains("did not move"));
        let too_small = NotAPinch::ScaleTooSmall(1.02).describe();
        assert!(too_small.contains("2.0%"), "{too_small}");
        assert!(
            too_small.contains("larger"),
            "it must say what to do instead: {too_small}"
        );
    }
}
