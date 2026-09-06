use super::*;
use std::fs::File;
use std::io::{Read, Write};
use std::sync::atomic::{AtomicUsize, Ordering};
use windows::Win32::Foundation::{ERROR_BROKEN_PIPE, ERROR_PIPE_CONNECTED, INVALID_HANDLE_VALUE};
use windows::Win32::Storage::FileSystem::{FlushFileBuffers, PIPE_ACCESS_DUPLEX};
use windows::Win32::System::Pipes::{
    ConnectNamedPipe, CreateNamedPipeW, PIPE_READMODE_BYTE, PIPE_TYPE_BYTE, PIPE_WAIT,
    PeekNamedPipe,
};

fn connected_pipe() -> (OwnedHandle, File) {
    static NEXT_PIPE: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        r"\\.\pipe\glass-client-test-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed),
    );
    let wide: Vec<u16> = name.encode_utf16().chain(Some(0)).collect();
    // SAFETY: the name stays live through the call; all remaining arguments are scalar or absent.
    let server = unsafe {
        CreateNamedPipeW(
            PCWSTR(wide.as_ptr()),
            PIPE_ACCESS_DUPLEX,
            PIPE_TYPE_BYTE | PIPE_READMODE_BYTE | PIPE_WAIT,
            1,
            4096,
            4096,
            0,
            None,
        )
    };
    assert_ne!(server, INVALID_HANDLE_VALUE);
    // SAFETY: CreateNamedPipeW returned a new valid handle owned only by this fixture.
    let server = unsafe { OwnedHandle::from_raw_handle(server.0) };
    let client = open_pipe(&name).expect("connect fixture client");
    // SAFETY: the server is owned and the client connected before this call.
    let connected = unsafe { ConnectNamedPipe(HANDLE(server.as_raw_handle()), None) };
    assert!(
        connected.is_ok()
            || connected.unwrap_err().code()
                == windows::core::HRESULT::from_win32(ERROR_PIPE_CONNECTED.0)
    );
    (client, File::from(server))
}

fn response_round_trip(response: Vec<u8>) -> (Option<Response>, File) {
    let (client, mut server) = connected_pipe();
    let worker = std::thread::spawn(move || {
        let mut request = vec![0; proto::frame(&Request::Seq.encode()).len()];
        server.read_exact(&mut request).unwrap();
        assert_eq!(request, proto::frame(&Request::Seq.encode()));
        server.write_all(&response).unwrap();
        // SAFETY: the owned server is connected; flush keeps the response alive until the client reads it.
        let _ = unsafe { FlushFileBuffers(HANDLE(server.as_raw_handle())) };
        server
    });
    let response = exchange(client, Request::Seq);
    (response, worker.join().unwrap())
}

fn assert_client_closed(server: &File) {
    // SAFETY: the server remains owned; a zero-buffer peek only queries connection state.
    let error = unsafe { PeekNamedPipe(HANDLE(server.as_raw_handle()), None, 0, None, None, None) }
        .expect_err("the exchange must close its client handle");
    assert_eq!(
        error.code(),
        windows::core::HRESULT::from_win32(ERROR_BROKEN_PIPE.0)
    );
}

#[test]
fn exchange_closes_pipe_after_successful_response() {
    let (response, server) = response_round_trip(proto::frame(&Response::Seq(42).encode()));

    assert_eq!(response, Some(Response::Seq(42)));
    assert_client_closed(&server);
}

#[test]
fn exchange_closes_pipe_after_invalid_response() {
    let (response, server) = response_round_trip(proto::frame(&[255]));

    assert_eq!(response, None);
    assert_client_closed(&server);
}

#[test]
fn exchange_closes_pipe_after_oversized_response() {
    let size = u32::try_from(proto::MAX_TOTAL_BYTES + 4097).unwrap();
    let (response, server) = response_round_trip(size.to_le_bytes().to_vec());

    assert_eq!(response, None);
    assert_client_closed(&server);
}
