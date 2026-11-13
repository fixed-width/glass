use super::ProcessIdentity;
use glass_core::{Deadline, GlassError, Result};
use std::fs::OpenOptions;
use std::os::windows::fs::OpenOptionsExt;
use std::os::windows::io::{AsRawHandle, FromRawHandle, OwnedHandle};
use std::time::Duration;
use windows::Win32::Foundation::{
    ERROR_NO_DATA, ERROR_PIPE_BUSY, ERROR_PIPE_CONNECTED, ERROR_PIPE_LISTENING, HANDLE, HLOCAL,
    INVALID_HANDLE_VALUE, LocalFree,
};
use windows::Win32::Security::Authorization::{
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
use windows::Win32::Storage::FileSystem::{
    FILE_FLAG_FIRST_PIPE_INSTANCE, PIPE_ACCESS_DUPLEX, ReadFile, SECURITY_IDENTIFICATION,
    SECURITY_SQOS_PRESENT, WriteFile,
};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, GetNamedPipeClientProcessId, GetNamedPipeServerProcessId,
    PIPE_NOWAIT, PIPE_READMODE_BYTE, PIPE_REJECT_REMOTE_CLIENTS, PIPE_TYPE_BYTE,
    SetNamedPipeHandleState,
};
use windows::core::{BOOL, HRESULT, PCWSTR};

const MAX_FRAME: usize = 128 * 1024 * 1024;
const WRITE_CHUNK: usize = 16 * 1024;

fn failure(operation: &str, error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("Windows desktop pipe {operation}: {error}"))
}

fn name(value: &str) -> Result<Vec<u16>> {
    if !value.starts_with("glass-desktop-")
        || value.len() > 200
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
    {
        return Err(failure("name", "invalid local endpoint"));
    }
    Ok(format!(r"\\.\pipe\{value}")
        .encode_utf16()
        .chain(Some(0))
        .collect())
}

fn poll(deadline: Deadline) -> Result<()> {
    if deadline.has_passed() {
        return Err(GlassError::deadline_not_started("Windows desktop pipe"));
    }
    std::thread::sleep(
        deadline
            .remaining()
            .unwrap_or(Duration::from_millis(10))
            .min(Duration::from_millis(10)),
    );
    Ok(())
}

/// A local, user-restricted byte stream with bounded, length-prefixed operations.
pub struct LocalPipe {
    handle: OwnedHandle,
    healthy: bool,
}

impl LocalPipe {
    /// Create the only instance, rejecting remote clients and an already claimed endpoint.
    pub fn listen(endpoint: &str) -> Result<Self> {
        let path = name(endpoint)?;
        let current = ProcessIdentity::current()?;
        let descriptor: Vec<u16> = format!("D:P(A;;GA;;;{})", current.sid)
            .encode_utf16()
            .chain(Some(0))
            .collect();
        let mut security = PSECURITY_DESCRIPTOR::default();
        // SAFETY: the SDDL is NUL-terminated, and the allocated descriptor is freed after creation.
        unsafe {
            ConvertStringSecurityDescriptorToSecurityDescriptorW(
                PCWSTR(descriptor.as_ptr()),
                SDDL_REVISION_1,
                &mut security,
                None,
            )
        }
        .map_err(|error| failure("security descriptor", error))?;
        let attributes = SECURITY_ATTRIBUTES {
            nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
            lpSecurityDescriptor: security.0,
            bInheritHandle: BOOL(0),
        };
        // SAFETY: path and attributes stay live; the returned handle is exclusively owned.
        let handle = unsafe {
            CreateNamedPipeW(
                PCWSTR(path.as_ptr()),
                PIPE_ACCESS_DUPLEX | FILE_FLAG_FIRST_PIPE_INSTANCE,
                PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_NOWAIT | PIPE_REJECT_REMOTE_CLIENTS,
                1,
                64 * 1024,
                64 * 1024,
                0,
                Some(&attributes),
            )
        };
        let error = if handle == INVALID_HANDLE_VALUE {
            Some(windows::core::Error::from_thread())
        } else {
            None
        };
        // SAFETY: this is the descriptor allocated by the conversion call above.
        unsafe {
            let _ = LocalFree(Some(HLOCAL(security.0)));
        }
        if let Some(error) = error {
            return Err(failure("create", error));
        }
        // SAFETY: creation succeeded and transfers the handle exactly once.
        Ok(Self {
            handle: unsafe { OwnedHandle::from_raw_handle(handle.0) },
            healthy: true,
        })
    }

    /// Wait for the expected process generation, rather than trusting the endpoint name.
    pub fn accept(&mut self, expected: &ProcessIdentity, deadline: Deadline) -> Result<()> {
        loop {
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started(
                    "Windows desktop pipe accept",
                ));
            }
            // SAFETY: this instance is owned, nonblocking, and has no overlapped operation.
            match unsafe { ConnectNamedPipe(self.raw(), None) } {
                // Nonblocking success means the instance became available, not connected.
                Ok(()) => poll(deadline)?,
                Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_CONNECTED.0) => break,
                Err(error) if error.code() == HRESULT::from_win32(ERROR_PIPE_LISTENING.0) => {
                    poll(deadline)?
                }
                Err(error) => return Err(failure("accept", error)),
            }
        }
        let mut pid = 0;
        // SAFETY: a client is connected and the output pointer is valid.
        unsafe { GetNamedPipeClientProcessId(self.raw(), &mut pid) }
            .map_err(|error| failure("client identity", error))?;
        if ProcessIdentity::read(pid)? != *expected {
            self.healthy = false;
            return Err(failure(
                "client identity",
                "connected process does not match the recorded owner",
            ));
        }
        Ok(())
    }

    /// Connect locally with identification-only impersonation rights.
    pub fn connect(endpoint: &str, deadline: Deadline) -> Result<Self> {
        let path = name(endpoint)?;
        let path =
            String::from_utf16(&path[..path.len() - 1]).map_err(|error| failure("path", error))?;
        loop {
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started(
                    "Windows desktop pipe connect",
                ));
            }
            match OpenOptions::new()
                .read(true)
                .write(true)
                .custom_flags(SECURITY_SQOS_PRESENT.0 | SECURITY_IDENTIFICATION.0)
                .open(&path)
            {
                Ok(file) => {
                    let handle: OwnedHandle = file.into();
                    let pipe = Self {
                        handle,
                        healthy: true,
                    };
                    let mode = PIPE_READMODE_BYTE | PIPE_NOWAIT;
                    // SAFETY: the newly opened byte pipe is owned, and mode is a valid live pointer.
                    unsafe { SetNamedPipeHandleState(pipe.raw(), Some(&mode), None, None) }
                        .map_err(|error| failure("client mode", error))?;
                    return Ok(pipe);
                }
                Err(error)
                    if matches!(error.raw_os_error(), Some(2 | 3))
                        || error.raw_os_error() == Some(ERROR_PIPE_BUSY.0 as i32) =>
                {
                    poll(deadline)?
                }
                Err(error) => return Err(failure("connect", error)),
            }
        }
    }

    pub fn server_identity(&self) -> Result<ProcessIdentity> {
        let mut pid = 0;
        // SAFETY: the connected client handle and output pointer are valid.
        unsafe { GetNamedPipeServerProcessId(self.raw(), &mut pid) }
            .map_err(|error| failure("server identity", error))?;
        ProcessIdentity::read(pid)
    }

    pub fn is_healthy(&self) -> bool {
        self.healthy
    }

    pub fn write_frame(&mut self, bytes: &[u8], deadline: Deadline) -> Result<()> {
        if !self.healthy || bytes.is_empty() || bytes.len() > MAX_FRAME {
            return Err(failure("write", "closed transport or invalid frame size"));
        }
        let mut frame = Vec::with_capacity(4 + bytes.len());
        frame.extend_from_slice(&(bytes.len() as u32).to_le_bytes());
        frame.extend_from_slice(bytes);
        let mut written = 0;
        while written < frame.len() {
            if deadline.has_passed() {
                self.healthy &= written == 0;
                return Err(GlassError::deadline_not_started(
                    "Windows desktop pipe write",
                ));
            }
            let mut count = 0;
            // Oversized nonblocking writes can return zero until the whole buffer fits.
            let end = (written + WRITE_CHUNK).min(frame.len());
            // SAFETY: the remaining bytes are live, and nonblocking mode bounds the OS call.
            if let Err(error) = unsafe {
                WriteFile(
                    self.raw(),
                    Some(&frame[written..end]),
                    Some(&mut count),
                    None,
                )
            } {
                self.healthy = false;
                return Err(failure("write", error));
            }
            written += count as usize;
            if count == 0 {
                let _ = poll(deadline);
            }
        }
        Ok(())
    }

    pub fn read_frame(&mut self, deadline: Deadline) -> Result<Vec<u8>> {
        if !self.healthy {
            return Err(failure(
                "read",
                "transport is closed after an incomplete frame",
            ));
        }
        let mut length = [0u8; 4];
        self.read_exact(&mut length, deadline, false)?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > MAX_FRAME {
            self.healthy = false;
            return Err(failure("read", "invalid frame size"));
        }
        let mut bytes = vec![0u8; length];
        self.read_exact(&mut bytes, deadline, true)?;
        Ok(bytes)
    }

    /// Idle time is separate from transfer time: a request arriving at the idle boundary still
    /// receives a full bounded transfer window, without losing a partial prefix.
    pub fn read_optional_frame(
        &mut self,
        idle: Deadline,
        transfer: Duration,
    ) -> Result<Option<Vec<u8>>> {
        let mut length = [0u8; 4];
        match self.read_exact(&mut length[..1], idle, false) {
            Ok(()) => {}
            Err(_) if self.healthy && idle.has_passed() => return Ok(None),
            Err(error) => return Err(error),
        }
        let deadline = Deadline::at(std::time::Instant::now() + transfer);
        self.read_exact(&mut length[1..], deadline, true)?;
        let length = u32::from_le_bytes(length) as usize;
        if length == 0 || length > MAX_FRAME {
            self.healthy = false;
            return Err(failure("read", "invalid frame size"));
        }
        let mut bytes = vec![0; length];
        self.read_exact(&mut bytes, deadline, true)?;
        Ok(Some(bytes))
    }

    fn read_exact(
        &mut self,
        bytes: &mut [u8],
        deadline: Deadline,
        frame_started: bool,
    ) -> Result<()> {
        let mut read = 0;
        while read < bytes.len() {
            if deadline.has_passed() {
                self.healthy &= !frame_started && read == 0;
                return Err(GlassError::deadline_not_started(
                    "Windows desktop pipe read",
                ));
            }
            let mut count = 0;
            // SAFETY: the remaining output slice is exclusive and the handle is nonblocking.
            match unsafe { ReadFile(self.raw(), Some(&mut bytes[read..]), Some(&mut count), None) }
            {
                Ok(()) if count != 0 => read += count as usize,
                Err(error) if error.code() == HRESULT::from_win32(ERROR_NO_DATA.0) => {
                    let _ = poll(deadline);
                }
                Ok(()) => {
                    self.healthy = false;
                    return Err(failure("read", "peer closed the stream"));
                }
                Err(error) => {
                    self.healthy = false;
                    return Err(failure("read", error));
                }
            }
        }
        Ok(())
    }

    fn raw(&self) -> HANDLE {
        HANDLE(self.handle.as_raw_handle())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(0);

    fn endpoint() -> String {
        format!(
            "glass-desktop-tests-{}-{}",
            std::process::id(),
            NEXT.fetch_add(1, Ordering::Relaxed)
        )
    }

    fn pair() -> (LocalPipe, LocalPipe) {
        let name = endpoint();
        let mut server = LocalPipe::listen(&name).expect("private server");
        let client = std::thread::spawn(move || {
            std::thread::sleep(Duration::from_millis(30));
            LocalPipe::connect(&name, Deadline::from_millis(2_000)).expect("local client")
        });
        server
            .accept(
                &ProcessIdentity::current().expect("identity"),
                Deadline::from_millis(2_000),
            )
            .expect("wait past initial listening state for the actual client");
        (server, client.join().expect("client thread"))
    }

    #[test]
    fn delayed_connection_authenticates_and_transfers_frames_in_both_directions() {
        let (mut server, mut client) = pair();
        assert_eq!(
            client.server_identity().expect("server identity"),
            ProcessIdentity::current().expect("current")
        );
        client
            .write_frame(b"request", Deadline::from_millis(1_000))
            .expect("request");
        assert_eq!(
            server
                .read_frame(Deadline::from_millis(1_000))
                .expect("read request"),
            b"request"
        );
        server
            .write_frame(b"answer", Deadline::from_millis(1_000))
            .expect("response");
        assert_eq!(
            client
                .read_frame(Deadline::from_millis(1_000))
                .expect("read response"),
            b"answer"
        );
    }

    #[test]
    fn a_frame_larger_than_the_pipe_buffer_streams_while_its_peer_reads() {
        let (mut server, mut client) = pair();
        let payload: Vec<u8> = (0..1024 * 1024).map(|index| (index % 251) as u8).collect();
        let expected = payload.clone();
        let writer = std::thread::spawn(move || {
            let result = server.write_frame(&payload, Deadline::from_millis(3000));
            (server, result)
        });
        let received = client.read_frame(Deadline::from_millis(4000));
        let (mut server, result) = writer.join().unwrap();
        result.unwrap();
        assert_eq!(received.unwrap(), expected);
        let reverse = expected.clone();
        let writer = std::thread::spawn(move || {
            let result = client.write_frame(&reverse, Deadline::from_millis(3000));
            (client, result)
        });
        let received = server.read_frame(Deadline::from_millis(4000));
        let (client, result) = writer.join().unwrap();
        result.unwrap();
        assert_eq!(received.unwrap(), expected);
        assert!(server.is_healthy() && client.is_healthy());
    }

    #[test]
    fn a_matching_pid_with_a_different_recorded_birth_is_rejected() {
        let name = endpoint();
        let mut server = LocalPipe::listen(&name).expect("server");
        let client = std::thread::spawn(move || {
            LocalPipe::connect(&name, Deadline::from_millis(2_000)).expect("client")
        });
        let mut expected = ProcessIdentity::current().expect("identity");
        expected.created += 1;
        assert!(
            server
                .accept(&expected, Deadline::from_millis(2_000))
                .is_err()
        );
        assert!(!server.is_healthy());
        drop(client.join().expect("client thread"));
    }

    #[test]
    fn idle_expiry_keeps_the_next_complete_request_usable() {
        let (mut server, mut client) = pair();
        assert_eq!(
            server
                .read_optional_frame(Deadline::from_millis(20), Duration::from_secs(1))
                .expect("idle"),
            None
        );
        assert!(server.is_healthy());
        client
            .write_frame(b"next", Deadline::from_millis(1_000))
            .expect("next request");
        assert_eq!(
            server
                .read_optional_frame(Deadline::from_millis(100), Duration::from_secs(1))
                .expect("request"),
            Some(b"next".to_vec())
        );
    }

    #[test]
    fn an_incomplete_frame_poisons_the_connection_instead_of_reusing_its_tail() {
        let (mut server, client) = pair();
        let mut count = 0;
        // SAFETY: the client owns this nonblocking stream; the four-byte prefix stays live.
        unsafe {
            WriteFile(
                client.raw(),
                Some(&16u32.to_le_bytes()),
                Some(&mut count),
                None,
            )
        }
        .expect("partial frame");
        assert_eq!(count, 4);
        assert!(server.read_frame(Deadline::from_millis(20)).is_err());
        assert!(!server.is_healthy());
        assert!(server.read_frame(Deadline::from_millis(1_000)).is_err());
    }
}
