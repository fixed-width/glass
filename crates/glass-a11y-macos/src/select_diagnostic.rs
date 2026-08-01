#![forbid(unsafe_code)]
//! Rendering for the per-candidate diagnostic `reader::select_window` prints when no `AXWindow`
//! matched the geometry the display backend reported. Separate from `reader` (which is
//! `#[cfg(target_os = "macos")]` and needs a live `AXUIElement`) so the format that has to
//! survive a debugging session is unit-tested on any host.
//!
//! The fields are chosen from what a real failure needed and did not have (#263): the
//! candidate's role, because a withheld tree hands back an `AXApplication` where a window
//! belongs, and the raw `AXError` behind a failed read, which names a genuine AX failure in one
//! line. `ffi::copy_attribute` classifies `kAXErrorAttributeUnsupported`/`kAXErrorNoValue` as an
//! absent attribute rather than a failure, so that code (-25205) never reaches a diagnostic
//! line — that case is identified by role alone (see `SizeUnreadable`'s test below).

/// What reading one `AXWindow` candidate produced — one variant per point at which
/// `select_window` gives up on a candidate, plus the fully-measured case.
#[derive(Clone, Debug, PartialEq)]
pub enum CandidateOutcome {
    /// `AXSize` could not be read. Carries the error's own text — the `AXError` code for a
    /// genuine failure (see `ffi::ax_err`), or "attribute not present" with no code for the
    /// absent-attribute case `ffi::copy_attribute` intercepts before `ax_err` runs.
    SizeUnreadable(String),
    /// `AXSize` read back a zero or negative dimension.
    NonPositiveSize { ax_w: f64, ax_h: f64 },
    /// The width-derived point→pixel scale was not a usable positive finite number.
    InvalidScale { ax_w: f64, ax_h: f64, scale: f64 },
    /// `AXPosition` could not be read. Carries the error's own text, as `SizeUnreadable` does.
    PositionUnreadable {
        ax_w: f64,
        ax_h: f64,
        scale: f64,
        error: String,
    },
    /// The candidate was measured end-to-end: its scaled origin offsets from the target are
    /// `dx`/`dy`, in pixels.
    Measured {
        ax_x: f64,
        ax_y: f64,
        ax_w: f64,
        ax_h: f64,
        scale: f64,
        dx: i64,
        dy: i64,
    },
}

/// One candidate's diagnostic line. `role` is the candidate's `AXRole`, rendered `?` when it
/// could not be read — never omitted, so every line has the same leading field to grep for
/// (structurally: every arm below builds `detail` only, and `role={role}` is prepended once).
pub fn candidate_line(role: Option<&str>, outcome: &CandidateOutcome) -> String {
    let role = role.unwrap_or("?");
    let detail = match outcome {
        CandidateOutcome::SizeUnreadable(error) => format!("<AXSize unreadable: {error}>"),
        CandidateOutcome::NonPositiveSize { ax_w, ax_h } => {
            format!("ax_w={ax_w} ax_h={ax_h} <non-positive size>")
        }
        CandidateOutcome::InvalidScale { ax_w, ax_h, scale } => {
            format!("ax_w={ax_w} ax_h={ax_h} scale={scale} <invalid scale>")
        }
        CandidateOutcome::PositionUnreadable {
            ax_w,
            ax_h,
            scale,
            error,
        } => {
            format!("ax_w={ax_w} ax_h={ax_h} scale={scale} <AXPosition unreadable: {error}>")
        }
        CandidateOutcome::Measured {
            ax_x,
            ax_y,
            ax_w,
            ax_h,
            scale,
            dx,
            dy,
        } => {
            format!("ax=({ax_x}, {ax_y}, {ax_w}, {ax_h}) scale={scale} dx={dx} dy={dy}")
        }
    };
    format!("role={role} {detail}")
}

#[cfg(test)]
mod tests {
    use super::{CandidateOutcome, candidate_line};

    /// A locked screen hands back the *application* element where a window belongs, and
    /// `AXSize` fails `kAXErrorAttributeUnsupported` (-25205). `ffi::copy_attribute` classifies
    /// that code as an absent attribute rather than a failure, so the message it produces is
    /// "attribute not present" with no error code — role is what actually identifies this
    /// case, not the AXError text (contrast `an_unreadable_position_keeps_the_ax_error` below,
    /// whose fixture is a genuine failure). The fixture carries the `"backend error: "` prefix
    /// `GlassError::Backend`'s `Display` adds — `select_window` builds this string via
    /// `e.to_string()` on the `GlassError` `ffi::ax_size` returns, not the inner message alone.
    #[test]
    fn an_unreadable_size_from_a_withheld_tree_keeps_the_role_not_an_ax_error_code() {
        let line = candidate_line(
            Some("AXApplication"),
            &CandidateOutcome::SizeUnreadable(
                "backend error: AXSize: attribute not present".into(),
            ),
        );
        assert_eq!(
            line,
            "role=AXApplication <AXSize unreadable: backend error: AXSize: attribute not present>"
        );
    }

    /// A candidate whose role could not be read is still a candidate; it renders `?` rather
    /// than dropping the field, so every line has the same shape and is greppable.
    #[test]
    fn a_missing_role_renders_as_a_question_mark() {
        let line = candidate_line(None, &CandidateOutcome::SizeUnreadable("boom".into()));
        assert_eq!(line, "role=? <AXSize unreadable: boom>");
    }

    /// The measured line keeps the pre-#263 field order and spelling — the format a reader
    /// comparing today's log against the one pasted in the issue has to recognize — with the
    /// role prepended.
    #[test]
    fn a_measured_candidate_renders_geometry_scale_and_offsets() {
        let line = candidate_line(
            Some("AXWindow"),
            &CandidateOutcome::Measured {
                ax_x: 510.0,
                ax_y: 497.0,
                ax_w: 230.0,
                ax_h: 408.0,
                scale: 2.0,
                dx: 540,
                dy: 590,
            },
        );
        assert_eq!(
            line,
            "role=AXWindow ax=(510, 497, 230, 408) scale=2 dx=540 dy=590"
        );
    }

    #[test]
    fn a_non_positive_size_says_so() {
        let line = candidate_line(
            Some("AXWindow"),
            &CandidateOutcome::NonPositiveSize {
                ax_w: 0.0,
                ax_h: 12.0,
            },
        );
        assert_eq!(line, "role=AXWindow ax_w=0 ax_h=12 <non-positive size>");
    }

    /// `candidate_line` renders whatever `scale` it's given; it doesn't distinguish which kind
    /// of non-finite value `select_window` could actually produce (see the test below for that).
    #[test]
    fn an_invalid_scale_says_so() {
        let line = candidate_line(
            Some("AXWindow"),
            &CandidateOutcome::InvalidScale {
                ax_w: 230.0,
                ax_h: 408.0,
                scale: f64::NAN,
            },
        );
        assert_eq!(
            line,
            "role=AXWindow ax_w=230 ax_h=408 scale=NaN <invalid scale>"
        );
    }

    /// The non-finite value `select_window` can actually produce: `.max(1.0)` resolves a NaN
    /// dividend away (`f64::NAN.max(1.0) == 1.0`), so only an `ax_w` near zero — overflowing
    /// the division to `Infinity` — reaches `InvalidScale`, never `NaN`.
    #[test]
    fn an_invalid_scale_from_a_near_zero_width_says_so() {
        let line = candidate_line(
            Some("AXWindow"),
            &CandidateOutcome::InvalidScale {
                ax_w: 230.0,
                ax_h: 408.0,
                scale: f64::INFINITY,
            },
        );
        assert_eq!(
            line,
            "role=AXWindow ax_w=230 ax_h=408 scale=inf <invalid scale>"
        );
    }

    /// `PositionUnreadable`'s error text is threaded through the same way `SizeUnreadable`'s is,
    /// `"backend error: "` prefix included. This fixture (-25204, `kAXErrorCannotComplete`) is a
    /// genuine failure, not an absent-attribute one, so unlike the `SizeUnreadable` test above,
    /// its AXError code does survive into the line.
    #[test]
    fn an_unreadable_position_keeps_the_ax_error() {
        let line = candidate_line(
            Some("AXWindow"),
            &CandidateOutcome::PositionUnreadable {
                ax_w: 230.0,
                ax_h: 408.0,
                scale: 2.0,
                error: "backend error: AXPosition: AX call failed (AXError -25204)".into(),
            },
        );
        assert_eq!(
            line,
            "role=AXWindow ax_w=230 ax_h=408 scale=2 <AXPosition unreadable: backend error: \
             AXPosition: AX call failed (AXError -25204)>"
        );
    }
}
