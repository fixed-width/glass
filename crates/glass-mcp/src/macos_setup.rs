//! Shared readiness states for the macOS account setup flow.

use serde::{Deserialize, Serialize};

pub(crate) fn selected_account(
    enrolled: Option<&str>,
    pending: Option<&str>,
    creation: Option<&crate::macos_account::Receipt>,
) -> String {
    enrolled
        .or(pending)
        .or_else(|| {
            creation
                .filter(|receipt| !receipt.created || !receipt.password_set)
                .map(|receipt| receipt.name.as_str())
        })
        .unwrap_or_default()
        .to_owned()
}

#[cfg(unix)]
pub(crate) mod pending;

#[cfg(all(test, unix))]
pub(crate) fn lock_at(root: &std::fs::File, owner: u32) -> anyhow::Result<Lease> {
    lock_for(root, owner, LeaseKind::Setup)
}

#[cfg(unix)]
#[derive(Clone, Copy)]
pub(crate) enum LeaseKind {
    Setup,
    Application,
}

#[cfg(unix)]
pub(crate) struct Lease(std::fs::File);

#[cfg(unix)]
impl Lease {
    pub(crate) fn new(file: std::fs::File) -> Self {
        Self(file)
    }
}

#[cfg(unix)]
impl Drop for Lease {
    fn drop(&mut self) {
        // Concurrent process creation can briefly inherit the open file description before exec.
        if let Err(error) = fs4::FileExt::unlock(&self.0) {
            eprintln!("glass: environment lease unlock failed: {error}");
        }
    }
}

#[cfg(unix)]
pub(crate) fn lock_for(root: &std::fs::File, owner: u32, kind: LeaseKind) -> anyhow::Result<Lease> {
    use anyhow::{Context, ensure};
    use rustix::fs::{Mode, OFlags, openat};
    use std::os::unix::fs::MetadataExt;
    let directory = root.metadata()?;
    ensure!(
        directory.is_dir() && directory.uid() == owner && directory.mode() & 0o077 == 0,
        "setup storage is not private to this user"
    );
    #[cfg(target_os = "macos")]
    glass_macos::workspace_access::require_owner_private(root)?;
    let file = std::fs::File::from(openat(
        root,
        "setup.lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let metadata = file.metadata()?;
    ensure!(
        metadata.is_file()
            && metadata.uid() == owner
            && metadata.nlink() == 1
            && metadata.mode() & 0o077 == 0,
        "setup lock is not protected"
    );
    #[cfg(target_os = "macos")]
    glass_macos::workspace_access::require_owner_private(&file)?;
    match kind {
        LeaseKind::Setup => fs4::FileExt::try_lock(&file),
        LeaseKind::Application => fs4::FileExt::try_lock_shared(&file),
    }.context("Glass application work or setup is already running; stop application work or finish the pending setup first")?;
    Ok(Lease::new(file))
}

/// Read-only facts reported by the authenticated worker in its own graphical login.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Readiness {
    pub private_session: bool,
    pub login_setup_pending: bool,
    pub accessibility: bool,
    pub screen_recording: bool,
    pub onboarding_running: bool,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub(crate) enum Phase {
    LoginRequired,
    LoginSetupRequired,
    StartupCleanupRequired,
    WorkerStarting,
    PermissionsRequired {
        accessibility: bool,
        screen_recording: bool,
        helper_running: bool,
    },
    ReturnToOwner,
    FinishingPermissions,
    Ready,
    Busy,
    RepairRequired(String),
}

impl Readiness {
    pub(crate) fn phase(self) -> Phase {
        if self.login_setup_pending {
            Phase::LoginSetupRequired
        } else if !self.accessibility || !self.screen_recording {
            Phase::PermissionsRequired {
                accessibility: !self.accessibility,
                screen_recording: !self.screen_recording,
                helper_running: self.onboarding_running,
            }
        } else if !self.private_session {
            Phase::ReturnToOwner
        } else if self.onboarding_running {
            Phase::FinishingPermissions
        } else {
            Phase::Ready
        }
    }
}

impl Phase {
    #[cfg(target_os = "macos")]
    pub(crate) fn setup_step(&self) -> Option<glass_macos::setup_window::SetupStep> {
        use glass_macos::setup_window::SetupStep;
        match self {
            Self::LoginRequired | Self::LoginSetupRequired | Self::WorkerStarting => {
                Some(SetupStep::SignIn)
            }
            Self::PermissionsRequired { .. } => Some(SetupStep::Permissions),
            Self::ReturnToOwner | Self::FinishingPermissions => Some(SetupStep::ReturnToOwner),
            Self::Ready => Some(SetupStep::Ready),
            Self::StartupCleanupRequired | Self::Busy | Self::RepairRequired(_) => None,
        }
    }

    pub(crate) fn unavailable_worker(
        socket_missing: bool,
        login_complete: Result<bool, String>,
        reason: String,
    ) -> Self {
        match login_complete {
            Ok(false) => Self::LoginRequired,
            Ok(true) if socket_missing => Self::WorkerStarting,
            Ok(true) => Self::RepairRequired(reason),
            Err(error) => Self::RepairRequired(format!(
                "{reason}; macOS login state could not be verified: {error}"
            )),
        }
    }

    pub(crate) fn message(&self, account: &str) -> String {
        match self {
            Self::LoginRequired => format!(
                "For a new account, choose Apple menu → Lock Screen and sign in to {account} manually. Finish all macOS welcome screens until its desktop appears. Then choose Apple menu → Log Out and sign back in to your normal account. Your first-login visit is complete; choose Start environment… here. If its first login is already finished, Start environment… can start its desktop here."
            ),
            Self::LoginSetupRequired => format!(
                "Sign in to {account} manually from Apple menu → Lock Screen. Finish all macOS welcome screens until its desktop appears. Then choose Apple menu → Log Out and sign back in to your normal account. Choose Start environment… here to start its private desktop. Take your time; these steps are not timed."
            ),
            Self::StartupCleanupRequired => "A previous desktop startup needs its temporary access cleaned up. Choose Start environment… in Glass; an already running desktop is reused without authenticating again.".into(),
            Self::WorkerStarting => format!(
                "{account} is logged in. Waiting for its Glass worker to start; registration will be checked automatically if it does not appear."
            ),
            Self::PermissionsRequired { accessibility, screen_recording, .. } => {
                let permissions = match (accessibility, screen_recording) {
                    (true, true) => "Accessibility and Screen Recording",
                    (true, false) => "Accessibility",
                    (false, true) => "Screen Recording",
                    (false, false) => "the requested permissions",
                };
                let settings = match (accessibility, screen_recording) {
                    (true, true) => "Use System Settings → Privacy & Security: allow Accessibility first, then Screen Recording (or Screen & System Audio Recording).",
                    (true, false) => "Use System Settings → Privacy & Security → Accessibility.",
                    (false, true) => "Use System Settings → Privacy & Security → Screen Recording (or Screen & System Audio Recording).",
                    (false, false) => "Check Glass in System Settings → Privacy & Security.",
                };
                format!("In your normal account, allow the missing Glass {permissions}. {settings} Existing grants are reused; you do not need to approve them separately in {account}. If Glass is missing from a list, use + to add /Applications/GlassMcp.app. Glass checks its worker automatically. There is no time limit.")
            }
            Self::ReturnToOwner => format!(
                "Glass has its permissions. After finishing macOS welcome screens in {account}, you can choose Apple menu → Log Out and sign back in to your normal account. Choose Start environment… here if its desktop is stopped. If checking has paused with its desktop running, choose Continue setup. Wait for Ready."
            ),
            Self::FinishingPermissions => "Permissions confirmed. Waiting for the permission window to finish before checking workspace recovery…".into(),
            Self::Ready => format!("{account} ready; apps use its private desktop through the normal MCP tools"),
            Self::Busy => "The Glass environment is in use. Let the current check finish or stop application work, then retry. Setup has not interrupted it.".into(),
            Self::RepairRequired(reason) => format!("Glass setup needs attention: {reason}"),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub(crate) enum Decision {
    Wait,
    OpenPermissions,
    Repair,
    Finish,
    Stop,
}

pub(crate) struct Coordinator {
    interactive: bool,
    permission_retried: bool,
    repaired: bool,
    worker_wait: Option<std::time::Instant>,
}

#[cfg(target_os = "macos")]
pub(crate) struct Progress {
    pub message: String,
    pub step: Option<glass_macos::setup_window::SetupStep>,
    pub can_cancel: bool,
    pub pending_id: Option<String>,
}

#[cfg(target_os = "macos")]
impl Progress {
    pub(crate) fn applying(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            step: None,
            can_cancel: false,
            pending_id: None,
        }
    }
    pub(crate) fn waiting(message: impl Into<String>) -> Self {
        Self {
            message: message.into(),
            step: None,
            can_cancel: true,
            pending_id: None,
        }
    }

    pub(crate) fn at(mut self, step: glass_macos::setup_window::SetupStep) -> Self {
        self.step = Some(step);
        self
    }

    pub(crate) fn for_phase(phase: &Phase, account: &str) -> Self {
        let mut progress = Self::waiting(phase.message(account));
        progress.step = phase.setup_step();
        progress
    }
}

impl Coordinator {
    pub(crate) fn new(interactive: bool) -> Self {
        Self {
            interactive,
            permission_retried: false,
            repaired: false,
            worker_wait: None,
        }
    }

    pub(crate) fn next(&mut self, phase: &Phase, cancelled: bool) -> Decision {
        self.next_at(phase, cancelled, std::time::Instant::now())
    }

    pub(crate) fn repair_completed(&mut self) {
        self.worker_wait = None;
    }

    fn next_at(&mut self, phase: &Phase, cancelled: bool, now: std::time::Instant) -> Decision {
        if cancelled {
            return Decision::Stop;
        }
        if !matches!(phase, Phase::WorkerStarting) {
            self.worker_wait = None;
        }
        match phase {
            Phase::Ready => Decision::Finish,
            Phase::Busy => Decision::Stop,
            _ if !self.interactive => Decision::Stop,
            Phase::WorkerStarting => {
                let started = self.worker_wait.get_or_insert(now);
                if now.saturating_duration_since(*started) < std::time::Duration::from_secs(10) {
                    Decision::Wait
                } else if !self.repaired {
                    self.repaired = true;
                    Decision::Repair
                } else {
                    Decision::Stop
                }
            }
            Phase::PermissionsRequired {
                helper_running: false,
                ..
            } if !self.permission_retried => {
                self.permission_retried = true;
                Decision::OpenPermissions
            }
            Phase::PermissionsRequired {
                helper_running: false,
                ..
            } => Decision::Stop,
            Phase::RepairRequired(_) if !self.repaired => {
                self.repaired = true;
                Decision::Repair
            }
            Phase::RepairRequired(_) => Decision::Stop,
            Phase::LoginRequired | Phase::StartupCleanupRequired => {
                self.repaired = false;
                Decision::Wait
            }
            _ => Decision::Wait,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn creation(completed: bool) -> crate::macos_account::Receipt {
        crate::macos_account::Receipt {
            version: 1,
            owner: 501,
            name: "previousdev".into(),
            uid: 502,
            uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            created: true,
            password_set: completed,
        }
    }

    #[test]
    fn completed_creation_history_does_not_select_an_account_after_uninstall() {
        assert_eq!(selected_account(None, None, Some(&creation(true))), "");
    }

    #[test]
    fn interrupted_creation_keeps_its_selected_name() {
        assert_eq!(
            selected_account(None, None, Some(&creation(false))),
            "previousdev"
        );
    }

    #[test]
    fn current_enrollment_takes_precedence_over_pending_and_historical_accounts() {
        assert_eq!(
            selected_account(Some("current"), Some("pending"), Some(&creation(false))),
            "current"
        );
    }

    #[test]
    fn explicit_saved_setup_keeps_its_account_instead_of_completed_history() {
        assert_eq!(
            selected_account(None, Some("pending"), Some(&creation(true))),
            "pending"
        );
    }

    #[test]
    fn a_logged_in_account_with_a_missing_worker_is_not_told_to_log_in_again() {
        let phase = Phase::unavailable_worker(true, Ok(true), "activation socket missing".into());
        assert_eq!(phase, Phase::WorkerStarting);
    }

    #[test]
    fn an_unknown_login_does_not_become_a_login_instruction_or_ready_claim() {
        let phase = Phase::unavailable_worker(
            true,
            Err("unreadable session metadata".into()),
            "activation socket missing".into(),
        );
        assert!(
            matches!(phase, Phase::RepairRequired(reason) if reason.contains("unreadable session metadata"))
        );
    }

    #[test]
    fn a_failed_connection_after_logout_waits_for_login_without_repairing_registration() {
        let mut setup = Coordinator::new(true);
        let phase = Phase::unavailable_worker(false, Ok(false), "connection refused".into());
        assert_eq!(setup.next(&phase, false), Decision::Wait);
    }

    #[test]
    fn worker_startup_gets_a_grace_period_then_one_repair_and_a_bounded_failure() {
        let mut setup = Coordinator::new(true);
        let now = std::time::Instant::now();
        assert_eq!(
            setup.next_at(&Phase::WorkerStarting, false, now),
            Decision::Wait
        );
        assert_eq!(
            setup.next_at(
                &Phase::WorkerStarting,
                false,
                now + std::time::Duration::from_secs(9)
            ),
            Decision::Wait
        );
        assert_eq!(
            setup.next_at(
                &Phase::WorkerStarting,
                false,
                now + std::time::Duration::from_secs(10)
            ),
            Decision::Repair
        );
        // Time spent authenticating the repair is outside the worker's startup budget.
        setup.repair_completed();
        assert_eq!(
            setup.next_at(
                &Phase::WorkerStarting,
                false,
                now + std::time::Duration::from_secs(3600)
            ),
            Decision::Wait
        );
        assert_eq!(
            setup.next_at(
                &Phase::WorkerStarting,
                false,
                now + std::time::Duration::from_secs(3609)
            ),
            Decision::Wait
        );
        assert_eq!(
            setup.next_at(
                &Phase::WorkerStarting,
                false,
                now + std::time::Duration::from_secs(3610)
            ),
            Decision::Stop
        );
    }

    #[test]
    fn pending_startup_cleanup_never_dispatches_worker_or_permission_repairs() {
        let now = std::time::Instant::now();
        let mut setup = Coordinator::new(true);
        for elapsed in [0, 60, 3600] {
            assert_eq!(
                setup.next_at(
                    &Phase::StartupCleanupRequired,
                    false,
                    now + std::time::Duration::from_secs(elapsed)
                ),
                Decision::Wait
            );
        }
        assert_eq!(
            setup.next(&Phase::StartupCleanupRequired, true),
            Decision::Stop
        );
        assert_eq!(
            Coordinator::new(false).next(&Phase::StartupCleanupRequired, false),
            Decision::Stop
        );
    }

    #[test]
    fn login_and_consent_waits_do_not_inherit_the_machine_startup_deadline() {
        let mut setup = Coordinator::new(true);
        let now = std::time::Instant::now();
        assert_eq!(
            setup.next_at(&Phase::LoginRequired, false, now),
            Decision::Wait
        );
        assert_eq!(
            setup.next_at(
                &Phase::LoginRequired,
                false,
                now + std::time::Duration::from_secs(3600)
            ),
            Decision::Wait
        );
        assert_eq!(
            setup.next_at(
                &Phase::WorkerStarting,
                false,
                now + std::time::Duration::from_secs(3601)
            ),
            Decision::Wait
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inherited_directory_write_ace_cannot_bypass_setup_serialization() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let result = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow add_file,delete_child,search"])
            .arg(directory.path())
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        let root = std::fs::File::open(directory.path()).unwrap();
        assert_eq!(root.metadata().unwrap().mode() & 0o777, 0o700);
        assert!(lock_at(&root, rustix::process::getuid().as_raw()).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn private_posix_lock_with_a_foreign_write_ace_is_refused() {
        use std::os::unix::fs::{MetadataExt, PermissionsExt};
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::File::open(directory.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        drop(lock_at(&root, owner).unwrap());
        let lock = directory.path().join("setup.lock");
        let result = std::process::Command::new("/bin/chmod")
            .args(["+a", "everyone allow write"])
            .arg(&lock)
            .output()
            .unwrap();
        assert!(
            result.status.success(),
            "{}",
            String::from_utf8_lossy(&result.stderr)
        );
        assert_eq!(std::fs::metadata(lock).unwrap().mode() & 0o777, 0o600);
        assert!(lock_at(&root, owner).is_err());
    }

    #[test]
    fn stopped_permission_helper_is_reopened_once_without_repeating_registration() {
        let mut setup = Coordinator::new(true);
        let running = Phase::PermissionsRequired {
            accessibility: false,
            screen_recording: true,
            helper_running: true,
        };
        let stopped = Phase::PermissionsRequired {
            accessibility: false,
            screen_recording: true,
            helper_running: false,
        };
        assert_eq!(setup.next(&running, false), Decision::Wait);
        assert_eq!(setup.next(&stopped, false), Decision::OpenPermissions);
        assert_eq!(setup.next(&running, false), Decision::Wait);
        assert_eq!(setup.next(&stopped, false), Decision::Stop);
    }

    #[cfg(unix)]
    #[test]
    fn application_sessions_block_setup_until_the_last_session_stops() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::File::open(directory.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        let first = lock_for(&root, owner, LeaseKind::Application).unwrap();
        let second = lock_for(&root, owner, LeaseKind::Application).unwrap();
        assert!(lock_at(&root, owner).is_err());
        drop(first);
        assert!(lock_at(&root, owner).is_err());
        drop(second);
        let setup = lock_at(&root, owner).unwrap();
        assert!(lock_for(&root, owner, LeaseKind::Application).is_err());
        drop(setup);
        assert!(lock_for(&root, owner, LeaseKind::Application).is_ok());
    }

    #[test]
    fn setup_waits_without_a_user_deadline_and_finishes_after_return() {
        let mut setup = Coordinator::new(true);
        for _ in 0..10000 {
            assert_eq!(setup.next(&Phase::LoginRequired, false), Decision::Wait);
        }
        let mut missing = Phase::PermissionsRequired {
            accessibility: true,
            screen_recording: true,
            helper_running: false,
        };
        assert_eq!(setup.next(&missing, false), Decision::OpenPermissions);
        if let Phase::PermissionsRequired { helper_running, .. } = &mut missing {
            *helper_running = true;
        }
        for _ in 0..10000 {
            assert_eq!(setup.next(&missing, false), Decision::Wait);
        }
        assert_eq!(setup.next(&Phase::ReturnToOwner, false), Decision::Wait);
        assert_eq!(setup.next(&Phase::Ready, false), Decision::Finish);
    }

    #[test]
    fn resumed_setup_reuses_an_already_started_permission_flow() {
        let mut setup = Coordinator::new(true);
        let missing = Phase::PermissionsRequired {
            accessibility: false,
            screen_recording: true,
            helper_running: true,
        };
        assert_eq!(setup.next(&missing, false), Decision::Wait);
        assert_eq!(setup.next(&Phase::ReturnToOwner, false), Decision::Wait);
        assert_eq!(setup.next(&Phase::Ready, false), Decision::Finish);
    }

    #[test]
    fn noninteractive_setup_never_opens_permissions_or_requests_a_repair() {
        let mut setup = Coordinator::new(false);
        for phase in [
            Phase::LoginRequired,
            Phase::ReturnToOwner,
            Phase::PermissionsRequired {
                accessibility: true,
                screen_recording: true,
                helper_running: false,
            },
            Phase::RepairRequired("connection refused".into()),
        ] {
            assert_eq!(setup.next(&phase, false), Decision::Stop);
        }
        assert_eq!(setup.next(&Phase::Ready, false), Decision::Finish);
    }

    #[test]
    fn failed_repair_stops_instead_of_repeatedly_prompting_for_authorization() {
        let mut setup = Coordinator::new(true);
        let failure = Phase::RepairRequired("connection refused".into());
        assert_eq!(setup.next(&failure, false), Decision::Repair);
        assert_eq!(setup.next(&failure, false), Decision::Stop);
        assert_eq!(setup.next(&Phase::LoginRequired, false), Decision::Wait);
        assert_eq!(setup.next(&failure, false), Decision::Repair);
    }

    #[test]
    fn cancellation_and_busy_work_do_not_dispatch_setup_effects() {
        let mut setup = Coordinator::new(true);
        assert_eq!(setup.next(&Phase::Busy, false), Decision::Stop);
        for phase in [
            Phase::Ready,
            Phase::RepairRequired("repair".into()),
            Phase::PermissionsRequired {
                accessibility: true,
                screen_recording: false,
                helper_running: false,
            },
        ] {
            assert_eq!(setup.next(&phase, true), Decision::Stop);
        }
    }

    #[cfg(unix)]
    #[test]
    fn concurrent_setup_is_refused_and_crashed_setup_can_resume() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::File::open(directory.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        let lock = lock_at(&root, owner).unwrap();
        assert!(lock_at(&root, owner).is_err());
        drop(lock);
        assert!(lock_at(&root, owner).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn inherited_description_does_not_extend_the_controllers_lease() {
        use std::os::unix::fs::PermissionsExt;
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = std::fs::File::open(directory.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        let lease = lock_at(&root, owner).unwrap();
        let inherited = lease.0.try_clone().unwrap();
        drop(lease);
        let next = lock_at(&root, owner).unwrap();
        drop(inherited);
        assert!(lock_at(&root, owner).is_err());
        drop(next);
        assert!(lock_at(&root, owner).is_ok());
    }

    #[cfg(unix)]
    #[test]
    fn setup_lock_refuses_links_and_preserves_the_link_target() {
        use std::os::unix::fs::{PermissionsExt, symlink};
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let target = directory.path().join("unrelated");
        std::fs::write(&target, b"keep").unwrap();
        symlink(&target, directory.path().join("setup.lock")).unwrap();
        let root = std::fs::File::open(directory.path()).unwrap();
        assert!(lock_at(&root, rustix::process::getuid().as_raw()).is_err());
        assert_eq!(std::fs::read(target).unwrap(), b"keep");
    }

    #[test]
    fn incomplete_registration_and_busy_work_are_never_reported_as_ready() {
        for phase in [
            Phase::LoginRequired,
            Phase::Busy,
            Phase::RepairRequired("connection refused".into()),
        ] {
            assert_ne!(phase, Phase::Ready);
            assert!(
                !phase
                    .message("dev")
                    .contains("apps use its private desktop")
            );
        }
        assert!(
            Phase::RepairRequired("connection refused".into())
                .message("dev")
                .contains("connection refused")
        );
    }

    #[test]
    fn readiness_requires_both_permissions_and_a_private_login() {
        for private_session in [false, true] {
            for accessibility in [false, true] {
                for screen_recording in [false, true] {
                    let facts = Readiness {
                        private_session,
                        login_setup_pending: false,
                        accessibility,
                        screen_recording,
                        onboarding_running: false,
                    };
                    assert_eq!(
                        facts.phase() == Phase::Ready,
                        private_session && accessibility && screen_recording
                    );
                }
            }
        }
    }

    #[test]
    fn granted_console_session_requires_return_to_the_normal_account() {
        let facts = Readiness {
            private_session: false,
            login_setup_pending: false,
            accessibility: true,
            screen_recording: true,
            onboarding_running: false,
        };
        assert_eq!(facts.phase(), Phase::ReturnToOwner);
    }

    #[test]
    fn helper_exit_precedes_workspace_recovery_and_ready() {
        let mut facts = Readiness {
            private_session: true,
            login_setup_pending: false,
            accessibility: true,
            screen_recording: true,
            onboarding_running: true,
        };
        let mut setup = Coordinator::new(true);
        assert_eq!(facts.phase(), Phase::FinishingPermissions);
        assert_eq!(setup.next(&facts.phase(), false), Decision::Wait);
        facts.onboarding_running = false;
        assert_eq!(setup.next(&facts.phase(), false), Decision::Finish);
    }

    #[test]
    fn revoked_permission_returns_setup_to_only_the_missing_step() {
        let facts = Readiness {
            private_session: true,
            login_setup_pending: false,
            accessibility: true,
            screen_recording: false,
            onboarding_running: false,
        };
        assert_eq!(
            facts.phase(),
            Phase::PermissionsRequired {
                accessibility: false,
                screen_recording: true,
                helper_running: false,
            }
        );
        assert!(!facts.phase().message("dev").contains("Accessibility"));
    }

    #[test]
    fn unknown_worker_response_never_deserializes_as_ready() {
        for bytes in [
            r#"{}"#,
            r#"{"private_session":true,"accessibility":true}"#,
            r#"{"private_session":true,"accessibility":true,"screen_recording":true,"onboarding_running":false}"#,
            r#"{"private_session":true,"accessibility":true,"screen_recording":true,"extra":true}"#,
        ] {
            assert!(serde_json::from_str::<Readiness>(bytes).is_err());
        }
    }

    #[test]
    fn macos_welcome_screens_precede_permissions_and_ready() {
        for private_session in [false, true] {
            for accessibility in [false, true] {
                for screen_recording in [false, true] {
                    let mut facts = Readiness {
                        private_session,
                        login_setup_pending: true,
                        accessibility,
                        screen_recording,
                        onboarding_running: false,
                    };
                    assert_eq!(facts.phase(), Phase::LoginSetupRequired);
                    #[cfg(target_os = "macos")]
                    assert_eq!(
                        facts.phase().setup_step(),
                        Some(glass_macos::setup_window::SetupStep::SignIn)
                    );
                    assert_eq!(
                        Coordinator::new(true).next(&facts.phase(), false),
                        Decision::Wait
                    );
                    assert_eq!(
                        Coordinator::new(false).next(&facts.phase(), false),
                        Decision::Stop
                    );
                    facts.login_setup_pending = false;
                    assert_eq!(
                        facts.phase() == Phase::Ready,
                        private_session && accessibility && screen_recording
                    );
                }
            }
        }
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn guide_completion_follows_verified_readiness_and_preserves_unknown_progress() {
        use glass_macos::setup_window::SetupStep;
        assert_eq!(Phase::Ready.setup_step(), Some(SetupStep::Ready));
        assert_eq!(
            Phase::ReturnToOwner.setup_step(),
            Some(SetupStep::ReturnToOwner)
        );
        assert_eq!(
            Phase::PermissionsRequired {
                accessibility: true,
                screen_recording: false,
                helper_running: true
            }
            .setup_step(),
            Some(SetupStep::Permissions)
        );
        for phase in [
            Phase::Busy,
            Phase::StartupCleanupRequired,
            Phase::RepairRequired("unconfirmed connection".into()),
        ] {
            assert_eq!(phase.setup_step(), None);
        }
    }
}
