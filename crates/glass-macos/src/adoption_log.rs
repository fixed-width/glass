//! Rendering for the diagnostic `backend::discover_window`/`discover_window_pid` print when
//! they adopt a window.
//!
//! Adoption takes the FIRST on-screen `SCWindow` in `SCShareableContent` order owned by the
//! target pid, and printed nothing about that choice — so a run that adopted a window the
//! accessibility reader could not resolve (#263) left no record of what else was on offer.
//! Separate from `scwindow`/`backend` (both `#[cfg(target_os = "macos")]`) so the format is
//! unit-tested on any host, following `coords`/`clipboard_route`.
#![forbid(unsafe_code)]

use glass_core::platform::WindowGeometry;

/// One on-screen window the adoption scan considered, as plain owned data — never a live
/// `SCWindow`, which cannot cross the `SCShareableContent` completion handler's thread
/// boundary (see `scwindow`'s module doc).
#[derive(Clone, Debug, PartialEq)]
pub struct CandidateWindow {
    /// `SCWindow.windowID()` — the same id `list_windows` hands an agent.
    pub window_id: u32,
    /// `SCWindow.title()`; `None` for a borderless/utility window, which is itself a signal
    /// about what kind of window was adopted.
    pub title: Option<String>,
    /// Pixel geometry, same derivation as `WindowMatch::geometry`.
    pub geometry: WindowGeometry,
    /// Whether this is the candidate adoption took.
    pub adopted: bool,
}

/// The one-line adoption record: how many windows the pid had on screen, in framework order,
/// with the adopted one marked.
pub fn adoption_line(pid: i32, candidates: &[CandidateWindow]) -> String {
    let rendered = if candidates.is_empty() {
        "(none)".to_string()
    } else {
        candidates
            .iter()
            .map(render_candidate)
            .collect::<Vec<_>>()
            .join("; ")
    };
    format!(
        "glass-macos: pid {pid} had {} on-screen window(s) at adoption (first in \
         SCShareableContent order wins): {rendered}",
        candidates.len()
    )
}

fn render_candidate(c: &CandidateWindow) -> String {
    let title = c
        .title
        .as_deref()
        .map_or_else(|| "<untitled>".to_string(), |t| format!("{t:?}"));
    format!(
        "{}id={} {title} {}x{} @({},{})",
        if c.adopted { "ADOPTED " } else { "" },
        c.window_id,
        c.geometry.width,
        c.geometry.height,
        c.geometry.x,
        c.geometry.y
    )
}

#[cfg(test)]
mod tests {
    use super::{CandidateWindow, adoption_line};
    use glass_core::platform::WindowGeometry;

    fn candidate(id: u32, title: Option<&str>, w: u32, h: u32, adopted: bool) -> CandidateWindow {
        CandidateWindow {
            window_id: id,
            title: title.map(str::to_string),
            geometry: WindowGeometry {
                x: 480,
                y: 404,
                width: w,
                height: h,
            },
            adopted,
        }
    }

    /// The shape #263 needed and did not have: which window was taken, out of what, in the
    /// order the framework offered them.
    #[test]
    fn two_candidates_render_in_order_with_the_adopted_one_marked() {
        let line = adoption_line(
            4321,
            &[
                candidate(101, None, 468, 101, true),
                candidate(102, Some("Calculator"), 230, 408, false),
            ],
        );
        assert_eq!(
            line,
            "glass-macos: pid 4321 had 2 on-screen window(s) at adoption (first in \
             SCShareableContent order wins): ADOPTED id=101 <untitled> 468x101 @(480,404); \
             id=102 \"Calculator\" 230x408 @(480,404)"
        );
    }

    #[test]
    fn a_single_candidate_still_renders() {
        let line = adoption_line(7, &[candidate(101, Some("Calculator"), 230, 408, true)]);
        assert_eq!(
            line,
            "glass-macos: pid 7 had 1 on-screen window(s) at adoption (first in \
             SCShareableContent order wins): ADOPTED id=101 \"Calculator\" 230x408 @(480,404)"
        );
    }

    /// A title is rendered with `{:?}` rather than `{}` so an embedded quote can't be confused
    /// with the format's own delimiters or the `"; "` join between candidates.
    #[test]
    fn a_title_with_a_quote_is_escaped() {
        let line = adoption_line(7, &[candidate(101, Some("App \"Beta\""), 230, 408, true)]);
        assert_eq!(
            line,
            "glass-macos: pid 7 had 1 on-screen window(s) at adoption (first in \
             SCShareableContent order wins): ADOPTED id=101 \"App \\\"Beta\\\"\" 230x408 @(480,404)"
        );
    }

    /// Defensive: the caller only logs after a match, so an empty list should be unreachable.
    /// It renders a statement rather than an empty bracket, so if it ever does appear the log
    /// says something true instead of looking truncated.
    #[test]
    fn no_candidates_says_so() {
        let line = adoption_line(7, &[]);
        assert_eq!(
            line,
            "glass-macos: pid 7 had 0 on-screen window(s) at adoption (first in \
             SCShareableContent order wins): (none)"
        );
    }
}
