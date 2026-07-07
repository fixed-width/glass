//! glass-mcp library root. `main.rs` is a thin binary over this so the serve
//! path is reachable from integration tests.

pub mod audit;
pub mod cli;
pub mod doctor;
mod env;
pub(crate) mod health;
pub mod launch;
// `menubar::run` takes a `serve::config::ServeConfig`, so — like `serve` itself — this module
// only exists when the network transport is compiled in; a `--no-default-features` ("free",
// stdio-only) build has no HTTP transport for `--menubar` to serve over (see main.rs's Serve
// arm, which already bails for plain `serve --http` in that build).
#[cfg(feature = "network")]
pub mod menubar;
pub mod onboarding;
mod params;
#[cfg(feature = "network")]
pub mod serve;
pub(crate) mod server;
pub mod setup;
pub(crate) mod shutdown;
pub(crate) mod status;
mod tools;
mod untrusted;

use std::time::Duration;

use anyhow::Context;
use glass_core::{Backend, BaselineStore, Glass, GlassError, Platform, Result};
#[cfg(target_os = "linux")]
use glass_wayland::WaylandPlatform;
#[cfg(target_os = "linux")]
use glass_x11::X11Platform;
use rmcp::transport::stdio;
use rmcp::ServiceExt;

use crate::server::GlassServer;

#[cfg(not(any(target_os = "linux", windows, target_os = "macos")))]
compile_error!(
    "glass-mcp has no display backend for this target OS; add one (a Platform impl \
     mirroring the linux/windows/macos backends) plus its make_platform + doctor arms"
);

/// Construct a backend by name. The only place that knows the concrete backends;
/// passed to `Glass` as a factory so the backend is built per `glass_start`.
pub fn make_platform(
    backend: &str,
    registry: &glass_android::EmulatorRegistry,
    agents: &glass_android::AgentRegistry,
    a11y: &glass_android::A11yServiceRegistry,
) -> Result<Backend> {
    if backend == "android" {
        let platform = glass_android::AndroidPlatform::from_env(registry, agents)?;
        let get = |k: &str| std::env::var(k).ok();
        let accessibility: Option<Box<dyn glass_core::Accessibility + Send>> =
            match glass_android::a11y_apk(&get) {
                Some(apk) => match a11y.ensure(&platform.resolved_adb(), &apk) {
                    // The package isn't known until start_app; the device service serves the
                    // ACTIVE window regardless, so an empty package is correct for the MVP.
                    Ok(client) => Some(Box::new(glass_android::ServiceA11y::new(
                        client,
                        String::new(),
                    ))),
                    Err(e) => {
                        eprintln!(
                            "glass-android: a11y service unavailable, using uiautomator: {e}"
                        );
                        Some(Box::new(glass_android::AndroidA11y::for_adb(
                            platform.resolved_adb(),
                        )))
                    }
                },
                None => Some(Box::new(glass_android::AndroidA11y::for_adb(
                    platform.resolved_adb(),
                ))),
            };
        let platform: Box<dyn Platform + Send> = Box::new(platform);
        return Ok(Backend {
            platform,
            accessibility,
        });
    }
    let platform: Box<dyn Platform + Send> = match backend {
        #[cfg(target_os = "linux")]
        "wayland" => Box::new(WaylandPlatform::new()?),
        #[cfg(target_os = "linux")]
        "x11" => Box::new(X11Platform::from_env()?),
        #[cfg(windows)]
        "windows" => Box::new(glass_windows::WindowsPlatform::new()?),
        #[cfg(target_os = "macos")]
        "macos" => Box::new(glass_macos::MacosPlatform::new()?),
        other => {
            #[cfg(target_os = "linux")]
            let valid = "\"x11\", \"wayland\", or \"android\"";
            #[cfg(windows)]
            let valid = "\"windows\" or \"android\"";
            #[cfg(target_os = "macos")]
            let valid = "\"macos\" or \"android\"";
            return Err(GlassError::Backend(format!(
                "unknown backend {other:?}; use {valid}"
            )));
        }
    };
    // On Linux, AT-SPI serves both display backends, so the same reader is attached
    // to each. It connects lazily on first snapshot; an absent a11y bus surfaces as
    // AccessibilityUnavailable at call time, not here.
    // Accessibility is per-OS: AT-SPI on Linux, UI Automation on Windows, AXUIElement on
    // macOS. Each reader is attached unconditionally here — no silent fallback: a
    // reader-specific failure (e.g. Linux's AT-SPI bus being unreachable, or macOS's
    // Accessibility TCC grant being missing) surfaces as `AccessibilityUnavailable`
    // (Linux/Windows) or `PermissionDenied` (macOS) at call time, not here.
    #[cfg(windows)]
    let accessibility: Option<Box<dyn glass_core::Accessibility + Send>> =
        Some(Box::new(glass_a11y_windows::WindowsA11y::new()));
    #[cfg(target_os = "linux")]
    let accessibility: Option<Box<dyn glass_core::Accessibility + Send>> =
        Some(Box::new(glass_a11y_linux::LinuxA11y::new()));
    #[cfg(target_os = "macos")]
    let accessibility: Option<Box<dyn glass_core::Accessibility + Send>> =
        Some(Box::new(glass_a11y_macos::MacosA11y::new()));
    Ok(Backend {
        platform,
        accessibility,
    })
}

/// Default backend name from `GLASS_BACKEND` (case-insensitive
/// `wayland`/`windows`/`macos`/`x11`/`android`). Unset defaults to the windows backend on
/// a Windows host, the macos backend on a macOS host, else X11.
pub fn default_backend(env: Option<&str>) -> &'static str {
    match env {
        Some(v) if v.eq_ignore_ascii_case("android") => "android",
        Some(v) if v.eq_ignore_ascii_case("wayland") => "wayland",
        Some(v) if v.eq_ignore_ascii_case("windows") => "windows",
        Some(v) if v.eq_ignore_ascii_case("macos") => "macos",
        Some(v) if v.eq_ignore_ascii_case("x11") => "x11",
        None if cfg!(windows) => "windows",
        None if cfg!(target_os = "macos") => "macos",
        _ => "x11",
    }
}

/// `glass-mcp env [--json]`: print glass's configuration env vars (secrets redacted).
pub fn run_env(json: bool) -> ! {
    let current = |name: &str| env::current_from_env(name);
    let out = if json {
        env::render_json(&current)
    } else {
        env::render_text(&current)
    };
    print!("{out}");
    std::process::exit(0);
}

/// `glass-mcp status [--addr ADDR]`: report whether a glass server is running and its
/// endpoint. A thin `pub` forwarder to [`status::run`], the same shape as [`run_env`]/
/// [`run_doctor`] over their own private-to-this-crate modules: `status` (like `env`) is a
/// CLI-only concern with no library/integration-test consumer, so it stays `pub(crate)`
/// and only this wrapper is public — `main.rs` is a separate crate from this library, so it
/// can't name a `pub(crate)` item directly.
pub fn run_status(addr: Option<&str>) -> anyhow::Result<()> {
    status::run(addr)
}

/// Spike/diagnostic (`debug-grants`): poll the two TCC grants once a second in one long-lived
/// process, so you can watch which flips live when granted in System Settings (Accessibility)
/// vs. which stays stale until the process relaunches (Screen Recording — `CGPreflightScreen
/// CaptureAccess` is a launch-time snapshot). Confirms the mechanics the onboarding flow relies on.
#[cfg(target_os = "macos")]
pub fn run_debug_grants() -> anyhow::Result<()> {
    use std::io::Write as _;
    println!("watching TCC grants once a second (Ctrl-C to stop).");
    println!("grant each in System Settings > Privacy & Security and watch which flips here:");
    loop {
        let ax = glass_macos::accessibility_granted();
        let sr = glass_macos::screen_recording_granted();
        println!("accessibility={ax}  screen_recording={sr}");
        std::io::stdout().flush().ok();
        std::thread::sleep(std::time::Duration::from_secs(1));
    }
}

/// Non-macOS: no TCC to poll.
#[cfg(not(target_os = "macos"))]
pub fn run_debug_grants() -> anyhow::Result<()> {
    anyhow::bail!("debug-grants is macOS-only")
}

/// Run the `doctor` subcommand and exit.
pub fn run_doctor(deep: bool, json: bool, audit_log: Option<&str>) -> ! {
    let backend = default_backend(std::env::var("GLASS_BACKEND").ok().as_deref());
    let report = audit::report_from_config(audit_log, |k| std::env::var(k).ok());
    let diag = doctor::diagnose_with_audit(deep, &report);
    if json {
        println!(
            "{}",
            serde_json::to_string_pretty(&diag).expect("serialize diagnosis")
        );
    } else {
        print!("{}", diag.render_text(backend));
    }
    std::process::exit(diag.exit_code(backend));
}

/// Build the `Glass` session manager, installing the audit sink if one is configured.
pub fn boot(audit: Option<Box<dyn glass_core::AuditSink>>) -> Glass {
    let default = default_backend(std::env::var("GLASS_BACKEND").ok().as_deref()).to_string();
    let baselines = BaselineStore::new(".glass/baselines");
    let registry = glass_android::EmulatorRegistry::new();
    let agents = glass_android::AgentRegistry::new();
    let a11y = glass_android::A11yServiceRegistry::new();
    let reg_factory = registry.clone();
    let agents_factory = agents.clone();
    let a11y_factory = a11y.clone();
    let mut glass = Glass::new(
        Box::new(move |b| make_platform(b, &reg_factory, &agents_factory, &a11y_factory)),
        default,
        baselines,
        10_000,
    );
    glass.set_shutdown_hook(Box::new(move || {
        a11y.shutdown();
        agents.shutdown();
        registry.kill_all();
    }));
    if let Some(sink) = audit {
        glass.set_audit_sink(sink);
    }
    glass
}

/// Serve MCP over stdio (the default transport) and tear down on EOF or signal.
pub async fn run_stdio(glass: Glass, report: crate::audit::AuditReport) -> anyhow::Result<()> {
    let server = GlassServer::new(glass, report);
    let sessions = server.sessions();
    let service = server
        .serve(stdio())
        .await
        .context("starting the MCP stdio service")?;

    let via_signal = tokio::select! {
        r = service.waiting() => { r.context("serving MCP")?; false }
        _ = shutdown::shutdown_signal() => {
            eprintln!("glass: received shutdown signal; tearing down sessions");
            true
        }
    };
    shutdown::run_shutdown(sessions, Duration::from_secs(3)).await;
    if via_signal {
        std::process::exit(0);
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::default_backend;

    #[test]
    fn android_backend_is_selectable_by_name() {
        assert_eq!(super::default_backend(Some("android")), "android");
        assert_eq!(super::default_backend(Some("ANDROID")), "android");
    }

    #[test]
    fn defaults_to_x11_unless_wayland() {
        assert_eq!(default_backend(Some("wayland")), "wayland");
        assert_eq!(default_backend(Some("WAYLAND")), "wayland");
        assert_eq!(default_backend(Some("windows")), "windows");
        assert_eq!(default_backend(Some("WINDOWS")), "windows");
        assert_eq!(default_backend(Some("macos")), "macos");
        assert_eq!(default_backend(Some("MACOS")), "macos");
        assert_eq!(default_backend(Some("x11")), "x11");
        assert_eq!(default_backend(Some("nonsense")), "x11");
        #[cfg(windows)]
        assert_eq!(default_backend(None), "windows");
        #[cfg(target_os = "macos")]
        assert_eq!(default_backend(None), "macos");
        #[cfg(not(any(windows, target_os = "macos")))]
        assert_eq!(default_backend(None), "x11");
    }
}
