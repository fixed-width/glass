//! glass-windows environment checks ("glass doctor"). Pure mapping (build_checks) is
//! Linux-tested; the Windows fact-gathering (checks) is cfg(windows) + on-box validated.

// The pure mapping + facts are consumed by the cfg(windows) fact-gathering below and
// exercised by the Linux unit tests, so off-Windows non-test builds see them as dead.
#![cfg_attr(not(windows), allow(dead_code))]

use glass_core::{Check, CheckStatus};

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
    pub wgc_supported: bool,
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
    v.push(if f.wgc_supported {
        Check::new("Windows.Graphics.Capture", CheckStatus::Ok, "supported")
    } else {
        Check::new(
            "Windows.Graphics.Capture",
            CheckStatus::Fail,
            "not supported on this system",
        )
        .with_remedy("WGC needs Windows 10 1903+ with a GPU or WARP software renderer")
    });
    // 3. DPI awareness (per-monitor = physical pixels; system/unaware = virtualized coords)
    v.push(match f.dpi {
        DpiAwareness::PerMonitorV2 => {
            Check::new("DPI awareness", CheckStatus::Ok, "Per-Monitor-V2 (manifest)")
        }
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

/// Windows in-OS containment posture (Sandboxie) for the doctor. Pure → Linux-tested.
/// `sandboxie_available` = Start.exe present + SbieSvc/SbieDrv running; `dir` = resolved
/// Sandboxie dir; `prompt_global_on` = the global `PromptForInternetAccess` (Some(true) means
/// strict would deadlock-then-fail-closed), `None` if unknown; `windows_sandbox` = VM-tier hint.
pub(crate) fn build_sandbox_checks(
    sandboxie_available: bool,
    dir: &str,
    prompt_global_on: Option<bool>,
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
    if let Some(true) = prompt_global_on {
        v.push(
            Check::new(
                "strict (no-egress)",
                CheckStatus::Warn,
                "Sandboxie global PromptForInternetAccess=y would deadlock sandbox=strict (it fails closed instead)",
            )
            .with_remedy("set Sandboxie's global PromptForInternetAccess to n"),
        );
    }
    // VM tier pointer (separate, stronger deployment option)
    v.push(if windows_sandbox {
        Check::new("Windows Sandbox (VM tier)", CheckStatus::Ok, "available — strongest isolation; run glass inside it (see packaging/windows-sandbox)")
    } else {
        Check::new("Windows Sandbox (VM tier)", CheckStatus::Skip, "not available on this edition (Pro/Enterprise/Education only)")
    });
    v
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
    let prompt = gather_prompt_global(&dir);
    let ws = gather_windows_sandbox();
    build_sandbox_checks(avail, &dir, prompt, ws)
}

/// Read Sandboxie's global `PromptForInternetAccess` via `SbieIni.exe query GlobalSettings
/// PromptForInternetAccess` (from `dir`). `Some(true)` if it trims to `y`/`Y`, `Some(false)`
/// if it reads `n`/empty, `None` on any error (we never write `[GlobalSettings]`).
#[cfg(windows)]
fn gather_prompt_global(dir: &str) -> Option<bool> {
    let out = std::process::Command::new(format!(r"{dir}\SbieIni.exe"))
        .args(["query", "GlobalSettings", "PromptForInternetAccess"])
        .output()
        .ok()?;
    if !out.status.success() {
        return None;
    }
    let value = String::from_utf8_lossy(&out.stdout)
        .trim()
        .to_ascii_lowercase();
    match value.as_str() {
        "y" => Some(true),
        "n" | "" => Some(false),
        _ => None,
    }
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
#[cfg(windows)]
fn gather_wgc() -> bool {
    use windows::Graphics::Capture::GraphicsCaptureSession;
    GraphicsCaptureSession::IsSupported().unwrap_or(false)
}

/// Port of the validated PMv2 probe: classify the thread's DPI-awareness context.
#[cfg(windows)]
fn gather_dpi() -> DpiAwareness {
    use windows::Win32::UI::HiDpi::{
        AreDpiAwarenessContextsEqual, GetThreadDpiAwarenessContext,
        DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE, DPI_AWARENESS_CONTEXT_PER_MONITOR_AWARE_V2,
        DPI_AWARENESS_CONTEXT_SYSTEM_AWARE, DPI_AWARENESS_CONTEXT_UNAWARE,
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

    #[test]
    fn session0_fails_with_remedy() {
        let f = DoctorFacts {
            session: SessionKind::Session0,
            wgc_supported: true,
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
                wgc_supported: true,
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
                wgc_supported: true,
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
            wgc_supported: true,
            dpi: DpiAwareness::PerMonitorV2,
            build: Some(26100),
        };
        assert!(build_checks(&f).iter().all(|c| c.status == CheckStatus::Ok));
    }

    #[test]
    fn sandbox_available_is_ok_and_names_dir() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", Some(false), false);
        let posture = v.iter().find(|c| c.name == "in-OS containment").unwrap();
        assert_eq!(posture.status, CheckStatus::Ok);
        assert!(posture.detail.contains(r"C:\Program Files\Sandboxie"));
        // No strict-egress warning when the global prompt is off.
        assert!(v.iter().all(|c| c.name != "strict (no-egress)"));
    }

    #[test]
    fn sandbox_unavailable_warns_with_remedy() {
        let v = build_sandbox_checks(false, r"C:\Program Files\Sandboxie", None, false);
        let posture = v.iter().find(|c| c.name == "in-OS containment").unwrap();
        assert_eq!(posture.status, CheckStatus::Warn);
        assert!(posture.remedy.is_some());
        assert!(posture.detail.contains("fail closed"));
    }

    #[test]
    fn strict_egress_warns_when_global_prompt_on() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", Some(true), false);
        let egress = v.iter().find(|c| c.name == "strict (no-egress)").unwrap();
        assert_eq!(egress.status, CheckStatus::Warn);
        assert!(egress.remedy.is_some());
    }

    #[test]
    fn windows_sandbox_vm_tier_skip_when_absent() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", Some(false), false);
        let vm = v.iter().find(|c| c.name == "Windows Sandbox (VM tier)").unwrap();
        assert_eq!(vm.status, CheckStatus::Skip);
    }

    #[test]
    fn windows_sandbox_vm_tier_ok_when_present() {
        let v = build_sandbox_checks(true, r"C:\Program Files\Sandboxie", Some(false), true);
        let vm = v.iter().find(|c| c.name == "Windows Sandbox (VM tier)").unwrap();
        assert_eq!(vm.status, CheckStatus::Ok);
    }
}
