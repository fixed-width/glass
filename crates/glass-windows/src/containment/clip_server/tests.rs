use super::*;
use std::fs::{File, OpenOptions};
use std::io::{Read, Write};
use std::sync::atomic::AtomicUsize;
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_PIPE_NOT_CONNECTED};
use windows::Win32::System::Pipes::WaitNamedPipeW;

#[test]
fn shutdown_during_pipe_creation_does_not_wait_for_another_client() {
    let name = format!("glass-shutdown-race-{}", std::process::id());
    let path = format!(r"\\.\pipe\{name}");
    let stop = Arc::new(AtomicBool::new(false));
    let worker_stop = stop.clone();
    let worker_path = path.clone();
    let (entered_tx, entered_rx) = std::sync::mpsc::channel();
    let (release_tx, release_rx) = std::sync::mpsc::channel();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    let thread = std::thread::spawn(move || {
        accept_loop(
            &worker_path,
            PrivateClipboard::new(),
            worker_stop,
            "unused".into(),
            name,
            || {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
            },
        );
        done_tx.send(()).unwrap();
    });
    entered_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    stop.store(true, Ordering::Relaxed);
    release_tx.send(()).unwrap();
    let finished = done_rx.recv_timeout(Duration::from_secs(1));
    if finished.is_err() {
        drop(
            OpenOptions::new()
                .read(true)
                .write(true)
                .open(&path)
                .unwrap(),
        );
        done_rx.recv_timeout(Duration::from_secs(5)).unwrap();
    }
    thread.join().unwrap();
    assert!(
        finished.is_ok(),
        "shutdown needed an extra client after signaling stop"
    );
    assert_pipe_closed(&path);
}

fn start_server() -> ClipServer {
    static NEXT_PIPE: AtomicUsize = AtomicUsize::new(0);
    let name = format!(
        "glass-server-test-{}-{}",
        std::process::id(),
        NEXT_PIPE.fetch_add(1, Ordering::Relaxed),
    );
    let server = ClipServer::start(
        &name,
        PrivateClipboard::new(),
        std::env::temp_dir()
            .join(&name)
            .to_string_lossy()
            .into_owned(),
        name.clone(),
    )
    .unwrap();
    let wide = to_wide(&server.pipe_path);
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        // SAFETY: the pipe name stays live through the bounded readiness query.
        if unsafe { WaitNamedPipeW(PCWSTR(wide.as_ptr()), 50) }.as_bool() {
            return server;
        }
        assert!(Instant::now() < deadline, "server did not create its pipe");
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn connect(server: &ClipServer) -> File {
    let deadline = Instant::now() + Duration::from_secs(5);
    loop {
        if let Ok(file) = OpenOptions::new()
            .read(true)
            .write(true)
            .open(&server.pipe_path)
        {
            return file;
        }
        assert!(
            Instant::now() < deadline,
            "server did not accept another client"
        );
        std::thread::sleep(Duration::from_millis(10));
    }
}

fn assert_pipe_closed(path: &str) {
    let error = OpenOptions::new()
        .read(true)
        .write(true)
        .open(path)
        .expect_err("server retained its pipe after shutdown");
    assert_eq!(
        error.raw_os_error(),
        Some(ERROR_FILE_NOT_FOUND.0 as i32),
        "pipe must be removed, rather than busy or inaccessible",
    );
}

#[test]
fn stop_releases_pipe_while_waiting_for_connection() {
    let server = start_server();
    let path = server.pipe_path.clone();

    server.stop();

    assert_pipe_closed(&path);
}

#[test]
fn drop_releases_pipe_after_incomplete_and_unauthorized_requests() {
    let server = start_server();
    let path = server.pipe_path.clone();
    let mut client = connect(&server);
    client.write_all(&[1]).unwrap();
    drop(client);

    let mut client = connect(&server);
    client
        .write_all(&proto::frame(&Request::Seq.encode()))
        .unwrap();
    match client.read(&mut [0]) {
        Ok(0) => {}
        Err(error) => assert_eq!(
            error.raw_os_error(),
            Some(ERROR_PIPE_NOT_CONNECTED.0 as i32),
        ),
        result => panic!("unboxed client was served: {result:?}"),
    }
    drop(client);

    drop(server);

    assert_pipe_closed(&path);
}
