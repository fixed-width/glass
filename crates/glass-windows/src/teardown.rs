//! Pure decision logic for graceful app teardown, factored out of the (Windows-only)
//! `process` module so it compiles and is unit-tested on any host — like [`crate::discovery`]
//! and friends. The backend owns the Win32 side (enumerate top-level windows, `PostMessageW`
//! a `WM_CLOSE`, `try_wait` the root, query the Job pid-set) and consults these two decisions.
//!
//! Why ask before terminating at all: closing the Job handle is `TerminateProcess` for every
//! process in the tree, which gives an app no chance to run its shutdown path. Apps that
//! persist a "did I exit cleanly?" flag then report a crash on their *next* launch — measured
//! on-box, an Edge tree torn down by `TerminateProcess` records `"exit_type": "Crashed"` in its
//! profile and greets the next run with a "Restore pages?" prompt, while the same tree sent a
//! `WM_CLOSE` records `"Normal"` and starts clean. A driven app whose first screen changes run
//! to run is a worse target for an agent than one that costs a few hundred ms to close politely.

/// Whether a top-level window belonging to the app's process set should be sent a `WM_CLOSE`.
///
/// - `visible`: the `WS_VISIBLE` style bit is set (not "the user can see it" — an occluded,
///   off-screen or cloaked window still has it). Windows without it are helper/message-only
///   ones (Chromium keeps several); a `WM_CLOSE` there is at best ignored and at worst closes a
///   worker the app expected to outlive the request.
/// - `owned`: has an owner window — i.e. it is a dialog or palette *belonging* to another
///   window. Skipped unless `appwindow`, because closing the owner is the app's own business:
///   dismissing its dialogs from outside can cancel work the owner is mid-way through, and a
///   well-behaved app tears them down itself once the owner closes.
/// - `appwindow`: `WS_EX_APPWINDOW`, the app's own declaration that this is a real taskbar-level
///   window despite having an owner. Honoured for the same reason the discovery ladder honours
///   it: glass may well be *driving* such a window, and a window glass drives is one it must ask
///   to close rather than terminate out from under.
/// - `toolwindow`: `WS_EX_TOOLWINDOW`, a floating palette rather than a real app window.
///
/// This is deliberately the discovery ladder's `WinInfo::looks_like_app_window` filter minus its
/// `!cloaked` clause, and that one difference is the whole reason the two are not one function: a
/// window on another virtual desktop is cloaked but entirely real. Excluding it is right for
/// "which window do we drive?" and wrong for "which windows must be asked to close?", where it
/// would leave that part of the app to be terminated instead.
pub fn wants_close_request(visible: bool, owned: bool, appwindow: bool, toolwindow: bool) -> bool {
    visible && (!owned || appwindow) && !toolwindow
}

/// What the graceful-close wait should do after one poll.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum CloseStep {
    /// The tree is still shutting down and there is grace left — sleep and poll again.
    KeepWaiting,
    /// Stop waiting and close the Job (terminating whatever is left).
    Proceed,
}

/// Decide whether to keep waiting for the app to close itself.
///
/// - `posted`: how many windows actually accepted a `WM_CLOSE`. Zero means nothing was asked
///   to close — a windowless (console/service) app, or one whose windows all failed the
///   [`wants_close_request`] filter or the post itself (a higher-integrity app rejects a
///   cross-process post under UIPI). There is nothing to wait *for*, so the grace is skipped
///   entirely rather than spent; teardown stays as fast as it was before.
/// - `root_exited`: the launched root process has been observed exited.
/// - `job_live`: whether the Job's pid-set still lists a live process — `None` when the query
///   failed, which is a different answer from `Some(false)` ("the job is empty, the tree is
///   gone") and must not be collapsed into it. The root exiting is not enough on its own: a
///   launcher that hands off (Chromium/Electron) leaves the real UI running in Job-captured
///   children, and proceeding on root-exit alone would terminate exactly the processes whose
///   clean shutdown we are waiting for. `None` therefore keeps waiting — proceeding is the
///   destructive branch, so an unreadable state must not be read as permission to take it.
///   The cost of being wrong is bounded by `past_deadline`.
/// - `past_deadline`: the grace has elapsed. An app may veto its own close (an unsaved-changes
///   sheet) or hang; the Job close is the backstop, so teardown always completes.
pub fn close_wait_step(
    posted: usize,
    root_exited: bool,
    job_live: Option<bool>,
    past_deadline: bool,
) -> CloseStep {
    if posted == 0 || past_deadline || (root_exited && job_live == Some(false)) {
        CloseStep::Proceed
    } else {
        CloseStep::KeepWaiting
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_plain_visible_top_level_window_is_asked_to_close() {
        assert!(wants_close_request(true, false, false, false));
    }

    #[test]
    fn an_invisible_window_is_not_asked_to_close() {
        assert!(!wants_close_request(false, false, false, false));
    }

    #[test]
    fn an_owned_window_is_not_asked_to_close() {
        assert!(!wants_close_request(true, true, false, false));
    }

    #[test]
    fn an_owned_appwindow_is_asked_to_close() {
        // `WS_EX_APPWINDOW` is the app declaring an owned window to be a real taskbar-level
        // one, and the discovery ladder adopts such a window — so glass can be driving it.
        // Filtering it out here would terminate the very window the agent was working in.
        assert!(wants_close_request(true, true, true, false));
    }

    #[test]
    fn a_toolwindow_is_not_asked_to_close() {
        assert!(!wants_close_request(true, false, false, true));
    }

    #[test]
    fn nothing_posted_skips_the_wait_entirely() {
        // A windowless app must not pay the grace: with no window asked to close, waiting
        // could only ever burn the full budget before terminating anyway.
        assert_eq!(
            close_wait_step(0, false, Some(true), false),
            CloseStep::Proceed,
            "no window was asked to close, so there is nothing to wait for"
        );
    }

    #[test]
    fn a_live_tree_within_the_grace_keeps_waiting() {
        assert_eq!(
            close_wait_step(1, false, Some(true), false),
            CloseStep::KeepWaiting
        );
    }

    #[test]
    fn an_exited_root_with_live_job_children_keeps_waiting() {
        // The launcher-handoff shape: proceeding here would TerminateProcess the very
        // children whose clean shutdown the WM_CLOSE asked for.
        assert_eq!(
            close_wait_step(1, true, Some(true), false),
            CloseStep::KeepWaiting,
            "handed-off children are still closing"
        );
    }

    #[test]
    fn an_exited_root_with_an_empty_job_proceeds() {
        assert_eq!(
            close_wait_step(1, true, Some(false), false),
            CloseStep::Proceed
        );
    }

    #[test]
    fn a_still_live_tree_past_the_grace_proceeds() {
        // An app that vetoes its own close (unsaved-changes sheet) must not wedge teardown.
        assert_eq!(
            close_wait_step(1, false, Some(true), true),
            CloseStep::Proceed,
            "the Job close is the backstop once the grace is spent"
        );
    }

    #[test]
    fn an_unreadable_job_pid_set_is_not_read_as_an_empty_one() {
        // The destructive branch requires a *positive* "the job is empty". An unreadable
        // pid-set (`None`) must keep waiting even once the root has exited: reading it as
        // "gone" is exactly how a launcher-handoff app's children would get terminated
        // milliseconds after being asked to close, with a half-written profile on disk.
        assert_eq!(
            close_wait_step(1, true, None, false),
            CloseStep::KeepWaiting,
            "unknown is not permission to terminate"
        );
        assert_eq!(
            close_wait_step(1, true, Some(false), false),
            CloseStep::Proceed,
            "a positive empty reading does proceed"
        );
    }

    #[test]
    fn an_unreadable_job_pid_set_still_gives_up_at_the_deadline() {
        // The grace bounds the cost of never getting a readable answer, so treating `None`
        // as "keep waiting" can't wedge teardown.
        assert_eq!(close_wait_step(1, true, None, true), CloseStep::Proceed);
    }
}
