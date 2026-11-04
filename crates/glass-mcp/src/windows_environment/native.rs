use super::files::{Root, base_path};
use super::runtime::{self, Runtime};
use super::{Configuration, DesktopCommand, DesktopMode, Selection};
use anyhow::{Context, Result, ensure};
use glass_core::{Backend, Deadline, GlassError};
use glass_windows::desktop::{ProcessIdentity, SettingChange};
#[cfg(feature = "network")]
use std::sync::atomic::AtomicBool;
use std::sync::atomic::{AtomicU8, Ordering};
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
    admitted: AtomicU8,
    #[cfg(feature = "network")]
    cancel: AtomicBool,
}

impl Manager {
    pub fn new() -> Self {
        Self {
            selection: Mutex::new(Selection::new(REQUESTED.get().copied().flatten())),
            runtime: Mutex::new(None),
            admitted: AtomicU8::new(0),
            #[cfg(feature = "network")]
            cancel: AtomicBool::new(false),
        }
    }

    pub fn backend(&self) -> glass_core::Result<Backend> {
        self.backend_inner()
            .map_err(|error| match error.downcast::<GlassError>() {
                Ok(error) => error.before_dispatch(),
                Err(error) => {
                    GlassError::Backend(format!("Windows desktop: {error:#}")).before_dispatch()
                }
            })
    }

    fn backend_inner(&self) -> Result<Backend> {
        if self
            .selection
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop selection is poisoned"))?
            .preference
            == Some(DesktopMode::Separate)
            && self.admitted.load(Ordering::Acquire) != 2
        {
            return Err(GlassError::EnvironmentNotReady.into());
        }
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
        if configuration.setup_pending {
            return Err(GlassError::EnvironmentNotReady.into());
        }
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
        if self.admitted.load(Ordering::Acquire) != 2
            || !glass_windows::desktop::child_sessions_enabled()?
        {
            return Err(GlassError::EnvironmentNotReady.into());
        }
        let current = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?;
        let owner = current
            .as_ref()
            .context("Windows desktop owner is unavailable")?;
        crate::remote_backend::backend(owner.acquire()?).map_err(Into::into)
    }

    pub fn shutdown(&self, deadline: Deadline) -> Result<()> {
        self.close_admission();
        let current = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?
            .clone();
        if let Some(owner) = current {
            owner.shutdown(deadline)?;
        }
        if let Some(base) = Root::open(&base_path()?, false)? {
            let current = ProcessIdentity::current()?;
            for (root, journal) in runtime::journals(&base)? {
                if journal.parent == current && !journal.clean {
                    if journal.owner.is_current()? {
                        runtime::stop_external(&root, &journal, deadline)?;
                    } else {
                        runtime::recover(&base, &root, &journal, deadline)?;
                    }
                }
            }
            for name in base
                .names()?
                .into_iter()
                .filter(|n| n.starts_with("environment-"))
            {
                let root = base
                    .child(&name, false)?
                    .context("desktop startup directory disappeared")?;
                if root.read::<runtime::Journal>("owner.json")?.is_none()
                    && let Some(intent) = root.read::<runtime::Launch>("intent.json")?
                    && intent.parent == current
                {
                    anyhow::bail!(
                        "Desktop startup has not confirmed cleanup. Keep its ownership record and run desktop recover."
                    );
                }
            }
        }
        Ok(())
    }

    pub fn close_admission(&self) {
        self.admitted.store(0, Ordering::Release);
    }

    #[cfg(feature = "network")]
    pub fn resident() -> Result<Self> {
        ensure!(
            REQUESTED.get().copied().flatten() != Some(DesktopMode::Shared),
            "the tray app prepares a separate desktop; omit --desktop shared"
        );
        Ok(Self {
            selection: Mutex::new(Selection::new(Some(DesktopMode::Separate))),
            ..Self::new()
        })
    }

    #[cfg(feature = "network")]
    pub fn cancel_preparation(&self) {
        self.cancel.store(true, Ordering::Release);
        self.close_admission();
    }

    #[cfg(feature = "network")]
    pub fn begin_preparation(&self) {
        self.cancel.store(false, Ordering::Release);
        self.admitted.store(1, Ordering::Release);
    }

    #[cfg(feature = "network")]
    pub fn prepare(&self) -> Result<()> {
        if self.ready().unwrap_or(false) {
            return Ok(());
        }
        let previous = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?
            .clone();
        if let Some(previous) = previous {
            previous.shutdown(Deadline::from_millis(30_000))?;
            *self
                .runtime
                .lock()
                .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))? = None;
        }
        command(DesktopCommand::Enable)?;
        ensure!(
            !self.cancel.load(Ordering::Acquire),
            "Desktop preparation cancelled"
        );
        let configuration = configuration()?;
        *self
            .selection
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop selection is poisoned"))? =
            Selection::new(Some(DesktopMode::Separate));
        let base =
            Root::open(&base_path()?, true)?.context("private Windows desktop state directory")?;
        let runtime = Arc::new(Runtime::start(base, configuration.revision, &self.cancel)?);
        *self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))? = Some(runtime);
        ensure!(
            !self.cancel.load(Ordering::Acquire),
            "Desktop preparation cancelled"
        );
        self.admit_prepared()?;
        Ok(())
    }

    #[cfg(feature = "network")]
    fn admit_prepared(&self) -> Result<()> {
        ensure!(
            self.admitted
                .compare_exchange(1, 2, Ordering::AcqRel, Ordering::Acquire)
                .is_ok(),
            "Desktop preparation cancelled before readiness"
        );
        Ok(())
    }

    #[cfg(feature = "network")]
    pub fn ready(&self) -> Result<bool> {
        if self.admitted.load(Ordering::Acquire) != 2 {
            return Ok(false);
        }
        let current = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?
            .clone();
        match current {
            Some(runtime) => runtime.ready(),
            None => Ok(false),
        }
    }

    #[cfg(feature = "network")]
    pub fn viewer(&self, show: bool) -> Result<()> {
        let current = self
            .runtime
            .lock()
            .map_err(|_| anyhow::anyhow!("desktop runtime is poisoned"))?
            .clone();
        current
            .context("Start the development desktop first")?
            .viewer(show)
    }
}

fn base() -> Result<Root> {
    Root::open(&base_path()?, true)?.context("private desktop state directory")
}

fn lock_configuration(base: &Root) -> Result<std::fs::File> {
    lock_named(base, "configuration.lock")
}

pub(super) fn lock_named(base: &Root, filename: &str) -> Result<std::fs::File> {
    let name = std::ffi::OsStr::new(filename);
    let file = if base
        .names()?
        .iter()
        .any(|name| name.eq_ignore_ascii_case(filename))
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
    if matches!(command, DesktopCommand::Open) {
        #[cfg(feature = "network")]
        return super::tray::open();
        #[cfg(not(feature = "network"))]
        anyhow::bail!("the tray app requires the network feature");
    }
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
                "Separate Windows desktop enabled for this account. Open Glass and choose Start development desktop before agent work."
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
        DesktopCommand::Status { .. } | DesktopCommand::Open => {
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

#[cfg(all(test, feature = "network"))]
mod readiness_tests {
    use super::*;

    #[test]
    fn resident_refuses_app_work_before_any_windows_probe_or_signin() {
        let manager = Manager::resident().unwrap();
        let error = manager
            .backend()
            .err()
            .expect("unprepared factory must refuse");
        assert!(matches!(error.cause(), GlassError::EnvironmentNotReady));
        assert_eq!(
            error.bound_dispatch(),
            Some(glass_core::BoundDispatch::NotDispatched)
        );
    }

    #[test]
    fn cancelling_preparation_cannot_be_undone_by_late_worker_readiness() {
        let manager = Manager::resident().unwrap();
        manager.begin_preparation();
        manager.cancel_preparation();
        assert!(manager.admit_prepared().is_err());
        assert!(!manager.ready().unwrap());
        assert!(matches!(
            manager.backend().err().unwrap().cause(),
            GlassError::EnvironmentNotReady
        ));
    }
}
