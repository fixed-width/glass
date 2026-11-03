use glass_core::{GlassError, Result};
use serde::{Deserialize, Serialize};
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::path::PathBuf;
use windows::Win32::Foundation::{FILETIME, HANDLE, HLOCAL, LocalFree, WAIT_TIMEOUT};
use windows::Win32::Security::Authorization::ConvertSidToStringSidW;
use windows::Win32::Security::{
    GetTokenInformation, TOKEN_QUERY, TOKEN_STATISTICS, TOKEN_USER, TokenStatistics, TokenUser,
};
use windows::Win32::System::Com::{
    CLSCTX_INPROC_SERVER, COINIT_APARTMENTTHREADED, CoCreateInstance, CoInitializeEx,
    CoUninitialize,
};
use windows::Win32::System::RemoteDesktop::{ProcessIdToSessionId, WTSIsChildSessionsEnabled};
use windows::Win32::System::SystemInformation::GetTickCount64;
use windows::Win32::System::Threading::{
    EVENT_MODIFY_STATE, GetProcessTimes, OpenEventW, OpenProcess, OpenProcessToken,
    PROCESS_NAME_WIN32, PROCESS_QUERY_LIMITED_INFORMATION, PROCESS_SYNCHRONIZE,
    QueryFullProcessImageNameW, SetEvent, WaitForSingleObject,
};
use windows::core::{GUID, IUnknown, PCWSTR, PWSTR};

fn failure(operation: &str, error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("Windows desktop {operation}: {error}"))
}

/// Retain the worker's MTA until all WinRT factories and capture operations have finished.
/// The marker keeps initialization and uninitialization on the same thread.
pub struct RuntimeApartment(std::marker::PhantomData<std::rc::Rc<()>>);

impl RuntimeApartment {
    pub fn initialize() -> Result<Self> {
        use windows::Win32::System::WinRT::{RO_INIT_MULTITHREADED, RoInitialize};
        // SAFETY: the worker owns this thread and balances the successful call on that thread.
        unsafe { RoInitialize(RO_INIT_MULTITHREADED) }
            .map_err(|error| failure("WinRT worker apartment", error))?;
        Ok(Self(std::marker::PhantomData))
    }
}

impl Drop for RuntimeApartment {
    fn drop(&mut self) {
        // SAFETY: the non-Send guard drops on its initializing thread, after its users.
        unsafe {
            windows::Win32::System::WinRT::RoUninitialize();
        }
    }
}

/// Keep a spawned helper from retaining an MCP pipe after its server exits.
pub fn protect_stdio_inheritance() -> Result<()> {
    use windows::Win32::Foundation::{HANDLE_FLAG_INHERIT, SetHandleInformation};
    use windows::Win32::Storage::FileSystem::{FILE_TYPE_PIPE, GetFileType};
    use windows::Win32::System::Console::{
        GetStdHandle, STD_ERROR_HANDLE, STD_INPUT_HANDLE, STD_OUTPUT_HANDLE,
    };
    for stream in [STD_INPUT_HANDLE, STD_OUTPUT_HANDLE, STD_ERROR_HANDLE] {
        // SAFETY: this queries the current process's standard handles without closing or replacing them.
        let handle = match unsafe { GetStdHandle(stream) } {
            Ok(handle) if !handle.is_invalid() => handle,
            _ => continue,
        };
        // SAFETY: only inheritance is removed; read/write access and all existing users remain intact.
        if unsafe { GetFileType(handle) } == FILE_TYPE_PIPE {
            unsafe { SetHandleInformation(handle, HANDLE_FLAG_INHERIT.0, Default::default()) }
                .map_err(|error| failure("stdio inheritance", error))?;
        }
    }
    Ok(())
}

/// A process generation, its Windows logon, and the executable it actually runs.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessIdentity {
    pub pid: u32,
    pub created: u64,
    pub session: u32,
    pub sid: String,
    pub logon: u64,
    pub image: PathBuf,
}

impl ProcessIdentity {
    /// End only this exact generation after its caller has established ownership and cleanup authority.
    pub fn terminate(&self) -> Result<()> {
        use windows::Win32::System::Threading::{PROCESS_TERMINATE, TerminateProcess};
        // SAFETY: a retained query/terminate handle prevents acting through an unbound PID.
        let handle = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE | PROCESS_TERMINATE,
                false,
                self.pid,
            )
        }
        .map_err(|error| failure("open owned process for termination", error))?;
        let process = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        if !self.is_current()? {
            return Err(failure(
                "owned process termination",
                "process identity changed",
            ));
        }
        // SAFETY: the retained handle names the generation just verified; this call never resolves a PID again.
        unsafe { TerminateProcess(HANDLE(process.as_raw_handle()), 1) }
            .map_err(|error| failure("terminate owned process", error))
    }
    pub fn current() -> Result<Self> {
        Self::read(std::process::id())
    }

    pub fn read(pid: u32) -> Result<Self> {
        // SAFETY: query/synchronize access only; each returned handle has one owner.
        let process = unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                pid,
            )
            .map_err(|error| failure("open process", error))?
        };
        let process = unsafe { OwnedHandle::from_raw_handle(process.0) };
        let handle = HANDLE(process.as_raw_handle());
        // SAFETY: the retained handle is valid; the zero timeout never blocks.
        if unsafe { WaitForSingleObject(handle, 0) } != WAIT_TIMEOUT {
            return Err(failure("identity", "process has exited"));
        }
        let (mut created, mut exited, mut kernel, mut user) = (
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
            FILETIME::default(),
        );
        let mut session = 0;
        let mut name = vec![0u16; 32768];
        let mut length = name.len() as u32;
        let mut token = HANDLE::default();
        // SAFETY: all output buffers are correctly sized and live for these calls.
        unsafe {
            GetProcessTimes(handle, &mut created, &mut exited, &mut kernel, &mut user)
                .map_err(|error| failure("process creation time", error))?;
            ProcessIdToSessionId(pid, &mut session)
                .map_err(|error| failure("process session", error))?;
            QueryFullProcessImageNameW(
                handle,
                PROCESS_NAME_WIN32,
                PWSTR(name.as_mut_ptr()),
                &mut length,
            )
            .map_err(|error| failure("process executable", error))?;
            OpenProcessToken(handle, TOKEN_QUERY, &mut token)
                .map_err(|error| failure("process token", error))?;
        }
        // SAFETY: OpenProcessToken succeeded and transferred this handle.
        let token_handle = unsafe { OwnedHandle::from_raw_handle(token.0) };
        let token = HANDLE(token_handle.as_raw_handle());
        let mut needed = 0;
        // SAFETY: a zero-sized query obtains the required buffer size without dereferencing it.
        let _ = unsafe { GetTokenInformation(token, TokenUser, None, 0, &mut needed) };
        if needed == 0 || needed > 1024 * 1024 {
            return Err(failure("token user", "invalid token buffer size"));
        }
        let mut buffer = vec![0u8; needed as usize];
        let mut statistics = TOKEN_STATISTICS::default();
        let mut returned = 0;
        let mut sid_text = PWSTR::null();
        // SAFETY: the token owns the SID buffer until conversion completes. The variable-sized
        // TOKEN_USER buffer is read unaligned; TOKEN_STATISTICS has its native stack alignment.
        unsafe {
            GetTokenInformation(
                token,
                TokenUser,
                Some(buffer.as_mut_ptr().cast()),
                needed,
                &mut returned,
            )
            .map_err(|error| failure("token user", error))?;
            if returned < std::mem::size_of::<TOKEN_USER>() as u32 {
                return Err(failure("token user", "short token response"));
            }
            let user = std::ptr::read_unaligned(buffer.as_ptr().cast::<TOKEN_USER>());
            ConvertSidToStringSidW(user.User.Sid, &mut sid_text)
                .map_err(|error| failure("token SID", error))?;
        }
        // SAFETY: conversion returned a NUL-terminated LocalAlloc string, freed exactly once.
        let sid = unsafe {
            let answer = sid_text
                .to_string()
                .map_err(|error| failure("token SID text", error));
            let _ = LocalFree(Some(HLOCAL(sid_text.0.cast())));
            answer?
        };
        // SAFETY: statistics points to a correctly sized native structure.
        unsafe {
            GetTokenInformation(
                token,
                TokenStatistics,
                Some((&mut statistics as *mut TOKEN_STATISTICS).cast()),
                std::mem::size_of::<TOKEN_STATISTICS>() as u32,
                &mut returned,
            )
            .map_err(|error| failure("token logon", error))?;
        }
        Ok(Self {
            pid,
            created: (u64::from(created.dwHighDateTime) << 32) | u64::from(created.dwLowDateTime),
            session,
            sid,
            logon: ((statistics.AuthenticationId.HighPart as u32 as u64) << 32)
                | u64::from(statistics.AuthenticationId.LowPart),
            image: PathBuf::from(
                String::from_utf16(&name[..length as usize])
                    .map_err(|error| failure("executable path", error))?,
            ),
        })
    }

    pub fn is_current(&self) -> Result<bool> {
        // SAFETY: query-only access retains the generation while liveness is checked.
        let handle = match unsafe {
            OpenProcess(
                PROCESS_QUERY_LIMITED_INFORMATION | PROCESS_SYNCHRONIZE,
                false,
                self.pid,
            )
        } {
            Ok(handle) => handle,
            Err(error) if matches!(error.code(), code if code == windows::core::HRESULT::from_win32(87) || code == windows::core::HRESULT::from_win32(1168)) =>
            {
                return Ok(false);
            }
            Err(error) => return Err(failure("process liveness", error)),
        };
        // SAFETY: successful open transfers one owned handle.
        let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
        // SAFETY: the handle is live and the wait does not block.
        if unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 0) } != WAIT_TIMEOUT {
            return Ok(false);
        }
        match Self::read(self.pid) {
            Ok(current) => Ok(current == *self),
            // SAFETY: a process that exited during the identity query is positively signaled.
            Err(_)
                if unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 0) }
                    != WAIT_TIMEOUT =>
            {
                Ok(false)
            }
            Err(error) => Err(error),
        }
    }
}

/// Configuration only: `false` means disabled, rather than unsupported.
pub fn child_sessions_enabled() -> Result<bool> {
    let mut enabled = windows::core::BOOL(0);
    // SAFETY: this is a read-only query with a valid output pointer.
    unsafe { WTSIsChildSessionsEnabled(&mut enabled) }
        .ok()
        .map_err(|error| failure("child-session configuration", error))?;
    Ok(enabled.as_bool())
}

/// Probe the installed control without creating a desktop, connecting, or changing settings.
pub fn rdp_control_available() -> Result<()> {
    std::thread::Builder::new()
        .name("glass-rdp-probe".into())
        .spawn(|| {
            // SAFETY: this fresh thread owns its COM apartment and releases the control before it ends.
            unsafe {
                CoInitializeEx(None, COINIT_APARTMENTTHREADED)
                    .ok()
                    .map_err(|error| failure("COM apartment", error))?;
                let answer = CoCreateInstance::<_, IUnknown>(
                    &GUID::from_u128(0xa0c63c30_f08d_4ab4_907c_34905d770c7d),
                    None,
                    CLSCTX_INPROC_SERVER,
                )
                .map(drop)
                .map_err(|error| failure("RDP control", error));
                CoUninitialize();
                answer
            }
        })
        .map_err(|error| failure("RDP control probe", error))?
        .join()
        .map_err(|_| failure("RDP control", "probe thread panicked"))?
}

/// Machine-wide monotonic time for deadlines shared by parent and child processes.
pub fn monotonic_ns() -> Result<u64> {
    // SAFETY: GetTickCount64 has no arguments or preconditions.
    unsafe { GetTickCount64() }
        .checked_mul(1_000_000)
        .ok_or_else(|| failure("monotonic clock", "overflow"))
}

/// Signal an existing, owner-created stop event; never create an event during status/stop.
pub fn signal_stop(name: &str) -> Result<()> {
    if !name.starts_with("Global\\Glass.Desktop.Stop.") {
        return Err(failure("stop event", "invalid event name"));
    }
    signal_control(name)
}

pub fn signal_control(name: &str) -> Result<()> {
    let suffix = [
        "Global\\Glass.Desktop.Stop.",
        "Local\\Glass.Desktop.Show.",
        "Local\\Glass.Desktop.Hide.",
    ]
    .into_iter()
    .find_map(|prefix| name.strip_prefix(prefix));
    if !suffix.is_some_and(|suffix| {
        suffix.len() == 32 && suffix.bytes().all(|byte| byte.is_ascii_hexdigit())
    }) {
        return Err(failure("control event", "invalid event name"));
    }
    let name: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the existing event is opened with signal rights only, then owned and closed here.
    let handle = unsafe { OpenEventW(EVENT_MODIFY_STATE, false, PCWSTR(name.as_ptr())) }
        .map_err(|error| failure("open stop event", error))?;
    let event = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    // SAFETY: event remains live for the call.
    unsafe { SetEvent(HANDLE(event.as_raw_handle())) }
        .map_err(|error| failure("signal stop", error))
}

pub fn stop_requested(instance: &str) -> Result<bool> {
    if instance.len() != 32 || !instance.bytes().all(|byte| byte.is_ascii_hexdigit()) {
        return Err(failure("stop event", "invalid instance"));
    }
    let name: Vec<u16> = format!("Global\\Glass.Desktop.Stop.{instance}")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    // SAFETY: open an existing event with wait-only rights; the zero wait does not consume a manual-reset event.
    let handle = unsafe {
        OpenEventW(
            windows::Win32::System::Threading::SYNCHRONIZATION_SYNCHRONIZE,
            false,
            PCWSTR(name.as_ptr()),
        )
    }
    .map_err(|error| failure("open stop witness", error))?;
    let handle = unsafe { OwnedHandle::from_raw_handle(handle.0) };
    Ok(
        unsafe { WaitForSingleObject(HANDLE(handle.as_raw_handle()), 0) }
            == windows::Win32::Foundation::WAIT_OBJECT_0,
    )
}
