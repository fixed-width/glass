//! Console mode: switch a Windows console into ANSI-interpreting mode.
//!
//! A Windows console does not interpret ANSI escapes until
//! `ENABLE_VIRTUAL_TERMINAL_PROCESSING` is set on its output handle. Windows Terminal sets it for
//! its own sessions; legacy conhost does not, and renders escapes as visible text.

/// Turn on `ENABLE_VIRTUAL_TERMINAL_PROCESSING` for stdout, reporting whether it took. `false`
/// for a redirected handle (which has no console mode to set) or a host that refuses the mode —
/// the caller then suppresses color rather than printing escapes as text.
#[cfg(windows)]
pub fn enable_vt_processing() -> bool {
    use windows::Win32::System::Console::{
        CONSOLE_MODE, ENABLE_VIRTUAL_TERMINAL_PROCESSING, GetConsoleMode, GetStdHandle,
        STD_OUTPUT_HANDLE, SetConsoleMode,
    };
    // SAFETY: `GetStdHandle` returns a process-owned handle that we neither close nor store past
    // this call. `GetConsoleMode` writes through `&mut mode`, which we own and have initialized.
    // `SetConsoleMode` takes that same handle by value. Every call's result is checked, so a
    // redirected handle (no console mode) returns false instead of proceeding on an unread `mode`.
    unsafe {
        let Ok(handle) = GetStdHandle(STD_OUTPUT_HANDLE) else {
            return false;
        };
        let mut mode = CONSOLE_MODE::default();
        if GetConsoleMode(handle, &mut mode).is_err() {
            return false;
        }
        SetConsoleMode(handle, mode | ENABLE_VIRTUAL_TERMINAL_PROCESSING).is_ok()
    }
}

#[cfg(all(windows, test))]
mod tests {
    use super::*;

    #[test]
    fn enabling_twice_reports_the_same_answer() {
        // The strong assertion — "returns true on a console, false when redirected" — can't be
        // written portably: under `cargo test` stdout is a pipe, so this is the redirected case,
        // and under `--nocapture` on a real console it is not. Idempotence holds either way, and
        // still catches a first call that leaves the mode in a state the second one chokes on.
        assert_eq!(enable_vt_processing(), enable_vt_processing());
    }
}
