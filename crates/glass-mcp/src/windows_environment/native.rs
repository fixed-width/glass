use super::files::{Root, base_path};
use super::runtime::{self, Runtime};
use super::{Configuration, DesktopCommand, DesktopMode, Selection};
use anyhow::{Context, Result, ensure};
use glass_core::{Backend, Deadline, GlassError};
use glass_windows::desktop::{ProcessIdentity, SettingChange};
use std::sync::{Arc, Mutex, OnceLock};

static REQUESTED: OnceLock<Option<DesktopMode>> = OnceLock::new();

pub fn set_requested_mode(mode: Option<DesktopMode>) -> Result<()> {
    REQUESTED
        .set(mode)
        .map_err(|_| anyhow::anyhow!("Windows desktop mode was already selected"))
}

pub(super) fn configuration() -> Result<Configuration> {
    let Some(root) = Root::open(&base_path()?, false)? else {
        return Ok(Configuration::default());
    };
    let value = root
        .read::<Configuration>("configuration.json")?
        .unwrap_or_default();
    value.validate().map_err(anyhow::Error::msg)?;
    Ok(value)
}

pub(crate) struct Manager {
    selection: Mutex<Selection>,
    runtime: Mutex<Option<Arc<Runtime>>>,
}

impl Manager {
    pub fn new() -> Self {
        Self {
            selection: Mutex::new(Selection::new(REQUESTED.get().copied().flatten())),
            runtime: Mutex::new(None),
        }
    }

    pub fn backend(&self) -> glass_core::Result<Backend> {
        self.backend_inner().map_err(|error| {
            GlassError::Backend(format!("Windows desktop: {error:#}")).before_dispatch()
        })
    }

    fn backend_inner(&self) -> Result<Backend> {
        if self
            .selection
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop selection is poisoned"))?
            .preference
            == Some(DesktopMode::Shared)
        {
            return Ok(Backend {
                platform: Box::new(glass_windows::WindowsPlatform::new()?),
                accessibility: Some(Box::new(glass_a11y_windows::WindowsA11y::new())),
            });
        }
        let configuration = configuration()?;
        let mode = self
            .selection
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop selection is poisoned"))?
            .select(&configuration)
            .map_err(anyhow::Error::msg)?;
        if mode == DesktopMode::Shared {
            return Ok(Backend {
                platform: Box::new(glass_windows::WindowsPlatform::new()?),
                accessibility: Some(Box::new(glass_a11y_windows::WindowsA11y::new())),
            });
        }
        ensure!(
            glass_windows::desktop::child_sessions_enabled()?,
            "child sessions are disabled; run glass-mcp desktop enable from your Windows desktop and complete setup first"
        );
        let mut current = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?;
        if current.is_none() {
            let base = Root::open(&base_path()?, true)?
                .context("private Windows desktop state directory")?;
            *current = Some(Arc::new(Runtime::start(base, configuration.revision)?));
        }
        let owner = current
            .as_ref()
            .context("Windows desktop owner is unavailable")?;
        crate::remote_backend::backend(owner.acquire()?).map_err(Into::into)
    }

    pub fn shutdown(&self, deadline: Deadline) -> Result<()> {
        let current = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?
            .clone();
        if let Some(owner) = current {
            owner.shutdown(deadline)?;
        }
        Ok(())
    }
}

fn base() -> Result<Root> {
    Root::open(&base_path()?, true)?.context("private desktop state directory")
}

fn lock_configuration(base: &Root) -> Result<std::fs::File> {
    let name = std::ffi::OsStr::new("configuration.lock");
    let file = if base
        .names()?
        .iter()
        .any(|name| name.eq_ignore_ascii_case("configuration.lock"))
    {
        glass_windows::open_file_child_readonly(&base.directory, name)
            .map_err(|_| anyhow::anyhow!("configuration lock changed identity"))?
    } else {
        glass_windows::create_private_child(&base.directory, name, false)?
    };
    ensure!(
        glass_windows::file_is_private_to_current_user(&file)?,
        "desktop configuration lock is not private"
    );
    fs4::FileExt::try_lock(&file)
        .context("another Windows desktop setup operation is in progress")?;
    Ok(file)
}

pub fn command(command: DesktopCommand) -> Result<()> {
    if let DesktopCommand::Status { json } = command {
        return status(json);
    }
    let current = ProcessIdentity::current()?;
    ensure!(
        current.session != 0,
        "run this desktop action from your signed-in Windows desktop"
    );
    let base = base()?;
    let _configuration_lock = lock_configuration(&base)?;
    match command {
        DesktopCommand::Enable => {
            runtime::require_payload()?;
            glass_windows::desktop::rdp_control_available()?;
            let mut value = base
                .read::<Configuration>("configuration.json")?
                .unwrap_or_default();
            value.validate().map_err(anyhow::Error::msg)?;
            let before = glass_windows::desktop::child_sessions_enabled()?;
            if value.enabled && !value.setup_pending && before {
                println!(
                    "Separate Windows desktop is enabled. Restart the MCP server to pick up a changed registration."
                );
                return Ok(());
            }
            if value.setting_before.is_none() {
                value.setting_before = Some(before);
            }
            value.enabled = false;
            value.setup_pending = true;
            value.revision = format!("{:032x}", rand::random::<u128>());
            if !before {
                value.setting_before = Some(false);
                value.owns_setting = true;
            }
            base.write("configuration.json", &value)?;
            if !before {
                eprintln!(
                    "glass: complete Windows administrator consent to enable local child sessions; no network Remote Desktop setting is changed"
                );
                let changed = glass_windows::desktop::change_setting(
                    &current.image,
                    true,
                    Deadline::from_millis(120_000),
                )?;
                if changed == SettingChange::AlreadySet && value.setting_before == Some(false) {
                    value.owns_setting = false;
                }
            }
            ensure!(
                glass_windows::desktop::child_sessions_enabled()?,
                "Windows child-session setup did not read back as enabled"
            );
            value.setup_pending = false;
            value.enabled = true;
            base.write("configuration.json", &value)?;
            println!(
                "Separate Windows desktop enabled for this account. Restart the MCP server. Windows sign-in may be requested at the first application launch."
            );
        }
        DesktopCommand::Disable => {
            let mut value = base
                .read::<Configuration>("configuration.json")?
                .unwrap_or_default();
            value.validate().map_err(anyhow::Error::msg)?;
            value.enabled = false;
            value.setup_pending = true;
            value.revision = format!("{:032x}", rand::random::<u128>());
            base.write("configuration.json", &value)?;
            stop_all(&base, &current, false)?;
            if value.owns_setting && glass_windows::desktop::child_sessions_enabled()? {
                eprintln!(
                    "glass: complete Windows administrator consent to restore the child-session setting"
                );
                glass_windows::desktop::change_setting(
                    &current.image,
                    false,
                    Deadline::from_millis(120_000),
                )?;
            }
            value.setup_pending = false;
            value.owns_setting = false;
            base.write("configuration.json", &value)?;
            println!(
                "Separate Windows desktop disabled. Restart existing MCP servers before using shared desktop input. Application files and persistent sandbox data were retained."
            );
        }
        DesktopCommand::Stop => {
            stop_all(&base, &current, false)?;
            println!("Owned Windows desktop stopped.");
        }
        DesktopCommand::Recover => {
            stop_all(&base, &current, true)?;
            println!("Windows desktop cleanup complete.");
        }
        DesktopCommand::Show | DesktopCommand::Hide => {
            let mut found = false;
            for (root, journal) in runtime::journals(&base)? {
                if journal.clean || !journal.owner.is_current()? {
                    continue;
                }
                runtime::validate_journal(&journal, &current)?;
                ensure!(
                    journal.parent.session == current.session
                        && journal.parent.logon == current.logon,
                    "open the viewer from its owning Windows logon"
                );
                ensure!(
                    journal.phase == "ready",
                    "the separate desktop is not ready"
                );
                runtime::control(&root, &journal, matches!(command, DesktopCommand::Show))?;
                found = true;
            }
            ensure!(found, "there is no running separate desktop in this logon");
        }
        DesktopCommand::Status { .. } => {
            unreachable!("handled before acquiring configuration lock")
        }
    }
    Ok(())
}

fn stop_all(base: &Root, current: &ProcessIdentity, recovery_only: bool) -> Result<()> {
    for (root, journal) in runtime::journals(base)? {
        runtime::validate_journal(&journal, current)?;
        if journal.clean {
            continue;
        }
        if journal.owner.is_current()? {
            ensure!(
                !recovery_only,
                "the recorded desktop owner is still running; use desktop stop"
            );
            ensure!(
                journal.parent.session == current.session && journal.parent.logon == current.logon,
                "stop this desktop from its owning Windows logon"
            );
            runtime::stop_external(&root, &journal, Deadline::from_millis(30_000))?;
        } else {
            runtime::recover(base, &root, &journal, Deadline::from_millis(30_000))?;
        }
    }
    runtime::archive_finished(base, current)?;
    ensure!(
        runtime::pending_starts(base, current)?.is_empty(),
        "desktop startup is still pending; close its MCP server and run desktop recover"
    );
    Ok(())
}

fn status(json: bool) -> Result<()> {
    let current = ProcessIdentity::current()?;
    let value = configuration()?;
    let mut environments = Vec::new();
    if let Some(base) = Root::open(&base_path()?, false)? {
        for (_, journal) in runtime::journals(&base)? {
            runtime::validate_journal(&journal, &current)?;
            let alive = journal.owner.is_current()?;
            let phase = if !journal.clean && !alive {
                "recovery_required"
            } else if journal.phase == "ready"
                && !journal
                    .worker
                    .as_ref()
                    .map(|worker| worker.is_current())
                    .transpose()?
                    .unwrap_or(false)
            {
                "worker_unavailable"
            } else {
                journal.phase.as_str()
            };
            environments.push(serde_json::json!({"phase":phase,"parent_session":journal.parent.session,"child_session":journal.child,"owner_alive":alive,"clean":journal.clean,"viewer_visible":journal.viewer_visible,"error":journal.error}));
        }
        environments.extend(runtime::pending_starts(&base, &current)?);
    }
    let setting =
        glass_windows::desktop::child_sessions_enabled().map_err(|error| error.to_string());
    let control =
        glass_windows::desktop::rdp_control_available().map_err(|error| error.to_string());
    let report = serde_json::json!({"configured":value.enabled,"setup_pending":value.setup_pending,"setting_before":value.setting_before,"owns_setting":value.owns_setting,"parent_session":current.session,"interactive_parent":current.session!=0,"helper_packaged":runtime::require_payload().is_ok(),"child_sessions_enabled":setting,"rdp_control":control,"environments":environments});
    if json {
        println!("{}", serde_json::to_string_pretty(&report)?);
    } else {
        println!(
            "Saved Windows desktop selection: {}",
            if value.setup_pending {
                "setup incomplete"
            } else if value.enabled {
                "enabled"
            } else {
                "disabled"
            }
        );
        if current.session == 0 {
            println!("Start it from a signed-in Windows desktop.");
        }
        for environment in report["environments"]
            .as_array()
            .context("desktop status environments")?
        {
            println!(
                "Environment: {}",
                environment["phase"].as_str().unwrap_or("unknown")
            );
        }
        if let Err(error) = setting {
            eprintln!("Child-session query: {error}");
        }
        if let Err(error) = control {
            eprintln!("RDP control: {error}");
        }
        if let Err(error) = runtime::require_payload() {
            eprintln!("{error}");
        }
        println!(
            "This desktop uses your Windows account. Application containment is configured separately."
        );
    }
    Ok(())
}
