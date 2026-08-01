#![forbid(unsafe_code)]
//! Rendering for the per-candidate diagnostic `reader::select_window` prints when no `AXWindow`
//! matched the geometry the display backend reported. Separate from `reader` (which is
//! `#[cfg(target_os = "macos")]` and needs a live `AXUIElement`) so the format that has to
//! survive a debugging session is unit-tested on any host.
//!
//! The fields are chosen from what a real failure needed and did not have (#263): the
//! candidate's role, because a withheld tree hands back an `AXApplication` where a window
//! belongs, and the raw `AXError` behind a failed read, because -25205 names that condition in
//! one line.

/// What reading one `AXWindow` candidate produced — one variant per point at which
/// `select_window` gives up on a candidate, plus the fully-measured case.
#[derive(Clone, Debug, PartialEq)]
pub enum CandidateOutcome {
    /// `AXSize` could not be read. Carries the error's own text, which names the `AXError`
    /// code (see `ffi::ax_err`).
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
/// could not be read — never omitted, so every line has the same leading field to grep for.
pub fn candidate_line(role: Option<&str>, outcome: &CandidateOutcome) -> String {
    let role = role.unwrap_or("?");
    match outcome {
        CandidateOutcome::SizeUnreadable(error) => {
            format!("role={role} <AXSize unreadable: {error}>")
        }
        CandidateOutcome::NonPositiveSize { ax_w, ax_h } => {
            format!("role={role} ax_w={ax_w} ax_h={ax_h} <non-positive size>")
        }
        CandidateOutcome::InvalidScale { ax_w, ax_h, scale } => {
            format!("role={role} ax_w={ax_w} ax_h={ax_h} scale={scale} <invalid scale>")
        }
        CandidateOutcome::PositionUnreadable {
            ax_w,
            ax_h,
            scale,
            error,
        } => {
            format!(
                "role={role} ax_w={ax_w} ax_h={ax_h} scale={scale} <AXPosition unreadable: {error}>"
            )
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
            format!("role={role} ax=({ax_x}, {ax_y}, {ax_w}, {ax_h}) scale={scale} dx={dx} dy={dy}")
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{CandidateOutcome, candidate_line};

    /// The line that cost an hour on 2026-07-29: a locked screen hands back the *application*
    /// element where a window belongs, and `AXSize` fails `kAXErrorAttributeUnsupported`
    /// (-25205). Both facts have to survive into the diagnostic, or the log reads as a glass
    /// regression. The error text here is the raw string `select_window` builds from
    /// `ffi::ax_err`; in production it arrives via `GlassError::Backend`'s `Display`, so the
    /// live line carries an extra `"backend error: "` prefix
    /// (`role=AXApplication <AXSize unreadable: backend error: AXSize: AX call failed
    /// (AXError -25205)>`) — this test only pins the part `candidate_line` itself controls.
    #[test]
    fn an_unreadable_size_keeps_the_role_and_the_ax_error() {
        let line = candidate_line(
            Some("AXApplication"),
            &CandidateOutcome::SizeUnreadable("AXSize: AX call failed (AXError -25205)".into()),
        );
        assert_eq!(
            line,
            "role=AXApplication <AXSize unreadable: AXSize: AX call failed (AXError -25205)>"
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

    /// The `AXPosition` branch drops its error today exactly like the `AXSize` one; it gets the
    /// same treatment, or the fix only half-lands.
    #[test]
    fn an_unreadable_position_keeps_the_ax_error() {
        let line = candidate_line(
            Some("AXWindow"),
            &CandidateOutcome::PositionUnreadable {
                ax_w: 230.0,
                ax_h: 408.0,
                scale: 2.0,
                error: "AXPosition: AX call failed (AXError -25204)".into(),
            },
        );
        assert_eq!(
            line,
            "role=AXWindow ax_w=230 ax_h=408 scale=2 <AXPosition unreadable: AXPosition: AX call \
             failed (AXError -25204)>"
        );
    }
}
