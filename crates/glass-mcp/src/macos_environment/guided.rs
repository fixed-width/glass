//! Resumable account setup shared by the CLI and the app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;
use crate::macos_setup::{Coordinator, Decision, Phase, Progress};

#[derive(Debug)]
pub(super) struct AccountCreationIncomplete;

impl std::fmt::Display for AccountCreationIncomplete {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("account creation needs attention")
    }
}
impl std::error::Error for AccountCreationIncomplete {}

pub(super) fn phase() -> anyhow::Result<Phase> {
    read_enrollment()?;
    ensure!(
        private_session::is_owner(),
        "account registration is disabled or invalid"
    );
    let lease = File::open(owner_root()?.join(ACCOUNT))?;
    match fs4::FileExt::try_lock(&lease) {
        Ok(()) => {}
        Err(fs4::TryLockError::WouldBlock) => return Ok(Phase::Busy),
        Err(error) => return Err(error.into()),
    }
    if let Err(error) = std::fs::symlink_metadata(owner_root()?.join("worker.sock")) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(Phase::LoginRequired);
        }
        return Err(error.into());
    }
    match crate::macos_worker::backend_transport::readiness_with_lease(lease) {
        Ok(readiness) => Ok(readiness.phase()),
        Err(error) => Ok(Phase::RepairRequired(error.to_string())),
    }
}

pub(super) fn diagnostic_phase() -> anyhow::Result<Phase> {
    let _observation = match environment_lease(crate::macos_setup::LeaseKind::Application) {
        Ok(lease) => lease,
        Err(error)
            if matches!(
                error.downcast_ref::<fs4::TryLockError>(),
                Some(fs4::TryLockError::WouldBlock)
            ) =>
        {
            return Ok(Phase::Busy);
        }
        Err(error) => return Err(error),
    };
    phase()
}

pub(super) struct Options<'a> {
    pub name: &'a str,
    pub create: Option<crate::macos_account::Password>,
    pub non_interactive: bool,
    pub launchagent: bool,
    pub addr: Option<&'a str>,
    pub system_dialog: bool,
    pub cancelled: &'a AtomicBool,
}

pub(super) fn run(options: Options<'_>, mut progress: impl FnMut(Progress)) -> anyhow::Result<()> {
    let _setup = lock_setup()?;
    let Options {
        name,
        create,
        non_interactive,
        launchagent,
        addr,
        system_dialog,
        cancelled,
    } = options;
    progress(Progress::applying(
        "Checking account registration. Complete or cancel macOS authentication locally; take your time.",
    ));
    if let Some(password) = create {
        ensure!(
            !non_interactive,
            "create an account interactively in Glass application setup"
        );
        progress(Progress::applying(
            "Creating the standard development account. Complete or cancel macOS authentication locally; take your time. Your password is not saved by Glass.",
        ));
        account_creation::create(name, &password, system_dialog)
            .context(AccountCreationIncomplete)?;
    } else {
        register_setup(name, non_interactive, system_dialog)?;
    }
    let mut coordinator = Coordinator::new(!non_interactive);
    let mut previous = None;
    loop {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "setup cancelled; completed registration and grants retained"
        );
        let current = phase()?;
        if current != Phase::Ready && previous.as_ref() != Some(&current) {
            progress(Progress::waiting(current.message(name)));
            previous = Some(current.clone());
        }
        let cancelled_now = cancelled.load(Ordering::Relaxed);
        match coordinator.next(&current, cancelled_now) {
            Decision::Finish => {
                progress(Progress::applying(
                    "Verifying workspace recovery and MCP integration…",
                ));
                finish_setup(launchagent, addr)?;
                progress(Progress::waiting(Phase::Ready.message(name)));
                return Ok(());
            }
            Decision::Stop => {
                ensure!(
                    !cancelled_now,
                    "setup cancelled; completed registration and grants retained"
                );
                bail!("{}", current.message(name));
            }
            Decision::OpenPermissions => {
                // The helper is registered and parented by launchd in the development login.
                crate::macos_worker::backend_transport::open_onboarding()?;
            }
            Decision::Repair => {
                progress(Progress::applying(
                    "Repairing the registered worker. Complete or cancel macOS authentication locally; take your time.",
                ));
                elevate_using(
                    &["macos-enroll", name, "--onboarding", "--repair"],
                    false,
                    system_dialog,
                )?;
            }
            Decision::Wait => {}
        }
        std::thread::sleep(Duration::from_millis(1500));
    }
}

pub(super) fn lock_setup() -> anyhow::Result<crate::macos_setup::Lease> {
    environment_lease(crate::macos_setup::LeaseKind::Setup)
}

pub(super) fn environment_lease(
    kind: crate::macos_setup::LeaseKind,
) -> anyhow::Result<crate::macos_setup::Lease> {
    let owner = rustix::process::getuid().as_raw();
    ensure!(
        owner >= 501 && rustix::process::getuid() == rustix::process::geteuid(),
        "run application setup from your normal account"
    );
    let root = crate::permission_journal::private_root(
        &crate::permission_journal::PermissionJournal::root()?,
        owner,
    )?;
    crate::macos_setup::lock_for(&root, owner, kind)
}
