//! Resident Windows desktop preparation and its native tray surface.

use super::files::{Root, base_path};
use super::native::{Manager, lock_named};
use super::runtime::{self, Launch};
use super::{DesktopMode, startup};
use anyhow::{Context, Result, ensure};
use glass_core::Deadline;
use glass_windows::desktop::ProcessIdentity;
use serde::{Deserialize, Serialize};
use std::os::windows::process::CommandExt;
use std::process::{Command, Stdio};
use std::sync::Arc;
use std::time::Duration;

#[derive(Clone, Copy, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "snake_case")]
enum Action {
    Start,
    Stop,
    Show,
    Hide,
    Quit,
    Configure,
    SaveStartup,
    Refresh,
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    instance: String,
    sequence: u64,
    action: Action,
    mode: Option<DesktopMode>,
    start_with_windows: Option<bool>,
}

#[derive(Serialize)]
struct Status<'a> {
    version: u32,
    instance: &'a str,
    acknowledged: u64,
    phase: &'a str,
    detail: &'a str,
    endpoint: &'a str,
    busy: bool,
    ready: bool,
    can_start: bool,
    closing: bool,
    mode: Option<DesktopMode>,
    start_with_windows: bool,
    preparing: bool,
    separate_available: bool,
    separate_detail: Option<&'a str>,
    startup_error: Option<&'a str>,
    viewer_visible: Option<bool>,
    viewer_error: Option<&'a str>,
}

fn validate_request(request: &Request, instance: &str, acknowledged: u64) -> Result<()> {
    ensure!(
        request.version == 1
            && request.instance == instance
            && request.sequence
                == acknowledged
                    .checked_add(1)
                    .context("tray request sequence exhausted")?,
        "Tray request does not match this launch or sequence"
    );
    Ok(())
}

/// Open the resident app without retaining a terminal or an MCP stdio connection.
pub fn open() -> Result<()> {
    let base = Root::open(&base_path()?, true)?.context("private Windows desktop directory")?;
    if let Some(launch) = base.read::<Launch>("tray.json")?
        && launch.parent.is_current()?
    {
        let current = ProcessIdentity::current()?;
        ensure!(
            launch.parent.session == current.session && launch.parent.logon == current.logon,
            "Glass is running in a different Windows logon"
        );
        ensure!(
            launch.instance.len() == 32 && launch.instance.bytes().all(|b| b.is_ascii_hexdigit()),
            "Glass launch identity changed"
        );
        let tray = base
            .child(&format!("tray-{}", launch.instance), false)?
            .context("Glass application setup is unavailable")?;
        tray.write("open.json", &serde_json::json!({"version":1,"instance":launch.instance,"request":format!("{:032x}",rand::random::<u128>())}))?;
        println!("Glass is already running. Opening Application setup.");
        return Ok(());
    }
    let executable = std::env::current_exe()?;
    let log_name = format!("tray-launch-{:032x}.log", rand::random::<u128>());
    let log = glass_windows::create_private_child(
        &base.directory,
        std::ffi::OsStr::new(&log_name),
        false,
    )?;
    glass_windows::desktop::protect_stdio_inheritance()?;
    let mut child = Command::new(executable)
        .args(["serve", "--tray"])
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::from(log.try_clone()?))
        .stderr(Stdio::from(log))
        .spawn()
        .context("open Glass tray app")?;
    let identity = ProcessIdentity::read(child.id())?;
    let deadline = std::time::Instant::now() + Duration::from_secs(15);
    loop {
        if let Some(launch) = base.read::<Launch>("tray.json")?
            && launch.parent == identity
            && launch
                .helper
                .as_ref()
                .is_some_and(|p| p.is_current().unwrap_or(false))
        {
            println!("Glass is open. Complete Application setup before agent work.");
            break;
        }
        if child.try_wait()?.is_some() || std::time::Instant::now() >= deadline {
            let detail = base.bytes(&log_name, 64 * 1024)?.unwrap_or_default();
            anyhow::bail!(
                "Glass could not open its tray app: {}. Inspect {}",
                String::from_utf8_lossy(&detail),
                base.path.join(log_name).display()
            );
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    Ok(())
}

/// Host one desktop and HTTP server independently of MCP client lifetimes.
pub async fn run(
    cfg: crate::serve::config::ServeConfig,
    audit: Option<&str>,
    startup_launch: bool,
    token_file: Option<std::path::PathBuf>,
) -> Result<()> {
    crate::serve::check_exposure(&cfg)?;
    let manager = Arc::new(Manager::resident()?);
    let base = Root::open(&base_path()?, true)?.context("private Windows desktop directory")?;
    let _lock =
        lock_named(&base, "tray.lock").context("Glass is already running in the system tray")?;
    let parent = ProcessIdentity::current()?;
    ensure!(
        parent.session != 0,
        "Open Glass from your signed-in Windows desktop"
    );
    let listener = tokio::net::TcpListener::bind(cfg.addr)
        .await
        .context("bind Glass MCP endpoint")?;
    let endpoint = format!("http://{}/", listener.local_addr()?);
    let startup_settings = startup::Settings {
        addr: cfg.addr,
        token_file,
        needs_token: cfg.token.is_some(),
    };
    let instance = format!("{:032x}", rand::random::<u128>());
    let root = base
        .child(&format!("tray-{instance}"), true)?
        .context("private tray directory")?;
    let mut launch = Launch {
        version: 1,
        instance: instance.clone(),
        pipe: String::new(),
        parent: parent.clone(),
        helper: None,
        worker_exe: parent.image.clone(),
    };
    root.write("intent.json", &launch)?;
    root.write("options.json", &serde_json::json!({"version":1,"instance":instance,"hidden":startup_launch && manager.mode()?.is_some()}))?;
    let (sink, report) = crate::audit::resolve(audit, |key| std::env::var(key).ok())?;
    let server = crate::server::GlassServer::new_configured(
        crate::boot_with_desktop(sink, manager.clone()),
        report,
        cfg.tool_profile,
        cfg.trace.as_ref(),
        "http",
    )?;
    glass_windows::desktop::protect_stdio_inheritance()?;
    let mut helper = Command::new(runtime::helper(&base)?)
        .arg(&root.path)
        .arg("tray")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start native Glass tray")?;
    launch.helper = Some(ProcessIdentity::read(helper.id())?);
    root.write("launch.json", &launch)?;
    base.write("tray.json", &launch)?;
    let shutdown = tokio_util::sync::CancellationToken::new();
    let stop = shutdown.clone();
    let http_server = server.clone();
    let serving = tokio::spawn(async move {
        crate::serve::run_server_on_until(listener, cfg, http_server, async move {
            stop.cancelled().await
        })
        .await
    });
    let outcome = control(
        &root,
        &launch,
        &endpoint,
        Controls {
            manager: manager.clone(),
            server,
            settings: startup_settings,
        },
        &mut helper,
        &serving,
    )
    .await;
    manager.cancel_preparation();
    shutdown.cancel();
    let stopped = serving.await.context("Glass HTTP server task stopped")?;
    root.write(
        "status.json",
        &Status {
            version: 1,
            instance: &instance,
            acknowledged: 0,
            phase: if stopped.is_ok() {
                "Stopped"
            } else {
                "Needs attention"
            },
            detail: "Glass has stopped.",
            endpoint: &endpoint,
            busy: false,
            ready: false,
            can_start: false,
            closing: true,
            mode: manager.mode()?,
            start_with_windows: false,
            preparing: false,
            separate_available: false,
            separate_detail: None,
            startup_error: None,
            viewer_visible: None,
            viewer_error: None,
        },
    )?;
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    while helper.try_wait()?.is_none() && std::time::Instant::now() < deadline {
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    ensure!(
        helper.try_wait()?.is_some(),
        "Glass tray did not finish closing; its ownership record was retained"
    );
    outcome?;
    stopped
}

fn action_task(
    action: Action,
    mode: Option<DesktopMode>,
    wanted_startup: Option<bool>,
    manager: Arc<Manager>,
    server: crate::server::GlassServer,
    settings: startup::Settings,
) -> tokio::task::JoinHandle<Result<Option<bool>>> {
    tokio::spawn(async move {
        match action {
            Action::Start => {
                server.stop_from_desktop_ui().await?;
                tokio::task::spawn_blocking(move || manager.prepare()).await??;
            }
            Action::Stop => {
                server.stop_from_desktop_ui().await?;
                tokio::task::spawn_blocking(move || {
                    manager.shutdown(Deadline::from_millis(30_000))
                })
                .await??;
            }
            Action::Configure => {
                let mode = mode.context("Choose where applications run")?;
                if mode == DesktopMode::Separate {
                    tokio::task::spawn_blocking(glass_windows::desktop::child_desktop_available)
                        .await??;
                }
                if manager.mode()? != Some(mode) {
                    manager.close_admission();
                    server.stop_from_desktop_ui().await?;
                    let owner = manager.clone();
                    tokio::task::spawn_blocking(move || owner.choose_mode(mode)).await??;
                }
                let actual = match wanted_startup {
                    Some(wanted) => Some(
                        tokio::task::spawn_blocking(move || startup::configure(wanted, &settings))
                            .await??,
                    ),
                    None => None,
                };
                if mode == DesktopMode::Shared && !manager.ready()? {
                    manager.begin_preparation();
                    tokio::task::spawn_blocking(move || manager.prepare()).await??;
                }
                return Ok(actual);
            }
            Action::SaveStartup => {
                let wanted = wanted_startup.context("Choose the Start with Windows preference")?;
                return tokio::task::spawn_blocking(move || startup::configure(wanted, &settings))
                    .await?
                    .map(Some);
            }
            Action::Refresh => {
                return tokio::task::spawn_blocking(startup::enabled)
                    .await?
                    .map(Some);
            }
            Action::Show | Action::Hide => {
                tokio::task::spawn_blocking(move || manager.viewer(action == Action::Show))
                    .await??;
            }
            Action::Quit => anyhow::bail!("Quit must finish owned cleanup first"),
        }
        Ok(None)
    })
}

struct Controls {
    manager: Arc<Manager>,
    server: crate::server::GlassServer,
    settings: startup::Settings,
}

async fn separate_desktop_error() -> Option<String> {
    match tokio::task::spawn_blocking(glass_windows::desktop::child_desktop_available).await {
        Ok(Ok(())) => None,
        Ok(Err(error)) => Some(error.to_string()),
        Err(error) => Some(format!(
            "Off-desktop availability could not be checked: {error}"
        )),
    }
}

async fn control(
    root: &Root,
    launch: &Launch,
    endpoint: &str,
    controls: Controls,
    helper: &mut std::process::Child,
    serving: &tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let Controls {
        manager,
        server,
        settings,
    } = controls;
    let mut acknowledged = 0;
    let mut separate_error = separate_desktop_error().await;
    let mut phase = "Not ready".to_owned();
    let mut detail =
        "Open Application setup to choose On your desktop or Off your desktop.".to_owned();
    let startup_state = tokio::task::spawn_blocking(startup::enabled).await?;
    let mut startup_error = startup_state
        .as_ref()
        .err()
        .map(|error| format!("{error:#}"));
    let mut start_with_windows = match startup_state {
        Ok(value) => value,
        Err(error) => {
            phase = "Needs attention".into();
            detail = format!("{error:#}");
            false
        }
    };
    let mut pending: Option<(Action, tokio::task::JoinHandle<Result<Option<bool>>>)> = None;
    if manager.mode()? == Some(DesktopMode::Shared) {
        manager.begin_preparation();
        phase = "Starting".into();
        detail = "Checking your desktop.".into();
        pending = Some((
            Action::Start,
            action_task(
                Action::Start,
                None,
                None,
                manager.clone(),
                server.clone(),
                settings.clone(),
            ),
        ));
    } else if manager.mode()? == Some(DesktopMode::Separate) {
        detail = separate_error.clone().unwrap_or_else(|| {
            "Choose Start Environment… and complete any Windows sign-in before agent work.".into()
        });
    }
    let mut quitting = false;
    let mut ui_gone = false;
    let mut stop_queued = false;
    let mut server_failed = false;
    let mut interval = tokio::time::interval(Duration::from_millis(200));
    let outcome: Result<()> = async {
        loop {
            interval.tick().await;
            if !server_failed && serving.is_finished() {
                server_failed=true;manager.cancel_preparation();
                stop_queued = !matches!(pending,Some((Action::Stop,_)));
            }
            if !ui_gone && helper.try_wait()?.is_some() {
                ui_gone=true;quitting=true;manager.cancel_preparation();
                stop_queued = !matches!(pending,Some((Action::Stop,_)));
            }
            if pending.as_ref().is_some_and(|(_,task)|task.is_finished()) {
                let (action,task)=pending.take().context("pending application setup operation")?;
                let result=task.await.context("application setup operation failed")?;
                if matches!(action,Action::SaveStartup|Action::Refresh) {
                    startup_error=result.as_ref().err().map(|error|format!("{error:#}"));
                }
                if action==Action::Refresh {separate_error=separate_desktop_error().await;}
                if action==Action::Stop && quitting {
                    if result.is_ok(){break;}
                    if ui_gone{return result.map(|_|());}
                    quitting=false;
                }
                match result {
                    Ok(preference)=>{
                        if let Some(value)=preference{start_with_windows=value;}
                        if manager.ready()? {
                            phase="Ready".into();detail="Glass is ready for application work.".into();
                        } else {
                            phase="Not ready".into();detail="Open Application setup to finish setup or choose Start Environment….".into();
                        }
                    }
                    Err(error)=>{phase="Needs attention".into();detail=format!("{error:#}");}
                }
            }
            if let Some(request)=root.read::<Request>("request.json")? && request.sequence!=acknowledged {
                validate_request(&request,&launch.instance,acknowledged)?;
                acknowledged=request.sequence;
                match request.action {
                    Action::Stop|Action::Quit=>{
                        quitting|=request.action==Action::Quit;manager.cancel_preparation();
                        stop_queued = !matches!(pending,Some((Action::Stop,_)));
                        detail="Stopping owned work. Complete or cancel any Windows dialog.".into();
                    }
                    Action::Start if pending.is_none() && !quitting && !server_failed=>{
                        if !manager.ready().unwrap_or(false) {
                            manager.begin_preparation();phase="Starting".into();
                            detail="Complete any Windows administrator consent and sign-in locally.".into();
                            pending=Some((Action::Start,action_task(Action::Start,None,None,manager.clone(),server.clone(),settings.clone())));
                        }
                    }
                    Action::Configure|Action::SaveStartup|Action::Refresh|Action::Show|Action::Hide if pending.is_none() && !quitting && !server_failed=>{
                        if request.action==Action::Configure {phase="Checking setup".into();detail="Saving Application setup. Complete any Windows consent locally.".into();}
                        pending=Some((request.action,action_task(request.action,request.mode,request.start_with_windows,manager.clone(),server.clone(),settings.clone())));
                    }
                    _=>{}
                }
            }
            if stop_queued && pending.is_none() {
                stop_queued=false;phase="Stopping".into();detail="Stopping owned application work and the environment.".into();
                pending=Some((Action::Stop,action_task(Action::Stop,None,None,manager.clone(),server.clone(),settings.clone())));
            }
            let ready=manager.ready();
            if pending.is_none() && phase=="Ready" && !matches!(ready,Ok(true)) {
                manager.close_admission();phase="Needs attention".into();
                detail=ready.err().map(|e|format!("{e:#}")).unwrap_or_else(||"The environment ended. Open Application setup to start it again.".into());
            }
            let preparing=matches!(pending,Some((Action::Start,_)));
            let displayed=if preparing {preparation_phase().unwrap_or_else(||phase.clone())}else{phase.clone()};
            if server_failed {phase="Needs attention".into();detail="The MCP server stopped. Quit Glass and open it again.".into();}
            let mode=manager.mode()?;
            let viewer=manager.viewer_visible();
            let viewer_error=viewer.as_ref().err().map(|error|format!("{error:#}"));
            root.write("status.json",&Status {
                version:1,instance:&launch.instance,acknowledged,phase:if server_failed{"Needs attention"}else{&displayed},
                detail:&detail,endpoint,busy:pending.is_some(),ready:manager.ready().unwrap_or(false),
                can_start:!server_failed && !quitting && (mode!=Some(DesktopMode::Separate) || separate_error.is_none()),closing:false,mode,start_with_windows,preparing,
                separate_available:separate_error.is_none(),separate_detail:separate_error.as_deref(),
                startup_error:startup_error.as_deref(),viewer_visible:viewer.ok().flatten(),viewer_error:viewer_error.as_deref(),
            })?;
        }
        Ok(())
    }.await;
    manager.cancel_preparation();
    if let Some((_, task)) = pending {
        let cleanup = task
            .await
            .context("finish application setup before resident shutdown")?;
        if outcome.is_ok() {
            cleanup?;
        }
    }
    outcome
}

fn preparation_phase() -> Option<String> {
    let base = Root::open(&base_path().ok()?, false).ok()??;
    let current = ProcessIdentity::current().ok()?;
    for (_, journal) in runtime::journals(&base).ok()? {
        if journal.parent == current
            && journal.owner.is_current().ok()?
            && journal.phase == "authentication_pending"
        {
            return Some("Waiting for sign-in".into());
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_setup_flow_covers_sequence_and_retained_configuration() {
        runtime::require_payload()
            .expect("native Windows helper is required for the setup model test");
        let directory = tempfile::tempdir().unwrap();
        let root = directory.path();
        std::fs::write(
            root.join("Host.cs"),
            include_str!("../../native/windows-desktop/Host.cs"),
        )
        .unwrap();
        std::fs::write(
            root.join("Tray.cs"),
            include_str!("../../native/windows-desktop/Tray.cs"),
        )
        .unwrap();
        std::fs::write(
            root.join("Startup.cs"),
            include_str!("../../native/windows-desktop/Startup.cs"),
        )
        .unwrap();
        std::fs::write(
            root.join("SetupFlow.cs"),
            include_str!("../../native/windows-desktop/tests/SetupFlow.cs"),
        )
        .unwrap();
        std::fs::write(root.join("GlassDesktopRdp.dll"), runtime::TEST_INTEROP).unwrap();
        std::fs::write(
            root.join("ChildOwnership.cs"),
            include_str!("../../native/windows-desktop/tests/ChildOwnership.cs"),
        )
        .unwrap();
        let compiler = std::path::PathBuf::from(std::env::var_os("SystemRoot").unwrap())
            .join("Microsoft.NET/Framework64/v4.0.30319/csc.exe");
        let compiled = Command::new(compiler)
            .current_dir(root)
            .args([
                "/nologo",
                "/codepage:65001",
                "/target:exe",
                "/platform:x64",
                "/main:SetupFlowTests",
                "/out:SetupFlowTests.exe",
                "/reference:GlassDesktopRdp.dll",
                "/reference:System.Windows.Forms.dll",
                "/reference:System.Drawing.dll",
                "/reference:System.Web.Extensions.dll",
                "/reference:Microsoft.CSharp.dll",
                "Host.cs",
                "Tray.cs",
                "Startup.cs",
                "SetupFlow.cs",
                "ChildOwnership.cs",
            ])
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            compiled.status.success(),
            "native setup model compilation failed: {} {}",
            String::from_utf8_lossy(&compiled.stdout),
            String::from_utf8_lossy(&compiled.stderr)
        );
        let result = Command::new(root.join("SetupFlowTests.exe"))
            .current_dir(root)
            .creation_flags(0x0800_0000)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "native setup model failed: {} {}",
            String::from_utf8_lossy(&result.stdout),
            String::from_utf8_lossy(&result.stderr)
        );
        let report: serde_json::Value = serde_json::from_slice(&result.stdout).unwrap();
        assert_eq!(report["count"], 19);
    }

    #[test]
    fn tray_commands_refuse_replay_wrong_launch_and_skipped_sequences() {
        let mut request = Request {
            version: 1,
            instance: "current".into(),
            sequence: 1,
            action: Action::Start,
            mode: None,
            start_with_windows: None,
        };
        assert!(validate_request(&request, "current", 0).is_ok());
        assert!(validate_request(&request, "other", 0).is_err());
        assert!(validate_request(&request, "current", 1).is_err());
        request.sequence = 3;
        assert!(validate_request(&request, "current", 1).is_err());
    }
    #[test]
    fn only_known_tray_actions_are_accepted() {
        assert!(
            serde_json::from_str::<Request>(
                r#"{"version":1,"instance":"x","sequence":1,"action":"enable_network_rdp"}"#
            )
            .is_err()
        );
    }
}
