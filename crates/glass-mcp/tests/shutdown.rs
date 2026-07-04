//! Signal-driven teardown: a graceful SIGTERM must tear down the launched app (and
//! any spawned Xvfb) — no orphan. #[ignore]d (needs Xvfb + the glass-testapp
//! binary). Build both binaries first, then run:
//!   cargo build -p glass-testapp -p glass-mcp
//!   cargo test -p glass-mcp --test shutdown -- --ignored --test-threads=1

mod common;

use std::io::{BufRead, BufReader, Write};
use std::path::PathBuf;
use std::process::{ChildStdin, ChildStdout, Command, Stdio};

use common::Xvfb;

const SERVER: &str = env!("CARGO_BIN_EXE_glass-mcp");

/// glass-testapp lands next to glass-mcp in `target/<profile>/`.
fn testapp_path() -> PathBuf {
    let dir = PathBuf::from(SERVER).parent().unwrap().to_path_buf();
    let p = dir.join("glass-testapp");
    assert!(
        p.exists(),
        "glass-testapp not found at {p:?}; build it first: cargo build -p glass-testapp"
    );
    p
}

fn send(stdin: &mut impl Write, msg: &serde_json::Value) {
    stdin.write_all(msg.to_string().as_bytes()).unwrap();
    stdin.write_all(b"\n").unwrap();
    stdin.flush().unwrap();
}

fn read_response(reader: &mut impl BufRead, id: i64) -> serde_json::Value {
    let mut line = String::new();
    for _ in 0..200 {
        line.clear();
        if reader.read_line(&mut line).unwrap() == 0 {
            panic!("server closed stdout before responding to id {id}");
        }
        if let Ok(v) = serde_json::from_str::<serde_json::Value>(line.trim()) {
            if v.get("id").and_then(|i| i.as_i64()) == Some(id) {
                return v;
            }
        }
    }
    panic!("no response with id {id}");
}

fn pid_alive(pid: u32) -> bool {
    std::path::Path::new(&format!("/proc/{pid}")).exists()
}

/// The pid of the (first) child of `parent` whose `/proc/<pid>/status` Name is
/// `comm`. Linux-only; robust under parallel tests because it filters by ppid.
fn child_pid_of(parent: u32, comm: &str) -> Option<u32> {
    for entry in std::fs::read_dir("/proc").ok()?.flatten() {
        let pid: u32 = match entry.file_name().to_str().and_then(|s| s.parse().ok()) {
            Some(p) => p,
            None => continue,
        };
        let status = match std::fs::read_to_string(format!("/proc/{pid}/status")) {
            Ok(s) => s,
            Err(_) => continue,
        };
        let name_ok = status
            .lines()
            .next()
            .and_then(|l| l.strip_prefix("Name:"))
            .map(|n| n.trim() == comm)
            .unwrap_or(false);
        let ppid_ok = status
            .lines()
            .find_map(|l| l.strip_prefix("PPid:"))
            .and_then(|v| v.trim().parse::<u32>().ok())
            == Some(parent);
        if name_ok && ppid_ok {
            return Some(pid);
        }
    }
    None
}

fn wait_gone(pid: u32, tries: u32) -> bool {
    for _ in 0..tries {
        if !pid_alive(pid) {
            return true;
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }
    !pid_alive(pid)
}

/// Drive the server through initialize + glass_start(testapp). Returns the child,
/// its pid, and the stdin/stdout handles — the CALLER must keep stdin alive until
/// after SIGTERM (closing it would exit the server via stdin-EOF, not the signal
/// path under test). `extra_env` sets backend/display.
fn start_session(
    extra_env: &[(&str, &str)],
) -> (std::process::Child, u32, ChildStdin, BufReader<ChildStdout>) {
    let app = testapp_path();
    let mut cmd = Command::new(SERVER);
    cmd.env("GLASS_BACKEND", "x11");
    // Hermetic: ignore any ambient GLASS_DISPLAY (e.g. a dev's :42 sandbox), so the
    // spawned-Xvfb test really spawns its own. `extra_env` re-adds it for attach mode.
    cmd.env_remove("GLASS_DISPLAY");
    for (k, v) in extra_env {
        cmd.env(k, v);
    }
    let mut child = cmd
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .expect("spawn glass-mcp");
    let server_pid = child.id();
    let mut stdin = child.stdin.take().unwrap();
    let mut stdout = BufReader::new(child.stdout.take().unwrap());

    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 1, "method": "initialize",
            "params": { "protocolVersion": "2024-11-05", "capabilities": {},
                        "clientInfo": { "name": "glass-shutdown-test", "version": "0" } }
        }),
    );
    assert!(
        read_response(&mut stdout, 1).get("result").is_some(),
        "initialize failed"
    );
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/initialized"
        }),
    );
    send(
        &mut stdin,
        &serde_json::json!({
            "jsonrpc": "2.0", "id": 2, "method": "tools/call",
            "params": { "name": "glass_start",
                        "arguments": { "run": [app.to_str().unwrap()], "timeout_ms": 5000 } }
        }),
    );
    let started = read_response(&mut stdout, 2);
    assert_ne!(
        started["result"]["isError"].as_bool(),
        Some(true),
        "glass_start errored: {started}"
    );
    // Return stdin/stdout so the CALLER keeps them alive (held in `_stdin`/`_stdout`).
    // stdin must stay OPEN across SIGTERM and child.wait(): closing it would give the
    // server stdin-EOF and exit it via the normal path instead of the signal path under
    // test. The server force-exits (std::process::exit) after signal teardown, so
    // child.wait() returns promptly even with stdin still open — and if signal handling
    // regressed, SIGTERM would default-terminate the server, orphaning the app and
    // failing the assertions below (i.e. these are real guards for the signal path).
    (child, server_pid, stdin, stdout)
}

fn sigterm(pid: u32) {
    let ok = Command::new("kill")
        .args(["-TERM", &pid.to_string()])
        .status()
        .expect("run kill -TERM");
    assert!(ok.success(), "kill -TERM {pid} failed");
}

/// Attach mode (`GLASS_DISPLAY` set): glass owns no Xvfb, so before this work the
/// app was fully orphaned on signal. SIGTERM must now reap it.
#[test]
#[ignore = "requires Xvfb + glass-testapp; see file header for the run command"]
fn sigterm_reaps_app_in_attach_mode() {
    let xvfb = Xvfb::start();
    let (mut child, server_pid, _stdin, _stdout) =
        start_session(&[("GLASS_DISPLAY", &xvfb.display)]);
    let app_pid = child_pid_of(server_pid, "glass-testapp").expect("app child of server");
    assert!(pid_alive(app_pid), "app should be running");

    // Keep stdin open: the server must exit via its SIGNAL path, not stdin-EOF.
    sigterm(server_pid);
    let _ = child.wait();

    if !wait_gone(app_pid, 100) {
        let _ = Command::new("kill")
            .args(["-KILL", &app_pid.to_string()])
            .status();
        panic!("app pid {app_pid} survived server SIGTERM — orphan leak");
    }
}

/// Spawned-Xvfb mode (`GLASS_DISPLAY` unset): glass spawns and owns the display.
/// SIGTERM must reap BOTH the app and the private Xvfb.
#[test]
#[ignore = "requires Xvfb + glass-testapp; see file header for the run command"]
fn sigterm_reaps_app_and_spawned_display() {
    // No GLASS_DISPLAY -> the server spawns its own private Xvfb.
    let (mut child, server_pid, _stdin, _stdout) = start_session(&[]);
    let app_pid = child_pid_of(server_pid, "glass-testapp").expect("app child of server");
    let xvfb_pid = child_pid_of(server_pid, "Xvfb").expect("spawned Xvfb child of server");
    assert!(
        pid_alive(app_pid) && pid_alive(xvfb_pid),
        "app + Xvfb should be running"
    );

    // Keep stdin open: the server must exit via its SIGNAL path, not stdin-EOF.
    sigterm(server_pid);
    let _ = child.wait();

    let app_gone = wait_gone(app_pid, 100);
    let xvfb_gone = wait_gone(xvfb_pid, 100);
    for (alive, pid) in [(!app_gone, app_pid), (!xvfb_gone, xvfb_pid)] {
        if alive {
            let _ = Command::new("kill")
                .args(["-KILL", &pid.to_string()])
                .status();
        }
    }
    assert!(app_gone, "app pid {app_pid} survived SIGTERM");
    assert!(xvfb_gone, "spawned Xvfb pid {xvfb_pid} survived SIGTERM");
}
