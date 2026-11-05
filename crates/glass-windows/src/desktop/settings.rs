use super::{ProcessIdentity, child_sessions_enabled};
use glass_core::{Deadline, GlassError, Result};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::Path;
use std::time::Duration;
use windows::Win32::Foundation::{GetLastError, HANDLE, SetLastError, WAIT_OBJECT_0, WIN32_ERROR};
use windows::Win32::System::RemoteDesktop::{
    WTSEnableChildSessions, WTSEnumerateSessionsW, WTSFreeMemory, WTSGetChildSessionId,
    WTSQuerySessionInformationW, WTSUserName,
};
use windows::Win32::System::Threading::{GetExitCodeProcess, WaitForSingleObject};
use windows::Win32::UI::Shell::{
    SEE_MASK_NOASYNC, SEE_MASK_NOCLOSEPROCESS, SHELLEXECUTEINFOW, ShellExecuteExW,
};
use windows::Win32::UI::WindowsAndMessaging::SW_HIDE;
use windows::core::{PCWSTR, PWSTR};

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SettingChange {
    Changed,
    AlreadySet,
}

pub fn current_child_session() -> Result<Option<u32>> {
    let mut child = u32::MAX;
    // SAFETY: output storage is valid; clearing last error keeps a refusal without diagnostics distinct.
    unsafe {
        SetLastError(WIN32_ERROR(0));
        if WTSGetChildSessionId(&mut child).as_bool() {
            return Ok((child != u32::MAX).then_some(child));
        }
        let error = GetLastError().0;
        if error == 1168 && child == u32::MAX {
            return Ok(None);
        }
        Err(GlassError::Backend(format!(
            "Windows child-session query failed (Win32 {error})"
        )))
    }
}

fn may_disable() -> Result<bool> {
    if current_child_session()?.is_some() {
        return Ok(false);
    }
    let parent = ProcessIdentity::current()?.session;
    if parent == 0 {
        return Ok(false);
    }
    let mut buffer = std::ptr::null_mut();
    let mut count = 0;
    // SAFETY: Windows allocates the session array, and it is freed after the bounded scan.
    unsafe { WTSEnumerateSessionsW(None, 0, 1, &mut buffer, &mut count) }.map_err(|error| {
        GlassError::Backend(format!(
            "enumerate sessions before restoring child setting: {error}"
        ))
    })?;
    let result = (|| {
        if count > 65536 {
            return Err(GlassError::Backend("invalid Windows session count".into()));
        }
        for index in 0..count as usize {
            // SAFETY: the successful enumeration returned count initialized records.
            let session = unsafe { &*buffer.add(index) };
            if session.SessionId == parent || session.SessionId == 0 || session.State.0 >= 5 {
                continue;
            }
            let mut user = PWSTR::null();
            let mut bytes = 0;
            // SAFETY: the user-name allocation is owned by Windows and freed after inspection.
            unsafe {
                WTSQuerySessionInformationW(
                    None,
                    session.SessionId,
                    WTSUserName,
                    &mut user,
                    &mut bytes,
                )
            }
            .map_err(|error| {
                GlassError::Backend(format!(
                    "verify session ownership before child setting restoration: {error}"
                ))
            })?;
            // SAFETY: this is the queried string buffer, read only within the returned byte count.
            let occupied = unsafe { bytes >= 2 && !user.is_null() && *user.0 != 0 };
            unsafe {
                WTSFreeMemory(user.0.cast());
            }
            if occupied {
                return Ok(false);
            }
        }
        Ok(true)
    })();
    // SAFETY: this is the allocation returned by enumeration.
    unsafe {
        WTSFreeMemory(buffer.cast());
    }
    result
}

/// Only the explicit setup command calls this privileged action. Exit codes preserve its outcome
/// across the Windows elevation boundary without passing a path, password, or arbitrary command.
pub fn setting_action_exit_code(enabled: bool) -> u32 {
    let before = match child_sessions_enabled() {
        Ok(value) => value,
        Err(_) => return 23,
    };
    if before == enabled {
        return 10;
    }
    if !enabled && !matches!(may_disable(), Ok(true)) {
        return 20;
    }
    // SAFETY: explicit setup changes only the documented child-session switch.
    let changed = unsafe {
        SetLastError(WIN32_ERROR(0));
        WTSEnableChildSessions(enabled)
    };
    if !changed.as_bool() {
        let error = unsafe { GetLastError().0 };
        return if error == 0 { 21 } else { 0x1000_0000 | error };
    }
    match child_sessions_enabled() {
        Ok(value) if value == enabled => 0,
        _ => 22,
    }
}

pub fn change_setting(
    executable: &Path,
    enabled: bool,
    deadline: Deadline,
) -> Result<SettingChange> {
    change_setting_with(executable, enabled, deadline, |launch| {
        // SAFETY: the caller retains all strings and requests the normal Windows consent UI.
        unsafe { ShellExecuteExW(launch) }
    })
}

fn change_setting_with(
    executable: &Path,
    enabled: bool,
    deadline: Deadline,
    elevate: impl FnOnce(&mut SHELLEXECUTEINFOW) -> windows::core::Result<()>,
) -> Result<SettingChange> {
    let path: Vec<u16> = executable
        .as_os_str()
        .encode_wide()
        .chain(Some(0))
        .collect();
    let action: Vec<u16> = format!(
        "windows-desktop-setting {}",
        if enabled { "enable" } else { "disable" }
    )
    .encode_utf16()
    .chain(Some(0))
    .collect();
    let verb: Vec<u16> = "runas".encode_utf16().chain(Some(0)).collect();
    let mut launch = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOCLOSEPROCESS | SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(path.as_ptr()),
        lpParameters: PCWSTR(action.as_ptr()),
        nShow: SW_HIDE.0,
        ..Default::default()
    };
    elevate(&mut launch).map_err(|error| {
        let action = if enabled { "enable" } else { "restore" };
        GlassError::Backend(format!(
            "Windows administrator approval to {action} child sessions was canceled, denied or unavailable. \
             If this was unexpected, ask your administrator to review elevation permissions. {error}"
        ))
    })?;
    if launch.hProcess.is_invalid() {
        return Err(GlassError::Backend(
            "Windows setup returned no process handle".into(),
        ));
    }
    // SAFETY: SEE_MASK_NOCLOSEPROCESS transfers this handle to the caller.
    let process = unsafe { OwnedHandle::from_raw_handle(launch.hProcess.0) };
    loop {
        if deadline.has_passed() {
            return Err(GlassError::Backend(
                "Windows desktop setup is still pending; inspect desktop status before retrying"
                    .into(),
            ));
        }
        // SAFETY: the retained process handle supports a short bounded wait.
        if unsafe { WaitForSingleObject(HANDLE(process.as_raw_handle()), 50) } == WAIT_OBJECT_0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(10));
    }
    let mut code = 0;
    // SAFETY: the completed process and exit-code pointer are valid.
    unsafe { GetExitCodeProcess(HANDLE(process.as_raw_handle()), &mut code) }
        .map_err(|error| GlassError::Backend(format!("read Windows setup result: {error}")))?;
    setting_result(code, enabled)
}

fn setting_result(code: u32, enabled: bool) -> Result<SettingChange> {
    match code {
        0 => Ok(SettingChange::Changed),
        10 => Ok(SettingChange::AlreadySet),
        20 => Err(GlassError::Backend(
            "another Windows session may use child sessions; their setting was retained".into(),
        )),
        21 => Err(GlassError::Backend(
            "Windows declined the child-session setting change without supplying an error code"
                .into(),
        )),
        22 => Err(GlassError::Backend(
            "Windows child-session setting did not read back correctly".into(),
        )),
        23 => Err(GlassError::Backend(
            "Windows could not query the child-session setting during setup".into(),
        )),
        code if code & 0x1000_0000 != 0 => Err(GlassError::Backend(format!(
            "Windows could not {} child sessions. Ask your administrator to review permissions for this setting. {}",
            if enabled { "enable" } else { "restore" },
            std::io::Error::from_raw_os_error((code & 0x0fff_ffff) as i32)
        ))),
        code => Err(GlassError::Backend(format!(
            "Windows desktop setup exited with code {code}"
        ))),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denied_elevation_preserves_native_error_and_names_setup_action() {
        for enabled in [true, false] {
            let error = change_setting_with(
                Path::new(r"C:\Glass\glass-mcp.exe"),
                enabled,
                Deadline::from_millis(1000),
                |_| {
                    Err(windows::core::Error::from_hresult(
                        windows::core::HRESULT::from_win32(5),
                    ))
                },
            )
            .unwrap_err()
            .to_string();
            assert!(error.contains("0x80070005"), "{error}");
            assert!(
                error.contains(if enabled {
                    "enable child sessions"
                } else {
                    "restore child sessions"
                }),
                "{error}"
            );
            assert!(error.contains("ask your administrator"), "{error}");
        }
    }

    #[test]
    fn canceled_elevation_is_not_reported_as_an_identified_policy() {
        let error = change_setting_with(
            Path::new(r"C:\Glass\glass-mcp.exe"),
            true,
            Deadline::from_millis(1000),
            |_| {
                Err(windows::core::Error::from_hresult(
                    windows::core::HRESULT::from_win32(1223),
                ))
            },
        )
        .unwrap_err()
        .to_string();
        assert!(error.contains("0x800704C7"), "{error}");
        assert!(error.contains("canceled, denied or unavailable"), "{error}");
        assert!(!error.contains("blocked by policy"), "{error}");
    }

    #[test]
    fn denied_setting_helper_retains_native_code_and_restoration_context() {
        let error = setting_result(0x1000_0005, false).unwrap_err().to_string();
        assert!(error.contains("restore child sessions"), "{error}");
        assert!(error.contains("os error 5"), "{error}");
        assert!(error.contains("administrator"), "{error}");
    }
}
