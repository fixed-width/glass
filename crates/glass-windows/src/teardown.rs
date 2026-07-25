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
/// - `visible`: a window the user could see. Invisible ones are helper/message-only windows
///   (Chromium keeps several); a `WM_CLOSE` there is at best ignored and at worst closes a
///   worker the app expected to outlive the request.
/// - `owned`: has an owner window — i.e. it is a dialog or palette *belonging* to another
///   window. Skipped because closing the owner is the app's own business: dismissing its
///   dialogs from outside can cancel work the owner is mid-way through, and a well-behaved
///   app tears them down itself once the owner closes.
/// - `toolwindow`: `WS_EX_TOOLWINDOW`, a floating palette rather than a real app window.
///
/// Deliberately *not* filtered on `cloaked`: a window on another virtual desktop is cloaked but
/// entirely real, and skipping it would leave that part of the app to be terminated instead.
/// (This is why the discovery ladder's [`crate::util::WinInfo::looks_like_app_window`] is not
/// reused here — it excludes cloaked windows, which is right for "which window do we drive?"
/// and wrong for "which windows must be asked to close?")
pub fn wants_close_request(visible: bool, owned: bool, toolwindow: bool) -> bool {
    visible && !owned && !toolwindow
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
/// - `job_reports_live`: the Job's pid-set still lists at least one live process. The root
///   exiting is not enough on its own: a launcher that hands off (Chromium/Electron) leaves
///   the real UI running in Job-captured children, and proceeding on root-exit alone would
///   terminate exactly the processes whose clean shutdown we are waiting for. An unreadable
///   pid-set reports `false` — the wait then rests on `root_exited` alone, which is no worse
///   than the unconditional terminate this replaces.
/// - `past_deadline`: the grace has elapsed. An app may veto its own close (an unsaved-changes
///   sheet) or hang; the Job close is the backstop, so teardown always completes.
pub fn close_wait_step(
    posted: usize,
    root_exited: bool,
    job_reports_live: bool,
    past_deadline: bool,
) -> CloseStep {
    if posted == 0 || past_deadline || (root_exited && !job_reports_live) {
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
        assert!(wants_close_request(true, false, false));
    }

    #[test]
    fn an_invisible_window_is_not_asked_to_close() {
        assert!(!wants_close_request(false, false, false));
    }

    #[test]
    fn an_owned_window_is_not_asked_to_close() {
        assert!(!wants_close_request(true, true, false));
    }

    #[test]
    fn a_toolwindow_is_not_asked_to_close() {
        assert!(!wants_close_request(true, false, true));
    }

    #[test]
    fn nothing_posted_skips_the_wait_entirely() {
        // A windowless app must not pay the grace: with no window asked to close, waiting
        // could only ever burn the full budget before terminating anyway.
        assert_eq!(
            close_wait_step(0, false, true, false),
            CloseStep::Proceed,
            "no window was asked to close, so there is nothing to wait for"
        );
    }

    #[test]
    fn a_live_tree_within_the_grace_keeps_waiting() {
        assert_eq!(
            close_wait_step(1, false, true, false),
            CloseStep::KeepWaiting
        );
    }

    #[test]
    fn an_exited_root_with_live_job_children_keeps_waiting() {
        // The launcher-handoff shape: proceeding here would TerminateProcess the very
        // children whose clean shutdown the WM_CLOSE asked for.
        assert_eq!(
            close_wait_step(1, true, true, false),
            CloseStep::KeepWaiting,
            "handed-off children are still closing"
        );
    }

    #[test]
    fn an_exited_root_with_an_empty_job_proceeds() {
        assert_eq!(close_wait_step(1, true, false, false), CloseStep::Proceed);
    }

    #[test]
    fn a_still_live_tree_past_the_grace_proceeds() {
        // An app that vetoes its own close (unsaved-changes sheet) must not wedge teardown.
        assert_eq!(
            close_wait_step(1, false, true, true),
            CloseStep::Proceed,
            "the Job close is the backstop once the grace is spent"
        );
    }

    #[test]
    fn an_unreadable_job_pid_set_waits_only_for_the_root() {
        // `job_reports_live: false` is also what an unreadable pid-set reports, so the
        // decision must not require a positive "job is empty" signal to ever proceed.
        assert_eq!(
            close_wait_step(1, false, false, false),
            CloseStep::KeepWaiting
        );
        assert_eq!(close_wait_step(1, true, false, false), CloseStep::Proceed);
    }
}
