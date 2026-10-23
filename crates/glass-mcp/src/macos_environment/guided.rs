//! Resumable account setup shared by the CLI and the app.

use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use super::*;
use crate::macos_setup::pending::{self, Identity, Pending};
use crate::macos_setup::{Coordinator, Decision, Phase, Progress};

fn saved_root(create: bool) -> anyhow::Result<Option<File>> {
    let path = crate::permission_journal::PermissionJournal::root()?;
    if !create {
        match std::fs::symlink_metadata(path.join(pending::FILE)) {
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
            Err(error) => return Err(error.into()),
            Ok(_) => {}
        }
    }
    match std::fs::symlink_metadata(&path) {
        Err(error) if !create && error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(error) if error.kind() != std::io::ErrorKind::NotFound => return Err(error.into()),
        _ => {}
    }
    let owner = rustix::process::getuid().as_raw();
    ensure!(
        owner >= 501 && rustix::process::geteuid().as_raw() == owner,
        "open application setup from your normal account"
    );
    Ok(Some(crate::permission_journal::private_root(&path, owner)?))
}

pub(super) fn saved() -> anyhow::Result<Option<Pending>> {
    let Some(root) = saved_root(false)? else {
        return Ok(None);
    };
    pending::read(&root, rustix::process::getuid().as_raw())
}

pub(super) fn forget(id: &str) -> anyhow::Result<()> {
    if let Some(root) = saved_root(false)? {
        pending::forget(&root, rustix::process::getuid().as_raw(), id)?;
    }
    Ok(())
}

pub(super) fn forget_saved() -> anyhow::Result<()> {
    if let Some(root) = saved_root(false)? {
        pending::discard(&root, rustix::process::getuid().as_raw())?;
    }
    Ok(())
}

pub(super) fn can_resume(saved: &Pending) -> anyhow::Result<bool> {
    if !private_session::is_owner() {
        return Ok(false);
    }
    let account = read_enrollment()?;
    if account.owner_uid != saved.owner || account.name != saved.name {
        return Ok(false);
    }
    let created = if saved.identity.is_none() && saved.create {
        account_creation::completed_identity(saved.owner, &saved.name)?
    } else {
        None
    };
    Ok(saved.may_resume(
        account.owner_uid,
        &account.name,
        &Identity {
            uid: account.uid,
            uuid: account.record_uuid.clone(),
        },
        true,
        created.as_ref(),
    ) && permission_helper_registered(&account)?)
}

#[derive(Debug)]
pub(super) struct AccountCreationIncomplete;

impl std::fmt::Display for AccountCreationIncomplete {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("account creation needs attention")
    }
}
impl std::error::Error for AccountCreationIncomplete {}

pub(super) fn phase() -> anyhow::Result<Phase> {
    let account = read_enrollment()?;
    ensure!(
        private_session::is_owner(),
        "account registration is disabled or invalid"
    );
    if desktop_start::pending(account.owner_uid)? {
        return Ok(Phase::StartupCleanupRequired);
    }
    if private_session::live_login_observation(account.uid)?
        == private_session::LoginObservation::NotObserved
    {
        return Ok(Phase::LoginRequired);
    }
    let lease = File::open(owner_root()?.join(ACCOUNT))?;
    match fs4::FileExt::try_lock(&lease) {
        Ok(()) => {}
        Err(fs4::TryLockError::WouldBlock) => return Ok(Phase::Busy),
        Err(error) => return Err(error.into()),
    }
    if let Err(error) = std::fs::symlink_metadata(owner_root()?.join("worker.sock")) {
        if error.kind() == std::io::ErrorKind::NotFound {
            return Ok(unavailable_worker(
                account.uid,
                true,
                "the worker activation socket is missing".into(),
            ));
        }
        return Err(error.into());
    }
    match crate::macos_worker::backend_transport::readiness_with_lease(lease) {
        Ok(readiness) => Ok(readiness.phase()),
        Err(error) => Ok(unavailable_worker(account.uid, false, error.to_string())),
    }
}

fn unavailable_worker(uid: u32, socket_missing: bool, reason: String) -> Phase {
    let login = private_session::live_login_observation(uid);
    if matches!(login, Ok(private_session::LoginObservation::Incomplete)) {
        return Phase::WorkerStarting;
    }
    Phase::unavailable_worker(
        socket_missing,
        login
            .map(|login| login == private_session::LoginObservation::Complete)
            .map_err(|error| error.to_string()),
        reason,
    )
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
    pub resume: Option<Pending>,
    pub previous: Option<String>,
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
        resume,
        previous,
    } = options;
    progress(Progress::applying(
        "Checking account registration. Complete or cancel macOS authentication locally; take your time.",
    ));
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "setup cancelled; completed account data retained"
    );
    if let Some(saved) = &resume {
        ensure!(
            saved.name == name
                && (if create.is_some() {
                    saved.create
                } else {
                    can_resume(saved)?
                }),
            "saved setup no longer matches an enabled enrollment; retained without re-enabling it"
        );
        if !self::saved()?.is_some_and(|current| current.id == saved.id) {
            return Err(pending::Superseded.into());
        }
    }
    let identity = if create.is_some() {
        account_creation::completed_identity(rustix::process::getuid().as_raw(), name)?
    } else {
        let account = account_record(name, rustix::process::getuid().as_raw(), false)?;
        Some(Identity {
            uid: account.uid,
            uuid: account.record_uuid,
        })
    };
    let saved = Pending::new(
        rustix::process::getuid().as_raw(),
        name,
        identity,
        create.is_some(),
        launchagent,
        addr.unwrap_or("127.0.0.1:7300"),
    )?;
    let root = saved_root(true)?.context("saved setup storage is unavailable")?;
    pending::remember(
        &root,
        &saved,
        resume
            .as_ref()
            .map(|saved| saved.id.as_str())
            .or(previous.as_deref()),
    )?;
    let mut checkpoint = Progress::applying(
        "Setup progress saved. Complete or cancel macOS authentication locally; take your time.",
    );
    checkpoint.pending_id = Some(saved.id.clone());
    progress(checkpoint);
    let remembered = saved.id;
    ensure!(
        !cancelled.load(Ordering::Relaxed),
        "setup cancelled; completed account data retained"
    );
    let registered = (|| -> anyhow::Result<()> {
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
        } else if resume.is_none() {
            register_setup(name, non_interactive, system_dialog)?;
        }
        Ok(())
    })();
    registered?;
    wait_registered(
        name,
        non_interactive,
        system_dialog,
        cancelled,
        &mut progress,
        || {
            finish_setup(launchagent, addr)?;
            forget(&remembered)
        },
    )
}

#[derive(Debug)]
pub(super) struct EnvironmentMissing;
impl std::fmt::Display for EnvironmentMissing {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("the development desktop needs starting; choose Start environment in Glass application setup")
    }
}
impl std::error::Error for EnvironmentMissing {}

pub(super) fn continue_existing(
    name: &str,
    cancelled: &AtomicBool,
    mut progress: impl FnMut(Progress),
) -> anyhow::Result<()> {
    let _setup = lock_setup()?;
    wait_registered(name, false, true, cancelled, &mut progress, || Ok(()))
}

fn wait_registered(
    name: &str,
    non_interactive: bool,
    system_dialog: bool,
    cancelled: &AtomicBool,
    progress: &mut impl FnMut(Progress),
    finish: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let mut coordinator = Coordinator::new(!non_interactive);
    let mut previous = None;
    loop {
        ensure!(
            !cancelled.load(Ordering::Relaxed),
            "setup cancelled; completed registration and grants retained"
        );
        let current = phase()?;
        if matches!(
            current,
            Phase::LoginRequired | Phase::StartupCleanupRequired
        ) {
            return Err(EnvironmentMissing.into());
        }

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
                ensure!(
                    !cancelled.load(Ordering::Relaxed),
                    "setup cancelled; completed registration and grants retained"
                );
                finish()?;
                progress(Progress::waiting(Phase::Ready.message(name)));
                return Ok(());
            }
            Decision::Stop => {
                ensure!(
                    !cancelled_now,
                    "setup cancelled; completed registration and grants retained"
                );
                ensure!(
                    current != Phase::WorkerStarting,
                    "the development account is logged in, but Glass's worker did not create its activation socket after registration repair; completed account data and grants were retained"
                );
                bail!("{}", current.message(name));
            }
            Decision::OpenPermissions => {
                progress(Progress::applying(
                    "Opening the Glass permission window in the development account…",
                ));
                ensure!(
                    !cancelled.load(Ordering::Relaxed),
                    "setup cancelled; completed registration and grants retained"
                );
                // The helper is registered and parented by launchd in the development login.
                crate::macos_worker::backend_transport::open_onboarding()?;
                progress(Progress::waiting(
                    "Permission window requested. Checking the development account…",
                ));
                previous = None;
            }
            Decision::Repair => {
                progress(Progress::applying(
                    "Repairing the registered worker. Complete or cancel macOS authentication locally; take your time.",
                ));
                ensure!(
                    !cancelled.load(Ordering::Relaxed),
                    "setup cancelled; completed registration and grants retained"
                );
                elevate_using(
                    &["macos-enroll", name, "--onboarding", "--repair"],
                    false,
                    system_dialog,
                )?;
                coordinator.repair_completed();
                progress(Progress::waiting(
                    "Worker registration checked. Waiting for the development account's worker…",
                ));
                previous = None;
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
