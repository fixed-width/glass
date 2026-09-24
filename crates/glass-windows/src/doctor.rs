//! glass-windows environment checks ("glass doctor"). Pure mapping (build_checks) is
//! Linux-tested; the Windows fact-gathering (checks) is cfg(windows) + on-box validated.

// The pure mapping + facts are consumed by the cfg(windows) fact-gathering below and
// exercised by the Linux unit tests, so off-Windows non-test builds see them as dead.
#![cfg_attr(not(windows), allow(dead_code))]

use std::sync::Mutex;
use std::{io, process};

use glass_core::{Check, CheckStatus, ProbeFailure};

// ---- pure, Linux-testable ----
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum SessionKind {
    Console,
    Session0,
    Other(u32),
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum DpiAwareness {
    PerMonitorV2,
    PerMonitorV1,
    System,
    Unaware,
    Unknown,
}

pub(crate) struct DoctorFacts {
    pub session: SessionKind,
    pub wgc_supported: Result<bool, ProbeFailure>,
    pub dpi: DpiAwareness,
    pub build: Option<u32>, // Windows build number; None = couldn't determine
}

/// Map gathered facts to checks. Pure — no OS calls — so it's unit-tested on Linux.
pub(crate) fn build_checks(f: &DoctorFacts) -> Vec<Check> {
    let mut v = Vec::new();
    // 1. interactive session (the defining constraint — only the active input desktop renders)
    v.push(match f.session {
        SessionKind::Console => Check::new(
            "interactive session",
            CheckStatus::Ok,
            "running in the active console session",
        ),
        SessionKind::Session0 => Check::new(
            "interactive session",
            CheckStatus::Fail,
            "running in Session 0 (service/SSH) — no rendering desktop",
        )
        .with_remedy(
            "run glass in an interactive, logged-in console session (auto-login), not a service \
             or SSH session",
        ),
        SessionKind::Other(n) => Check::new(
            "interactive session",
            CheckStatus::Warn,
            format!("session {n} is not the active console session — capture may target a non-rendering session"),
        )
        .with_remedy(
            "run on the physical/auto-login console (the VirtualDisplay provider is a follow-on plan)",
        ),
    });
    // 2. WGC
    v.push(match &f.wgc_supported {
        Ok(true) => Check::new("Windows.Graphics.Capture", CheckStatus::Ok, "supported"),
        Ok(false) => Check::new(
            "Windows.Graphics.Capture",
            CheckStatus::Fail,
            "not supported on this system",
        )
        .with_remedy("WGC needs Windows 10 1903+ with a GPU or WARP software renderer"),
        Err(failure) => Check::new(
            "Windows.Graphics.Capture",
            CheckStatus::Fail,
            format!(
                "could not determine support: {}",
                failure.detail("Windows.Graphics.Capture")
            ),
        )
        .with_remedy(match failure {
            ProbeFailure::Vanished => {
                "the WGC probe thread panicked; check glass's stderr and rerun glass doctor; \
                 report the panic if it recurs"
            }
            ProbeFailure::NotStarted(_) => {
                "the WGC probe could not start; resolve the resource error in the detail and \
                 rerun glass doctor"
            }
            _ => {
                "rerun glass doctor to retry the WGC probe; if it still fails, check the Windows \
                 error in the detail and ensure glass runs in an interactive desktop session"
            }
        }),
    });
    // 3. DPI awareness (per-monitor = physical pixels; system/unaware = virtualized coords)
    v.push(match f.dpi {
        DpiAwareness::PerMonitorV2 => Check::new(
            "DPI awareness",
            CheckStatus::Ok,
            "Per-Monitor-V2 (manifest)",
        ),
        DpiAwareness::PerMonitorV1 => Check::new(
            "DPI awareness",
            CheckStatus::Ok,
            "Per-Monitor-V1 (physical pixels)",
        ),
        DpiAwareness::System => Check::new(
            "DPI awareness",
            CheckStatus::Warn,
            "system-DPI-aware — coords/capture virtualized on scaled monitors",
        )
        .with_remedy(
            "ship the PerMonitor-V2 manifest (glass-mcp embeds it) so coords are physical pixels",
        ),
        DpiAwareness::Unaware => Check::new(
            "DPI awareness",
            CheckStatus::Warn,
            "DPI-unaware — coords/capture virtualized on scaled monitors",
        )
        .with_remedy("ship the PerMonitor-V2 manifest (glass-mcp embeds it)"),
        DpiAwareness::Unknown => Check::new(
            "DPI awareness",
            CheckStatus::Warn,
            "could not determine DPI awareness",
        )
        .with_remedy("verify the PerMonitor-V2 manifest applied (glass-mcp embeds it)"),
    });
    // 4. Windows build vs the WGC floor (1903 = build 18362)
    v.push(match f.build {
        Some(b) if b >= 18362 => Check::new("Windows build", CheckStatus::Ok, format!("build {b}")),
        Some(b) => Check::new(
            "Windows build",
            CheckStatus::Fail,
            format!("build {b} is below 18362 (Windows 10 1903)"),
        )
        .with_remedy("update to Windows 10 1903 or later for Windows.Graphics.Capture"),
        None => Check::new(
            "Windows build",
            CheckStatus::Skip,
            "could not determine the Windows build number",
        ),
    });
    v
}

#[derive(Debug)]
pub(crate) enum PromptReadError {
    QueryFailed(String),
    UnexpectedValue(String),
}

/// Report Sandboxie posture, querying the global prompt only when the provider is available.
pub(crate) fn build_sandbox_checks(
    sandboxie_available: bool,
    dir: &str,
    query_prompt: impl FnOnce() -> Result<bool, PromptReadError>,
    windows_sandbox: bool,
) -> Vec<Check> {
    let mut v = Vec::new();
    // in-OS containment provider (Sandboxie Classic)
    v.push(if sandboxie_available {
        Check::new(
            "in-OS containment",
            CheckStatus::Ok,
            format!("Sandboxie at {dir} — sandbox=default/strict run the app contained (filesystem/registry/network)"),
        )
    } else {
        Check::new(
            "in-OS containment",
            CheckStatus::Warn,
            format!("Sandboxie not available at {dir} — sandbox=default/strict fail closed"),
        )
        .with_remedy(
            "install Sandboxie Classic (sandboxie-plus.com/downloads) and ensure its service runs, \
             set GLASS_SANDBOXIE_DIR if installed elsewhere, or use sandbox=off",
        )
    });
    // strict no-egress gate
    if sandboxie_available {
        match query_prompt() {
            Ok(true) => v.push(Check::new(
                "strict (no-egress)",
                CheckStatus::Warn,
                "Sandboxie global PromptForInternetAccess=y would deadlock sandbox=strict (it fails closed instead)",
            )
            .with_remedy("set Sandboxie's global PromptForInternetAccess to n")),
            Ok(false) => {}
            Err(error) => v.push(prompt_read_error_check(dir, error)),
        }
    }
    // VM tier pointer (separate, stronger deployment option)
    v.push(if windows_sandbox {
        Check::new(
            "Windows Sandbox (VM tier)",
            CheckStatus::Ok,
            "available — strongest isolation; run glass inside it (see packaging/windows-sandbox)",
        )
    } else {
        Check::new(
            "Windows Sandbox (VM tier)",
            CheckStatus::Skip,
            "not available on this edition (Pro/Enterprise/Education only)",
        )
    });
    v
}

fn prompt_read_error_check(dir: &str, error: PromptReadError) -> Check {
    let (detail, guidance) = match error {
        PromptReadError::QueryFailed(reason) => (
            format!(
                "could not read Sandboxie global PromptForInternetAccess: {reason}; \
                 sandbox=strict fails closed"
            ),
            "check the Sandboxie installation path and executable access, resolve the reported query error",
        ),
        PromptReadError::UnexpectedValue(value) => (
            format!(
                "could not verify Sandboxie global PromptForInternetAccess: \
                 unexpected SbieIni.exe output \"{value}\"; strict compatibility is unknown"
            ),
            "inspect the returned setting value and resolve the unexpected query output",
        ),
    };
    let exe = format!(r"{dir}\SbieIni.exe").replace('\'', "''");
    Check::new("strict (no-egress)", CheckStatus::Warn, detail).with_remedy(format!(
        "in PowerShell, run & '{exe}' query GlobalSettings PromptForInternetAccess \
         as the same user running glass; {guidance}, then rerun glass doctor"
    ))
}

/// Clipboard posture: with the hook DLL the contained app gets a PRIVATE clipboard isolated from
/// the user's; without it, Layer 1 (`OpenClipboard=n`) still protects the user but the app has no
/// clipboard. Pure mapping → Linux-tested.
pub(crate) fn build_clipboard_check(hook_resolvable: bool, dll_path: &str) -> Check {
    if hook_resolvable {
        Check::new(
            "clipboard isolation",
            CheckStatus::Ok,
            format!("private clipboard active (hook {dll_path}) — isolated from your clipboard"),
        )
    } else {
        Check::new(
            "clipboard isolation",
            CheckStatus::Warn,
            "contained app clipboard disabled (hook DLL not found); your clipboard is protected",
        )
        .with_remedy(
            "set GLASS_CLIP_SHIM_DLL or reinstall so the boxed app gets a private clipboard",
        )
    }
}

// ---- Windows fact-gathering (cfg(windows), on-box validated) ----
#[cfg(windows)]
pub fn checks(_deep: bool) -> Vec<Check> {
    build_checks(&gather_facts())
}

/// Windows sandbox section for the doctor (pure mapping + the host probes).
#[cfg(windows)]
pub fn sandbox_checks() -> Vec<Check> {
    let dir = crate::containment::sandboxie_dir();
    let avail = crate::containment::available(&dir);
    let ws = gather_windows_sandbox();
    let mut v = build_sandbox_checks(avail, &dir, || gather_prompt_global(&dir), ws);
    let exe_dir = std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(|d| d.to_string_lossy().into_owned()));
    let dll = crate::containment::config_shim_dll_path(exe_dir.as_deref());
    let resolvable = dll
        .as_deref()
        .map(|p| std::path::Path::new(p).exists())
        .unwrap_or(false);
    v.push(build_clipboard_check(
        resolvable,
        dll.as_deref().unwrap_or("<unset>"),
    ));
    v
}

/// Read the global setting without writing `[GlobalSettings]`.
#[cfg(windows)]
fn gather_prompt_global(dir: &str) -> Result<bool, PromptReadError> {
    gather_prompt_global_with(dir, process::Command::output)
}

fn gather_prompt_global_with(
    dir: &str,
    run: impl FnOnce(&mut process::Command) -> io::Result<process::Output>,
) -> Result<bool, PromptReadError> {
    let out = run(process::Command::new(format!(r"{dir}\SbieIni.exe")).args([
        "query",
        "GlobalSettings",
        "PromptForInternetAccess",
    ]))
    .map_err(|error| {
        PromptReadError::QueryFailed(format!("could not start SbieIni.exe: {error}"))
    })?;
    if !out.status.success() {
        return Err(PromptReadError::QueryFailed(format!(
            "SbieIni.exe query returned {}; stderr: \"{}\"; stdout: \"{}\"",
            out.status,
            prompt_diagnostic(&out.stderr),
            prompt_diagnostic(&out.stdout),
        )));
    }
    let value = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase();
    match value.as_str() {
        "y" => Ok(true),
        "n" | "" => Ok(false),
        _ => Err(PromptReadError::UnexpectedValue(prompt_diagnostic(
            &out.stdout,
        ))),
    }
}

fn prompt_diagnostic(bytes: &[u8]) -> String {
    const MAX: usize = 256;
    let mut detail = String::from_utf8_lossy(&bytes[..bytes.len().min(MAX)])
        .trim()
        .escape_debug()
        .to_string();
    if bytes.len() > MAX {
        detail.push_str("...");
    }
    detail
}

/// Whether this host can run the Windows Sandbox VM tier. The optional feature installs
/// `WindowsSandbox.exe` into System32; its presence is a cheap, reliable proxy.
#[cfg(windows)]
fn gather_windows_sandbox() -> bool {
    let windir = std::env::var("WINDIR").unwrap_or_else(|_| r"C:\Windows".to_string());
    std::path::Path::new(&windir)
        .join("System32")
        .join("WindowsSandbox.exe")
        .exists()
}

#[cfg(windows)]
fn gather_facts() -> DoctorFacts {
    DoctorFacts {
        session: gather_session(),
        wgc_supported: gather_wgc(),
        dpi: gather_dpi(),
        build: gather_build(),
    }
}

/// Our session vs the active console session: Session 0 (service/SSH) can't render;
/// a non-console session may target a non-rendering desktop.
#[cfg(windows)]
fn gather_session() -> SessionKind {
    use windows::Win32::System::RemoteDesktop::{
        ProcessIdToSessionId, WTSGetActiveConsoleSessionId,
    };
    use windows::Win32::System::Threading::GetCurrentProcessId;

    let mut sid: u32 = 0;
    // SAFETY: GetCurrentProcessId is infallible; ProcessIdToSessionId writes our
    // session id into the local `sid` (a valid, exclusively-borrowed u32). On failure
    // `sid` stays 0, which we treat as Session 0 (the conservative "no desktop" answer).
    let ok = unsafe { ProcessIdToSessionId(GetCurrentProcessId(), &mut sid) }.is_ok();
    if !ok {
        // ProcessIdToSessionId on our own PID effectively never fails; if it somehow does we
        // can't tell our session, so we conservatively assume the worst (Session 0 / Fail)
        // rather than reporting a falsely-healthy session.
        return SessionKind::Session0;
    }
    // SAFETY: WTSGetActiveConsoleSessionId takes no arguments and returns the active
    // console session id (u32::MAX if none is attached); no pointers involved.
    let console = unsafe { WTSGetActiveConsoleSessionId() };
    // WTSGetActiveConsoleSessionId returns 0xFFFFFFFF (u32::MAX) when no session is attached to
    // the physical console (headless / mid-transition); we then classify as Other(sid) → a Warn,
    // which is the right severity (no active console desktop) even if the detail wording is generic.
    if sid == 0 {
        SessionKind::Session0
    } else if sid == console {
        SessionKind::Console
    } else {
        SessionKind::Other(sid)
    }
}

/// Whether Windows.Graphics.Capture is usable on this system (the real capability gate).
///
/// Successful answers are cached; failed probes are retried on the next call.
/// `IsSupported` activates a WinRT runtime class on
/// whatever thread calls it, and doing that on a borrowed thread that later exits faults with
/// `STATUS_ACCESS_VIOLATION`: `cargo test --workspace` on Windows crashed the `glass-mcp` test
/// binary about half the time, libtest giving each test its own short-lived thread. Caching alone
/// does not fix it — one activation on a borrowed thread is enough — so the apartment is
/// initialized and torn down here, on a thread whose lifetime is bounded by the `join` below.
#[cfg(windows)]
fn gather_wgc() -> Result<bool, ProbeFailure> {
    static SUPPORTED: Mutex<Option<bool>> = Mutex::new(None);
    gather_wgc_with(&SUPPORTED, probe_wgc)
}

/// Serialize probes on owned threads, caching only completed capability answers.
fn gather_wgc_with(
    cache: &Mutex<Option<bool>>,
    probe: impl FnOnce() -> Result<bool, ProbeFailure> + Send + 'static,
) -> Result<bool, ProbeFailure> {
    let mut cached = cache.lock().unwrap_or_else(|e| e.into_inner());
    if let Some(supported) = *cached {
        return Ok(supported);
    }
    let supported = std::thread::Builder::new()
        .name("glass-wgc-probe".into())
        .spawn(probe)
        .map_err(|e| ProbeFailure::NotStarted(e.to_string()))?
        .join()
        .map_err(|_| ProbeFailure::Vanished)??;
    *cached = Some(supported);
    Ok(supported)
}

/// Runs only on the owned thread created by [`gather_wgc_with`].
#[cfg(windows)]
fn probe_wgc() -> Result<bool, ProbeFailure> {
    use windows::Graphics::Capture::GraphicsCaptureSession;
    use windows::Win32::System::Com::{COINIT_MULTITHREADED, CoInitializeEx, CoUninitialize};

    struct Apartment;
    impl Drop for Apartment {
        fn drop(&mut self) {
            // SAFETY: constructed only after successful CoInitializeEx on this same thread;
            // balances S_OK and S_FALSE, including when the probe unwinds.
            unsafe { CoUninitialize() };
        }
    }

    // SAFETY: initializes COM on our owned probe thread; no borrowed pointers.
    unsafe { CoInitializeEx(None, COINIT_MULTITHREADED) }
        .ok()
        .map_err(|e| ProbeFailure::Failed(format!("CoInitializeEx: {e}")))?;
    let _apartment = Apartment;
    GraphicsCaptureSession::IsSupported()
        .map_err(|e| ProbeFailure::Failed(format!("GraphicsCaptureSession::IsSupported: {e}")))
}

/// Port of the validated PMv2 probe: classify the thread's DPI-awareness context.
#[cfg(windows)]
fn gather_dpi() -> DpiAwareness {
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2, DPI_AWARENESS_CONTEXT_SYSTEM_AWARE,
        DPI_AWARENESS_CONTEXT_UNAWARE, GetThreadDpiAwarenessContext,
    };

    // SAFETY: GetThreadDpiAwarenessContext returns a pseudo-handle for the calling
    // thread; AreDpiAwarenessContextsEqual only compares two such handles. No memory
    // is dereferenced and the handles outlive the comparison.
    let ctx = unsafe { GetThreadDpiAwarenessContext() };
    let eq = |c| unsafe { AreDpiAwarenessContextsEqual(ctx, c) }.as_bool();
    if eq(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2) {
        DpiAwareness::PerMonitorV2
    } else if eq(DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE) {
        DpiAwareness::PerMonitorV1
    } else if eq(DPI_AWARENESS_CONTEXT_SYSTEM_AWARE) {
        DpiAwareness::System
    } else if eq(DPI_AWARENESS_CONTEXT_UNAWARE) {
        DpiAwareness::Unaware
    } else {
        DpiAwareness::Unknown
    }
}

/// True build number via `RtlGetVersion` (unaffected by the app's compatibility
/// manifest, unlike the Win32 `GetVersionEx`).
#[cfg(windows)]
fn gather_build() -> Option<u32> {
    use windows::Wdk::System::SystemServices::RtlGetVersion;
    use windows::Win32::System::SystemInformation::OSVERSIONINFOW;

    let mut info = OSVERSIONINFOW {
        dwOSVersionInfoSize: std::mem::size_of::<OSVERSIONINFOW>() as u32,
        ..Default::default()
    };
    // SAFETY: RtlGetVersion fills the OSVERSIONINFOW we exclusively own; we set its
    // dwOSVersionInfoSize first as the API expects. It returns STATUS_SUCCESS (0) for a
    // correctly-sized struct, but we still gate on the returned NTSTATUS before reading.
    let status = unsafe { RtlGetVersion(&mut info) };
    (status.0 == 0).then_some(info.dwBuildNumber)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn wgc_check(wgc_supported: Result<bool, ProbeFailure>) -> Check {
        build_checks(&DoctorFacts {
            session: SessionKind::Console,
            wgc_supported,
            dpi: DpiAwareness::PerMonitorV2,
            build: Some(26100),
        })
        .into_iter()
        .find(|c| c.name == "Windows.Graphics.Capture")
        .unwrap()
    }

    #[test]
    fn wgc_unsupported_has_os_and_renderer_remedy() {
        let check = wgc_check(Ok(false));
        assert_eq!(check.status, CheckStatus::Fail);
        assert_eq!(check.detail, "not supported on this system");
        assert!(check.remedy.unwrap().contains("Windows 10 1903+"));
    }

    #[test]
    fn wgc_error_preserves_hresult_and_offers_retry() {
        let check = wgc_check(Err(ProbeFailure::Failed(
            "GraphicsCaptureSession::IsSupported: Class not registered (0x80040154)".into(),
        )));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("could not determine support"));
        assert!(
            check
                .detail
                .contains("IsSupported: Class not registered (0x80040154)")
        );
        let remedy = check.remedy.unwrap();
        assert!(remedy.contains("retry"));
        assert!(!remedy.contains("1903"));
    }

    #[test]
    fn wgc_panic_names_thread_and_stderr() {
        let check = wgc_check(Err(ProbeFailure::Vanished));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("thread unwound"));
        let remedy = check.remedy.unwrap();
        assert!(remedy.contains("panicked"));
        assert!(remedy.contains("stderr"));
        assert!(!remedy.contains("1903"));
    }

    #[test]
    fn wgc_start_failure_names_resource_error() {
        let check = wgc_check(Err(ProbeFailure::NotStarted("thread limit".into())));
        assert_eq!(check.status, CheckStatus::Fail);
        assert!(check.detail.contains("could not start"));
        assert!(check.detail.contains("thread limit"));
        let remedy = check.remedy.unwrap();
        assert!(remedy.contains("resource error"));
        assert!(!remedy.contains("1903"));
    }

    #[test]
    fn wgc_caches_both_supported_and_unsupported_answers() {
        for supported in [true, false] {
            let cache = Mutex::new(None);
            assert_eq!(
                gather_wgc_with(&cache, move || Ok(supported)),
                Ok(supported)
            );
            assert_eq!(
                gather_wgc_with(&cache, || panic!("cached answer must not probe again")),
                Ok(supported)
            );
        }
    }

    #[test]
    fn wgc_retries_errors_until_a_capability_answer_is_cached() {
        let cache = Mutex::new(None);
        for _ in 0..2 {
            let error = ProbeFailure::Failed("activation failed (0x80040154)".into());
            let expected = error.clone();
            assert_eq!(gather_wgc_with(&cache, move || Err(error)), Err(expected));
        }
        assert_eq!(gather_wgc_with(&cache, || Ok(true)), Ok(true));
        assert_eq!(
            gather_wgc_with(&cache, || panic!("successful retry must be cached")),
            Ok(true)
        );
    }

    #[test]
    fn wgc_retries_after_probe_thread_panics() {
        let cache = Mutex::new(None);
        assert_eq!(
            gather_wgc_with(&cache, || panic!("injected WGC probe panic")),
            Err(ProbeFailure::Vanished)
        );
        assert_eq!(gather_wgc_with(&cache, || Ok(true)), Ok(true));
        assert_eq!(
            gather_wgc_with(&cache, || panic!("successful retry must be cached")),
            Ok(true)
        );
    }

    #[test]
    fn wgc_probes_on_an_owned_thread() {
        let caller = std::thread::current().id();
        assert_eq!(
            gather_wgc_with(&Mutex::new(None), move || {
                assert_ne!(std::thread::current().id(), caller);
                Ok(true)
            }),
            Ok(true)
        );
    }

    #[test]
    fn concurrent_wgc_calls_share_one_successful_probe() {
        use std::sync::atomic::{AtomicUsize, Ordering};
        use std::sync::{Arc, Barrier};

        let cache = Arc::new(Mutex::new(None));
        let calls = Arc::new(AtomicUsize::new(0));
        let ready = Arc::new(Barrier::new(8));
        let threads: Vec<_> = (0..8)
            .map(|_| {
                let cache = Arc::clone(&cache);
                let calls = Arc::clone(&calls);
                let ready = Arc::clone(&ready);
                std::thread::spawn(move || {
                    ready.wait();
                    gather_wgc_with(&cache, move || {
                        calls.fetch_add(1, Ordering::SeqCst);
                        Ok(true)
                    })
                })
            })
            .collect();
        for thread in threads {
            assert_eq!(thread.join().unwrap(), Ok(true));
        }
        assert_eq!(calls.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn session0_fails_with_remedy() {
        let f = DoctorFacts {
            session: SessionKind::Session0,
            wgc_supported: Ok(true),
            dpi: DpiAwareness::PerMonitorV2,
            build: Some(26100),
        };
        let c = build_checks(&f);
        let s = c.iter().find(|c| c.name == "interactive session").unwrap();
        assert_eq!(s.status, CheckStatus::Fail);
        assert!(s.remedy.is_some());
    }

    #[test]
    fn old_build_fails_recent_ok() {
        assert_eq!(
            build_checks(&DoctorFacts {
                session: SessionKind::Console,
                wgc_supported: Ok(true),
                dpi: DpiAwareness::PerMonitorV2,
                build: Some(17763)
            })
            .iter()
            .find(|c| c.name == "Windows build")
            .unwrap()
            .status,
            CheckStatus::Fail
        );
        assert_eq!(
            build_checks(&DoctorFacts {
                session: SessionKind::Console,
                wgc_supported: Ok(true),
                dpi: DpiAwareness::PerMonitorV2,
                build: Some(18362)
            })
            .iter()
            .find(|c| c.name == "Windows build")
            .unwrap()
            .status,
            CheckStatus::Ok
        );
    }

    #[test]
    fn all_green_when_healthy() {
        let f = DoctorFacts {
            session: SessionKind::Console,
            wgc_supported: Ok(true),
            dpi: DpiAwareness::PerMonitorV2,
            build: Some(26100),
        };
        assert!(build_checks(&f).iter().all(|c| c.status == CheckStatus::Ok));
    }

    #[test]
    fn sandbox_available_is_ok_and_names_dir() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", || Ok(false), false);
        let posture = v.iter().find(|c| c.name == "in-OS containment").unwrap();
        assert_eq!(posture.status, CheckStatus::Ok);
        assert!(posture.detail.contains(r"C:\Program Files\Sandboxie"));
        // No strict-egress warning when the global prompt is off.
        assert!(v.iter().all(|c| c.name != "strict (no-egress)"));
    }

    #[test]
    fn sandbox_unavailable_warns_with_remedy() {
        let v = build_sandbox_checks(
            false,
            r"C:\Program Files\Sandboxie",
            || panic!("must not query an unavailable provider"),
            false,
        );
        let posture = v.iter().find(|c| c.name == "in-OS containment").unwrap();
        assert_eq!(posture.status, CheckStatus::Warn);
        assert!(posture.remedy.is_some());
        assert!(posture.detail.contains("fail closed"));
        assert!(v.iter().all(|check| check.name != "strict (no-egress)"));
    }

    #[test]
    fn strict_egress_warns_when_global_prompt_on() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", || Ok(true), false);
        let egress = v.iter().find(|c| c.name == "strict (no-egress)").unwrap();
        assert_eq!(egress.status, CheckStatus::Warn);
        assert!(egress.remedy.is_some());
    }

    #[test]
    fn strict_egress_warns_when_global_prompt_unreadable() {
        let dir = r"C:\Users\O'Brien\Sandboxie";
        let checks = build_sandbox_checks(
            true,
            dir,
            || {
                gather_prompt_global_with(dir, |_| {
                    Err(io::Error::new(
                        io::ErrorKind::PermissionDenied,
                        "Access denied (os error 5)",
                    ))
                })
            },
            false,
        );
        let egress = checks
            .iter()
            .find(|check| check.name == "strict (no-egress)")
            .expect("unreadable setting must remain visible");
        assert_eq!(egress.status, CheckStatus::Warn);
        assert!(egress.detail.contains("could not start SbieIni.exe"));
        assert!(egress.detail.contains("Access denied (os error 5)"));
        assert!(egress.detail.contains("sandbox=strict fails closed"));
        let remedy = egress.remedy.as_ref().unwrap();
        assert!(remedy.contains(r"& 'C:\Users\O''Brien\Sandboxie\SbieIni.exe' query GlobalSettings PromptForInternetAccess"));
        assert!(remedy.contains("same user"));
        assert!(remedy.contains("rerun glass doctor"));
        assert!(!remedy.contains("to n"));
        let report =
            glass_core::Diagnosis::new(vec![glass_core::Section::new("sandbox", None, checks)]);
        assert_eq!(report.overall("windows"), CheckStatus::Warn);
        assert_eq!(report.exit_code("windows"), 0);
        assert!(
            report
                .render_text("windows")
                .contains("Access denied (os error 5)")
        );
    }

    fn prompt_output(code: u32, stdout: &[u8], stderr: &[u8]) -> process::Output {
        #[cfg(unix)]
        let status = {
            use std::os::unix::process::ExitStatusExt;
            process::ExitStatus::from_raw((code as i32) << 8)
        };
        #[cfg(windows)]
        let status = {
            use std::os::windows::process::ExitStatusExt;
            process::ExitStatus::from_raw(code)
        };
        process::Output {
            status,
            stdout: stdout.to_vec(),
            stderr: stderr.to_vec(),
        }
    }

    #[test]
    fn prompt_query_uses_resolved_executable_and_read_only_arguments() {
        let enabled = gather_prompt_global_with(r"C:\Custom Sandboxie", |command| {
            assert_eq!(command.get_program(), r"C:\Custom Sandboxie\SbieIni.exe");
            assert_eq!(
                command.get_args().collect::<Vec<_>>(),
                ["query", "GlobalSettings", "PromptForInternetAccess"]
            );
            Ok(prompt_output(0, b"Y\r\n", b""))
        })
        .unwrap();
        assert!(enabled);
    }

    #[test]
    fn prompt_query_preserves_known_values_and_unset_setting() {
        for (value, expected) in [
            ("y", true),
            (" Y\r\n", true),
            ("n", false),
            (" N\r\n", false),
            ("", false),
            (" \r\n\t", false),
        ] {
            let result =
                gather_prompt_global_with("S", |_| Ok(prompt_output(0, value.as_bytes(), b"")));
            assert_eq!(result.unwrap(), expected, "query output {value:?}");
        }
    }

    #[test]
    fn strict_egress_reports_query_exit_status_and_both_output_streams() {
        for (stdout, stderr) in [
            ("", "Access denied"),
            ("configuration error", ""),
            ("config error", "service error"),
            ("", ""),
        ] {
            let status = prompt_output(5, b"", b"").status.to_string();
            let checks = build_sandbox_checks(
                true,
                "S",
                || {
                    gather_prompt_global_with("S", |_| {
                        Ok(prompt_output(5, stdout.as_bytes(), stderr.as_bytes()))
                    })
                },
                false,
            );
            let check = checks
                .iter()
                .find(|check| check.name == "strict (no-egress)")
                .unwrap();
            assert_eq!(check.status, CheckStatus::Warn);
            assert!(check.detail.contains(&status));
            assert!(check.detail.contains(&format!("stderr: \"{stderr}\"")));
            assert!(check.detail.contains(&format!("stdout: \"{stdout}\"")));
            assert!(check.detail.contains("sandbox=strict fails closed"));
            assert!(
                check
                    .remedy
                    .as_ref()
                    .unwrap()
                    .contains("resolve the reported query error")
            );
        }
    }

    #[test]
    fn strict_egress_reports_unexpected_output_without_claiming_launch_refusal() {
        for value in [b"Maybe".as_slice(), b"y\r\nn", b"\xff"] {
            let checks = build_sandbox_checks(
                true,
                "S",
                || gather_prompt_global_with("S", |_| Ok(prompt_output(0, value, b""))),
                false,
            );
            let check = checks
                .iter()
                .find(|check| check.name == "strict (no-egress)")
                .unwrap();
            assert_eq!(check.status, CheckStatus::Warn);
            assert!(check.detail.contains("unexpected SbieIni.exe output"));
            assert!(check.detail.contains(&prompt_diagnostic(value)));
            assert!(check.detail.contains("strict compatibility is unknown"));
            assert!(!check.detail.contains("fails closed"));
            let remedy = check.remedy.as_ref().unwrap();
            assert!(remedy.contains("query GlobalSettings PromptForInternetAccess"));
            assert!(remedy.contains("inspect the returned setting value"));
            assert!(!remedy.contains("to n"));
        }
    }

    #[test]
    fn prompt_failure_output_is_bounded_and_escapes_control_characters() {
        let mut output = b"\x1b[31merror\r\n".to_vec();
        output.extend("é".repeat(300).as_bytes());
        let checks = build_sandbox_checks(
            true,
            "S",
            || gather_prompt_global_with("S", |_| Ok(prompt_output(1, &output, &output))),
            false,
        );
        let check = checks
            .iter()
            .find(|check| check.name == "strict (no-egress)")
            .unwrap();
        assert!(!check.detail.contains('\x1b'));
        assert!(!check.detail.contains('\n'));
        assert!(check.detail.contains("\\r\\n"));
        assert!(check.detail.contains("..."));
        assert!(check.detail.len() < 1000);
    }

    #[cfg(windows)]
    #[test]
    fn prompt_query_reports_missing_executable_on_windows() {
        let dir = tempfile::tempdir().unwrap();
        let error = gather_prompt_global(dir.path().to_str().unwrap()).unwrap_err();
        assert!(
            matches!(error, PromptReadError::QueryFailed(ref detail) if detail.contains("could not start SbieIni.exe"))
        );
    }

    #[test]
    fn windows_sandbox_vm_tier_skip_when_absent() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", || Ok(false), false);
        let vm = v
            .iter()
            .find(|c| c.name == "Windows Sandbox (VM tier)")
            .unwrap();
        assert_eq!(vm.status, CheckStatus::Skip);
    }

    #[test]
    fn windows_sandbox_vm_tier_ok_when_present() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", || Ok(false), true);
        let vm = v
            .iter()
            .find(|c| c.name == "Windows Sandbox (VM tier)")
            .unwrap();
        assert_eq!(vm.status, CheckStatus::Ok);
    }

    #[test]
    fn clipboard_check_reports_layer() {
        // hook resolvable → private clipboard active; else Layer-1-only (disabled-for-safety).
        let ok = build_clipboard_check(true, "C:\\g\\glass_clip_shim_windows.dll");
        assert_eq!(ok.status, CheckStatus::Ok);
        assert!(ok.detail.contains("private clipboard"));
        let warn = build_clipboard_check(false, "C:\\g\\glass_clip_shim_windows.dll");
        assert_eq!(warn.status, CheckStatus::Warn);
        assert!(warn.detail.contains("disabled"));
    }
}
