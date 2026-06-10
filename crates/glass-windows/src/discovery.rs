//! Pure decision logic for the window-discovery poll loop, factored out of the
//! (Windows-only) `backend` so it compiles and is unit-tested on any host — like
//! [`crate::dpi`] and friends. The backend owns the Win32 side of each poll
//! (enumerate windows, `try_wait` the root, read DWM frame bounds) and consults
//! [`poll_decision`] once per iteration to decide whether to wait or give up.

/// What the discovery loop should do after a poll that found no adoptable window.
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
/// - `past_deadline`: whether the timeout has elapsed.
///
/// The subtlety this encodes: a launcher can exit `0` the instant it hands its UI
/// to an *unrelated* process — some packaged Windows apps activate through a system
/// broker, so the real window is owned by neither the launcher nor a descendant the
/// Job/Toolhelp set can follow. Failing the moment the root exits reports
/// `AppExited` a beat before that handoff window maps. So when a hint is present —
/// the caller's explicit signal that a specific window is expected, possibly from
/// another process — keep polling for it until the deadline, then report the exit
/// (more actionable than a bare `Timeout`). With no hint there is nothing an
/// unrelated process could satisfy, so fail fast on root exit rather than burning
/// the whole timeout on what is almost certainly a crash.
///
/// [`WindowHint`]: glass_core::platform::WindowHint
pub fn poll_decision(root_exit: Option<Option<i32>>, has_hint: bool, past_deadline: bool) -> PollStep {
    match root_exit {
        // Root exited: fail now if there's no hint to wait on, or if we've already
        // given a hinted handoff window until the deadline to appear.
        Some(code) if !has_hint || past_deadline => PollStep::FailExited(code),
        // Root exited, a hint is set, and there's still time: a broker/handoff
        // window may yet map — keep polling for it.
        Some(_) => PollStep::KeepPolling,
        // Root still alive but out of time: report the timeout.
        None if past_deadline => PollStep::FailTimeout,
        // Root still alive and time remaining: keep polling.
        None => PollStep::KeepPolling,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn root_alive_keeps_polling_until_deadline() {
        assert_eq!(poll_decision(None, false, false), PollStep::KeepPolling);
        assert_eq!(poll_decision(None, true, false), PollStep::KeepPolling);
    }

    #[test]
    fn root_alive_past_deadline_times_out() {
        assert_eq!(poll_decision(None, false, true), PollStep::FailTimeout);
        assert_eq!(poll_decision(None, true, true), PollStep::FailTimeout);
    }

    #[test]
    fn root_exited_without_hint_fails_fast() {
        // The original fast-fail: a launcher that exits with no hint to wait on has
        // (almost certainly) crashed — don't burn the whole timeout.
        assert_eq!(poll_decision(Some(Some(1)), false, false), PollStep::FailExited(Some(1)));
        assert_eq!(poll_decision(Some(None), false, false), PollStep::FailExited(None));
    }

    #[test]
    fn root_exited_with_hint_keeps_polling_for_handoff() {
        // The grace period: with a hint, a handoff/broker window may still appear,
        // so keep polling rather than reporting AppExited the instant the root dies.
        assert_eq!(poll_decision(Some(Some(0)), true, false), PollStep::KeepPolling);
    }

    #[test]
    fn root_exited_with_hint_reports_exit_at_deadline() {
        // If the hinted window never showed, report the exit (more actionable than a
        // bare Timeout), preserving the exit code.
        assert_eq!(poll_decision(Some(Some(0)), true, true), PollStep::FailExited(Some(0)));
        assert_eq!(poll_decision(Some(Some(3)), true, true), PollStep::FailExited(Some(3)));
    }

    #[test]
    fn root_exited_without_hint_at_deadline_still_reports_exit() {
        assert_eq!(poll_decision(Some(Some(2)), false, true), PollStep::FailExited(Some(2)));
    }
}
