//! Pure decision logic for the window-discovery poll loop, factored out of the
//! (Windows-only) `backend` so it compiles and is unit-tested on any host — like
//! [`crate::dpi`] and friends. The backend owns the Win32 side of each poll
//! (enumerate windows, `try_wait` the root, read DWM frame bounds, query the Job
//! pid-set) and consults [`poll_decision`] once per iteration to decide whether to
//! wait or give up.

/// What the discovery loop should do after one poll found no usable window yet.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum PollStep {
    /// No window yet, but it's still worth waiting — sleep and poll again.
    KeepPolling,
    /// Give up: the launched (root) process exited and nothing took over its UI.
    /// Carries the root's exit code (`None` if it exited without one).
    FailExited(Option<i32>),
    /// Give up: the timeout elapsed while the root process was still alive.
    FailTimeout,
}

/// Decide the next step when a poll iteration found no window to adopt.
///
/// - `root_exit`: `Some(code)` once the launched process has been observed exited,
///   `None` while it is still alive.
/// - `has_hint`: whether the caller supplied a title/class [`WindowHint`].
/// - `has_live_descendants`: whether the app's process set still contains a live
///   process other than the (now-exited) root — i.e. a Job-captured child.
/// - `past_deadline`: whether the timeout has elapsed.
///
/// The subtlety this encodes: a launcher can exit `0` the instant it hands its UI
/// off, before that UI's window maps. There are two legitimate hand-off shapes, and
/// failing on root-exit alone breaks both:
///
/// 1. **To a Job-captured child** — the common multi-process case (Chromium/Edge/
///    Electron/Java): the real browser/UI runs in child processes the Job retains
///    after the root dies, so `job_pids()` still lists them (`has_live_descendants`).
///    Their window appears a beat later. Validated on-box: Edge's root exits ~1s in
///    while 5 live children remain in the Job and a window then maps.
/// 2. **To an *unrelated* process** — some packaged apps activate through a system
///    broker, so the window is owned by neither the launcher nor a descendant the
///    pid-set can follow; only a `has_hint` title/class match can locate it.
///
/// So on root-exit keep polling until the deadline whenever a hint *or* a live
/// descendant could still produce the window, then report the exit (more actionable
/// than a bare `Timeout`). Fast-fail only when neither holds — a launcher that exits
/// leaving no descendant and no hint to wait on is a genuine crash, and we shouldn't
/// burn the whole timeout on it.
///
/// [`WindowHint`]: glass_core::platform::WindowHint
pub fn poll_decision(
    root_exit: Option<Option<i32>>,
    has_hint: bool,
    has_live_descendants: bool,
    past_deadline: bool,
) -> PollStep {
    match root_exit {
        // Root exited. Give up only when nothing more could appear: the deadline passed, OR there
        // is neither a hint to wait on nor a live descendant that might still open a window.
        Some(code) if past_deadline || (!has_hint && !has_live_descendants) => {
            PollStep::FailExited(code)
        }
        // Root exited, but a hint or a live (Job-child) descendant may yet produce the window.
        Some(_) => PollStep::KeepPolling,
        // Root still alive but out of time: report the timeout.
        None if past_deadline => PollStep::FailTimeout,
        // Root still alive and time remaining: keep polling.
        None => PollStep::KeepPolling,
    }
}

/// Whether a candidate window's class is adoptable, given the optional containment class prefix.
///
/// Sandboxie renames a boxed app's top-level window class to `Sandbox:<box>:<orig>`, but leaves
/// glass's own interposed launcher console as `ConsoleWindowClass` (Sandboxie does not rename
/// console windows). So under Sandboxie discovery requires the box prefix — that positively
/// identifies the real app window and skips the launcher console, regardless of process topology
/// (the boxed shell is reparented to `SbieSvc`, so a wrapper-parent walk can't find it). `None`
/// (unconfined) accepts any class, since nothing renames windows there.
pub fn class_adoptable(class: &str, class_prefix: Option<&str>) -> bool {
    class_prefix.is_none_or(|p| class.starts_with(p))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_alive_keeps_polling_until_deadline() {
        assert_eq!(
            poll_decision(None, false, false, false),
            PollStep::KeepPolling
        );
        assert_eq!(
            poll_decision(None, true, false, false),
            PollStep::KeepPolling
        );
    }

    #[test]
    fn root_alive_past_deadline_times_out() {
        assert_eq!(
            poll_decision(None, false, false, true),
            PollStep::FailTimeout
        );
        assert_eq!(poll_decision(None, true, true, true), PollStep::FailTimeout);
    }

    #[test]
    fn root_exited_crash_fast_fails() {
        // No hint AND no live descendant: a launcher that exited leaving nothing behind has
        // (almost certainly) crashed — fail fast rather than burning the whole timeout.
        assert_eq!(
            poll_decision(Some(Some(1)), false, false, false),
            PollStep::FailExited(Some(1))
        );
        assert_eq!(
            poll_decision(Some(None), false, false, false),
            PollStep::FailExited(None)
        );
    }

    #[test]
    fn root_exited_with_live_descendant_keeps_polling() {
        // The fix: a Chromium/Edge/Electron launcher exits 0 but the Job retains live children
        // whose window maps a beat later — keep polling even without a hint.
        assert_eq!(
            poll_decision(Some(Some(0)), false, true, false),
            PollStep::KeepPolling
        );
    }

    #[test]
    fn root_exited_with_hint_keeps_polling_for_handoff() {
        // A broker/unrelated-process hand-off (no descendant in the set) — a title/class hint may
        // still locate the window, so keep polling.
        assert_eq!(
            poll_decision(Some(Some(0)), true, false, false),
            PollStep::KeepPolling
        );
    }

    #[test]
    fn root_exited_reports_exit_at_deadline() {
        // If the hinted/descendant window never showed, report the exit (more actionable than a
        // bare Timeout), preserving the exit code — whichever kept us polling.
        assert_eq!(
            poll_decision(Some(Some(0)), true, false, true),
            PollStep::FailExited(Some(0))
        );
        assert_eq!(
            poll_decision(Some(Some(3)), false, true, true),
            PollStep::FailExited(Some(3))
        );
        assert_eq!(
            poll_decision(Some(Some(2)), false, false, true),
            PollStep::FailExited(Some(2))
        );
    }

    #[test]
    fn class_adoptable_requires_box_prefix_under_sandboxie() {
        let prefix = Some("Sandbox:glass_11128:");
        assert!(class_adoptable("Sandbox:glass_11128:Notepad", prefix));
        // glass's interposed launcher console keeps ConsoleWindowClass — not adoptable.
        assert!(!class_adoptable("ConsoleWindowClass", prefix));
        // a window from a different Sandboxie box is not ours.
        assert!(!class_adoptable("Sandbox:other_box:Foo", prefix));
    }

    #[test]
    fn class_adoptable_accepts_any_class_when_unconfined() {
        assert!(class_adoptable("ConsoleWindowClass", None));
        assert!(class_adoptable("Chrome_WidgetWin_1", None));
    }
}
