//! Classify a two-pointer gesture as a pinch, or say why it is not one.
//!
//! Backend-agnostic geometry over [`Segment`], mirroring [`crate::drag`]: the plan is derived
//! here and actuated by whichever backend has a pinch primitive.

use crate::platform::Segment;

/// Below this fractional change in finger separation, a gesture is not a scale change.
///
/// Measured against idb companion 1.5.0b3 on iOS 26.5: a requested scale of 2.0 delivered 1.905
/// and 0.5 delivered 0.513, because `UIPinchGestureRecognizer` takes its reference distance at
/// its own `began`, already one interpolation step in. A request smaller than that error cannot
/// be told apart from it.
pub const SCALE_EPSILON: f64 = 0.05;

/// Midpoint travel (px) separating a two-finger pan from a rotation. Consulted only when the
/// separation held, so it discriminates those two and nothing else.
pub const PAN_MIN_PX: f64 = 4.0;

/// A two-finger pinch: where it is centred, how far apart the fingers start, and the factor
/// their separation changes by.
#[derive(Clone, Copy, Debug, PartialEq)]
pub struct Pinch {
    /// Midpoint of the two **start** points, window-relative px.
    pub center: (i32, i32),
    /// Half the starting separation, px.
    pub start_radius: f64,
    /// End separation divided by start separation. Above 1 the fingers spread.
    pub scale: f64,
}

/// Why a gesture is not a pinch. Each variant is a distinct gesture the caller asked for, so a
/// backend can name the one it refused rather than answering "unsupported".
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NotAPinch {
    /// Not exactly two pointers; carries how many there were.
    PointerCount(usize),
    /// Both fingers start at the same point, leaving no separation to scale.
    Coincident,
    /// Separation held while the midpoint travelled: a two-finger pan.
    Pan,
    /// Separation and midpoint both held while the finger axis turned.
    Rotation,
    /// Neither finger moved.
    Stationary,
}

impl NotAPinch {
    /// A phrase naming what the caller asked for, for an error a user reads.
    pub fn describe(self) -> String {
        match self {
            NotAPinch::PointerCount(n) => {
                format!("a {n}-pointer gesture; only a 2-finger pinch can be actuated")
            }
            NotAPinch::Coincident => {
                "both fingers start at the same point, so there is no separation to scale"
                    .to_string()
            }
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
    /// A change in separation is what makes a pinch; the axis it happens on does not, because a
    /// pinch recognizer reports scale alone. An asymmetric pair — one finger held, the other
    /// travelling — is therefore still a pinch.
    pub fn classify(pointers: &[Segment]) -> Result<Pinch, NotAPinch> {
        let [a, b] = pointers else {
            return Err(NotAPinch::PointerCount(pointers.len()));
        };
        // Compared as integers: a zero separation would make `scale` a division by zero, and the
        // coordinates are exact, so this needs no float tolerance.
        if a.from_x == b.from_x && a.from_y == b.from_y {
            return Err(NotAPinch::Coincident);
        }
        let start = separation((a.from_x, a.from_y), (b.from_x, b.from_y));
        let end = separation((a.to_x, a.to_y), (b.to_x, b.to_y));
        let scale = end / start;
        if (scale - 1.0).abs() < SCALE_EPSILON {
            return Err(separation_held(a, b));
        }
        Ok(Pinch {
            center: midpoint((a.from_x, a.from_y), (b.from_x, b.from_y)),
            start_radius: start / 2.0,
            scale,
        })
    }
}

/// Which non-pinch a constant-separation pair is: nothing moved, the pair travelled, or it turned.
fn separation_held(a: &Segment, b: &Segment) -> NotAPinch {
    let still = |s: &Segment| s.from_x == s.to_x && s.from_y == s.to_y;
    if still(a) && still(b) {
        return NotAPinch::Stationary;
    }
    let from = midpoint((a.from_x, a.from_y), (b.from_x, b.from_y));
    let to = midpoint((a.to_x, a.to_y), (b.to_x, b.to_y));
    if separation(from, to) > PAN_MIN_PX {
        NotAPinch::Pan
    } else {
        NotAPinch::Rotation
    }
}

/// Distance between two window-relative points, px.
fn separation(a: (i32, i32), b: (i32, i32)) -> f64 {
    f64::from(a.0 - b.0).hypot(f64::from(a.1 - b.1))
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
        // Fingers at x=100 and x=300 (y=400) move out to x=50 and x=350.
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
        // The pair swaps ends about a fixed midpoint: separation held, axis turned.
        let g = [seg((100, 400), (300, 400)), seg((300, 400), (100, 400))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Rotation));
    }

    #[test]
    fn two_fingers_that_never_move_are_stationary() {
        let g = [seg((100, 400), (100, 400)), seg((300, 400), (300, 400))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Stationary));
    }

    #[test]
    fn a_separation_change_under_the_epsilon_is_not_a_scale_change() {
        // 200 -> 204 is 2%, inside SCALE_EPSILON, and the midpoint holds: a rotation-shaped
        // no-op rather than a pinch glass would be unable to deliver faithfully.
        let g = [seg((100, 400), (98, 400)), seg((300, 400), (302, 400))];
        assert_eq!(Pinch::classify(&g), Err(NotAPinch::Rotation));
    }

    #[test]
    fn describe_names_what_the_caller_asked_for() {
        assert!(NotAPinch::PointerCount(3).describe().contains('3'));
        assert!(NotAPinch::Pan.describe().contains("pan"));
        assert!(NotAPinch::Rotation.describe().contains("rotation"));
        assert!(NotAPinch::Coincident.describe().contains("same point"));
        assert!(NotAPinch::Stationary.describe().contains("did not move"));
    }
}
