//! Pure mapping from [`SandboxLevel`] to the set of Job-object limits the Windows
//! backend applies (Job-based in-OS hardening). No Win32 here — the cfg(windows)
//! `build_kill_on_close_job` translates this descriptor into `JOBOBJECT_*` flags — so
//! the policy is unit-tested on the Linux dev box.

// Consumed only by the cfg(windows) job builder + the Linux unit tests, so a non-test
// Linux build sees these as dead.
#![cfg_attr(not(windows), allow(dead_code))]

use glass_core::SandboxLevel;

/// Active-process cap for `sandbox=default`: high enough to clear any realistic app +
/// parallel build, low enough to stop a fork-bomb. A documented, easily-tuned constant.
pub(crate) const DEFAULT_ACTIVE_PROCESS_LIMIT: u32 = 512;

/// Which Job-object limits to apply, independent of the `windows` crate's flag types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) struct JobConfig {
    /// `KILL_ON_JOB_CLOSE` — always set; teardown integrity (close the job → kill the tree).
    pub kill_on_close: bool,
    /// `DIE_ON_UNHANDLED_EXCEPTION` — always set; suppress the crash dialog so a crashed
    /// app dies cleanly instead of hanging the agent loop on a modal box (robustness, not
    /// security, so it applies even at `Off`).
    pub suppress_crash_dialog: bool,
    /// `ACTIVE_PROCESS` cap — `Some` for `Default` (fork-bomb blast-radius), `None` otherwise.
    pub active_process_limit: Option<u32>,
}

/// Map a sandbox level to its Job-limit descriptor.
///
/// `Strict` fails closed in `start_app` *before* any job is built, so its descriptor is
/// never used at runtime; it is mapped (to no cap) only so this function is total.
pub(crate) fn job_config(level: SandboxLevel) -> JobConfig {
    JobConfig {
        kill_on_close: true,
        suppress_crash_dialog: true,
        active_process_limit: match level {
            SandboxLevel::Default => Some(DEFAULT_ACTIVE_PROCESS_LIMIT),
            SandboxLevel::Off | SandboxLevel::Strict => None,
        },
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn off_has_teardown_and_robustness_but_no_cap() {
        let c = job_config(SandboxLevel::Off);
        assert!(c.kill_on_close);
        assert!(c.suppress_crash_dialog);
        assert_eq!(c.active_process_limit, None);
    }

    #[test]
    fn default_adds_the_active_process_cap() {
        let c = job_config(SandboxLevel::Default);
        assert!(c.kill_on_close);
        assert!(c.suppress_crash_dialog);
        assert_eq!(c.active_process_limit, Some(DEFAULT_ACTIVE_PROCESS_LIMIT));
    }

    #[test]
    fn teardown_and_crash_dialog_are_always_on() {
        for level in [
            SandboxLevel::Off,
            SandboxLevel::Default,
            SandboxLevel::Strict,
        ] {
            let c = job_config(level);
            assert!(c.kill_on_close);
            assert!(c.suppress_crash_dialog);
        }
    }
}
