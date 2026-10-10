//! Run in an interactive Windows test session with `--ignored --nocapture`.
#![cfg(windows)]
#![allow(unsafe_code)]

use std::sync::{Mutex, mpsc};
use std::time::Duration;

use glass_core::{AppSpec, GlassError, Platform, SandboxLevel, WindowHint};
use glass_windows::WindowsPlatform;
use windows::Win32::Foundation::{HWND, LPARAM, WPARAM};
use windows::Win32::UI::WindowsAndMessaging::{
    CreateWindowExW, DestroyWindow, DispatchMessageW, GetWindowThreadProcessId, IsWindow, MSG,
    PM_REMOVE, PeekMessageW, WINDOW_EX_STYLE, WM_CLOSE, WS_OVERLAPPEDWINDOW, WS_VISIBLE,
};
use windows::core::{HSTRING, w};

static SERIAL: Mutex<()> = Mutex::new(());

struct ExternalWindow {
    raw: isize,
    title: String,
    stop: mpsc::Sender<()>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl ExternalWindow {
    fn new() -> Self {
        let title = format!("glass-external-stop-{}", std::process::id());
        let window_title = title.clone();
        let (ready_tx, ready_rx) = mpsc::channel();
        let (stop, stopped) = mpsc::channel();
        let thread = std::thread::spawn(move || {
            // SAFETY: STATIC is a system window class; this thread owns and pumps the window.
            let hwnd = unsafe {
                CreateWindowExW(
                    WINDOW_EX_STYLE::default(),
                    w!("STATIC"),
                    &HSTRING::from(window_title),
                    WS_OVERLAPPEDWINDOW | WS_VISIBLE,
                    20,
                    20,
                    320,
                    200,
                    None,
                    None,
                    None,
                    None,
                )
                .unwrap()
            };
            ready_tx.send(hwnd.0 as isize).unwrap();
            loop {
                // SAFETY: only this fixture thread dispatches or destroys its own window.
                unsafe {
                    if !IsWindow(Some(hwnd)).as_bool() {
                        break;
                    }
                    if stopped.try_recv().is_ok() {
                        DestroyWindow(hwnd).unwrap();
                        break;
                    }
                    let mut message = MSG::default();
                    while PeekMessageW(&mut message, None, 0, 0, PM_REMOVE).as_bool() {
                        DispatchMessageW(&message);
                    }
                }
                std::thread::sleep(Duration::from_millis(10));
            }
        });
        let raw = ready_rx.recv_timeout(Duration::from_secs(10)).unwrap();
        Self {
            raw,
            title,
            stop,
            thread: Some(thread),
        }
    }
}

impl Drop for ExternalWindow {
    fn drop(&mut self) {
        let _ = self.stop.send(());
        self.thread.take().unwrap().join().unwrap();
    }
}

fn owner(raw: isize) -> u32 {
    let mut pid = 0;
    // SAFETY: a read-only window query; stale handles return zero.
    unsafe {
        GetWindowThreadProcessId(HWND(raw as *mut _), Some(&mut pid));
    }
    pid
}

#[test]
#[ignore = "needs an interactive Windows test desktop"]
fn stop_reports_a_surviving_external_window() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let window = ExternalWindow::new();
    let mut platform = WindowsPlatform::new().unwrap();
    let spec = external_spec(&window);
    platform.start_app(&spec).unwrap();
    assert_eq!(platform.active_window_handle(), Some(window.raw as i64));
    eprintln!(
        "external: root={:?} tracked={:?} owner={}",
        platform.app_pid(),
        platform.app_pids(),
        owner(window.raw)
    );
    let stopped = platform.stop_app();
    eprintln!(
        "external: stop={stopped:?} remaining_owner={}",
        owner(window.raw)
    );
    assert!(
        matches!(stopped, Err(GlassError::Backend(ref message)) if message.contains("partial stop"))
    );
    assert_eq!(
        owner(window.raw),
        std::process::id(),
        "the unrelated process must remain alive"
    );
    assert_eq!(platform.app_pid(), None);
    assert_eq!(platform.active_window_handle(), None);
    platform.stop_app().expect("repeated stop is harmless");
}

fn external_spec(window: &ExternalWindow) -> AppSpec {
    AppSpec {
        run: vec![
            std::env::current_exe().unwrap().display().to_string(),
            "--list".into(),
        ],
        window_hint: Some(WindowHint {
            title: Some(window.title.clone()),
            class: None,
        }),
        timeout_ms: 10_000,
        sandbox: SandboxLevel::Off,
        build: None,
        cwd: None,
        env: vec![],
        a11y: false,
    }
}

#[test]
#[ignore = "needs an interactive Windows test desktop"]
fn stop_succeeds_when_the_external_window_already_closed() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let window = ExternalWindow::new();
    let mut platform = WindowsPlatform::new().unwrap();
    platform.start_app(&external_spec(&window)).unwrap();
    drop(window);
    platform.stop_app().expect("the external window is gone");
}

#[test]
#[ignore = "needs an interactive Windows test desktop"]
fn dropping_platform_leaves_the_external_window_untouched() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let window = ExternalWindow::new();
    let mut platform = WindowsPlatform::new().unwrap();
    platform.start_app(&external_spec(&window)).unwrap();
    drop(platform);
    assert_eq!(owner(window.raw), std::process::id());
}

#[test]
#[ignore = "needs an interactive Windows test desktop and Firefox"]
fn firefox_stop_does_not_silently_leave_its_window_running() {
    let _serial = SERIAL.lock().unwrap_or_else(|e| e.into_inner());
    let firefox = ["ProgramFiles", "ProgramFiles(x86)"]
        .into_iter()
        .filter_map(std::env::var_os)
        .map(|base| std::path::PathBuf::from(base).join(r"Mozilla Firefox\firefox.exe"))
        .find(|path| path.exists())
        .expect("Firefox must be installed for this test");
    let profile = tempfile::Builder::new()
        .prefix("glass-firefox-stop-")
        .tempdir()
        .unwrap();
    std::fs::write(
        profile.path().join("user.js"),
        concat!(
            "user_pref(\"browser.aboutwelcome.enabled\", false);\n",
            "user_pref(\"browser.shell.checkDefaultBrowser\", false);\n",
            "user_pref(\"browser.startup.homepage_override.mstone\", \"ignore\");\n",
            "user_pref(\"datareporting.policy.dataSubmissionPolicyBypassNotification\", true);\n",
        ),
    )
    .unwrap();
    let title = format!("glass-firefox-stop-{}", std::process::id());
    let page = profile.path().join("fixture.html");
    std::fs::write(&page, format!("<title>{title}</title><p>Stop fixture</p>")).unwrap();
    let spec = AppSpec {
        run: vec![
            firefox.display().to_string(),
            "--no-remote".into(),
            "--new-instance".into(),
            "--profile".into(),
            profile.path().display().to_string(),
            format!("file:///{}", page.display().to_string().replace('\\', "/")),
        ],
        window_hint: Some(WindowHint {
            title: Some(title),
            class: None,
        }),
        timeout_ms: 30_000,
        sandbox: SandboxLevel::Off,
        build: None,
        cwd: None,
        env: vec![],
        a11y: false,
    };
    let mut platform = WindowsPlatform::new().unwrap();
    platform.start_app(&spec).unwrap();
    let raw = platform.active_window_handle().unwrap() as isize;
    eprintln!(
        "firefox: root={:?} tracked={:?} owner={} windows={:?}",
        platform.app_pid(),
        platform.app_pids(),
        owner(raw),
        platform.list_windows()
    );
    let stopped = platform.stop_app();
    let surviving_owner = owner(raw);
    eprintln!("firefox: stop={stopped:?} remaining_owner={surviving_owner}");
    // SAFETY: close only the surviving window opened with this test's unique profile and title.
    if surviving_owner != 0 {
        unsafe {
            let _ = windows::Win32::UI::WindowsAndMessaging::PostMessageW(
                Some(HWND(raw as *mut _)),
                WM_CLOSE,
                WPARAM(0),
                LPARAM(0),
            );
        }
    }
    for _ in 0..100 {
        if owner(raw) == 0 {
            break;
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    assert!(
        surviving_owner == 0 || stopped.is_err(),
        "Firefox survived a successful stop"
    );
    assert_eq!(owner(raw), 0, "test cleanup must close its Firefox window");
}
