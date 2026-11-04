//! Resident Windows desktop preparation and its native tray surface.

use super::files::{Root, base_path};
use super::native::{Manager, lock_named};
use super::runtime::{self, Launch};
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
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct Request {
    version: u32,
    instance: String,
    sequence: u64,
    action: Action,
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
        println!("Glass is already running. Open it from the system tray.");
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
            println!("Glass is open. Choose Start development desktop before agent work.");
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
pub async fn run(cfg: crate::serve::config::ServeConfig, audit: Option<&str>) -> Result<()> {
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
        manager.clone(),
        server,
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
    manager: Arc<Manager>,
    server: crate::server::GlassServer,
) -> tokio::task::JoinHandle<Result<()>> {
    tokio::spawn(async move {
        match action {
            Action::Start => {
                server.stop_from_desktop_ui().await?;
                tokio::task::spawn_blocking(move || manager.prepare()).await?
            }
            Action::Stop => {
                server.stop_from_desktop_ui().await?;
                tokio::task::spawn_blocking(move || manager.shutdown(Deadline::from_millis(30_000)))
                    .await?
            }
            Action::Show | Action::Hide => {
                tokio::task::spawn_blocking(move || manager.viewer(action == Action::Show)).await?
            }
            Action::Quit => anyhow::bail!("Quit must finish owned cleanup first"),
        }
    })
}

async fn control(
    root: &Root,
    launch: &Launch,
    endpoint: &str,
    manager: Arc<Manager>,
    server: crate::server::GlassServer,
    helper: &mut std::process::Child,
    serving: &tokio::task::JoinHandle<Result<()>>,
) -> Result<()> {
    let mut acknowledged = 0;
    let mut phase = "Not ready".to_owned();
    let mut detail = "Start the development desktop before agent work. Windows may request administrator consent and local sign-in.".to_owned();
    let mut pending: Option<(Action, tokio::task::JoinHandle<Result<()>>)> = None;
    let mut quitting = false;
    let mut ui_gone = false;
    let mut stop_queued = false;
    let mut server_failed = false;
    let mut interval = tokio::time::interval(Duration::from_millis(200));
    let outcome: Result<()> = async {
        loop {
            interval.tick().await;
            if !server_failed && serving.is_finished() {
                server_failed = true;
                manager.cancel_preparation();
                stop_queued = !matches!(pending, Some((Action::Stop, _)));
            }
            if !ui_gone && helper.try_wait()?.is_some() {
                ui_gone = true;
                quitting = true;
                manager.cancel_preparation();
                stop_queued = !matches!(pending, Some((Action::Stop, _)));
            }
            if pending.as_ref().is_some_and(|(_, task)| task.is_finished()) {
                let (action, task) = pending.take().context("pending desktop action")?;
                let result = task.await.context("desktop action task failed")?;
                if action == Action::Stop && quitting {
                    if result.is_ok() { break; }
                    if ui_gone { return result; }
                    quitting = false;
                }
                match result {
                    Ok(()) => {
                        if manager.ready()? {
                            phase = "Ready".into();
                            detail = "The development desktop is ready for agent work.".into();
                        } else {
                            phase = "Not ready".into();
                            detail = "Start the development desktop before agent work.".into();
                        }
                    }
                    Err(error) => {
                        phase = "Needs attention".into();
                        detail = format!("{error:#}");
                    }
                }
            }
            if let Some(request) = root.read::<Request>("request.json")?
                && request.sequence != acknowledged
            {
                validate_request(&request, &launch.instance, acknowledged)?;
                acknowledged = request.sequence;
                match request.action {
                    Action::Stop | Action::Quit => {
                        quitting |= request.action == Action::Quit;
                        manager.cancel_preparation();
                        stop_queued = !matches!(pending, Some((Action::Stop, _)));
                        detail = "Stopping the desktop. Complete or cancel any Windows dialog.".into();
                    }
                    Action::Start if pending.is_none() && !quitting && !server_failed => {
                        if !manager.ready().unwrap_or(false) {
                            manager.begin_preparation();
                            phase = "Starting".into();
                            detail = "Complete any Windows administrator consent and sign-in dialogs locally. You can cancel from Windows or Glass.".into();
                            pending = Some((Action::Start, action_task(Action::Start, manager.clone(), server.clone())));
                        }
                    }
                    Action::Show | Action::Hide if pending.is_none() && !quitting => {
                        pending = Some((request.action, action_task(request.action, manager.clone(), server.clone())));
                    }
                    _ => {}
                }
            }
            if stop_queued && pending.is_none() {
                stop_queued = false;
                phase = "Stopping".into();
                detail = "Stopping agent work and the owned desktop.".into();
                pending = Some((Action::Stop, action_task(Action::Stop, manager.clone(), server.clone())));
            }
            let ready = manager.ready();
            if pending.is_none() && phase == "Ready" && !matches!(ready, Ok(true)) {
                manager.close_admission();
                phase = "Needs attention".into();
                detail = ready.err().map(|e| format!("{e:#}")).unwrap_or_else(|| "The desktop ended. Choose Start development desktop to prepare it again.".into());
            }
            let displayed_phase = if matches!(pending, Some((Action::Start, _))) {
                preparation_phase().unwrap_or_else(|| phase.clone())
            } else { phase.clone() };
            if server_failed {
                phase = "Needs attention".into();
                detail = "The MCP server stopped. Quit Glass and open it again before starting work.".into();
            }
            root.write("status.json", &Status {
                version: 1, instance: &launch.instance, acknowledged, phase: if server_failed { "Needs attention" } else { &displayed_phase },
                detail: &detail, endpoint, busy: pending.is_some(), ready: manager.ready().unwrap_or(false), can_start: !server_failed && !quitting, closing: false,
            })?;
        }
        Ok(())
    }.await;
    manager.cancel_preparation();
    if let Some((_, task)) = pending {
        let cleanup = task
            .await
            .context("finish desktop action before resident shutdown")?;
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
    fn tray_commands_refuse_replay_wrong_launch_and_skipped_sequences() {
        let mut request = Request {
            version: 1,
            instance: "current".into(),
            sequence: 1,
            action: Action::Start,
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
