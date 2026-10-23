//! Continuous private-output access for one authenticated native backend.

use std::sync::{Arc, Mutex};

use glass_core::{Deadline, GlassError, Result};
use glass_macos::workspace_access::outputs::{OutputAccess, OutputMonitor, OutputScope};

pub(super) struct Outputs {
    access: Arc<Mutex<OutputAccess>>,
    monitors: Vec<(OutputScope, OutputMonitor)>,
    prepared: bool,
}

fn error(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("private output access: {error}"))
}

impl Outputs {
    pub(super) fn new(owner: u32) -> Result<Self> {
        Ok(Self {
            access: Arc::new(Mutex::new(OutputAccess::new(owner).map_err(error)?)),
            monitors: Vec::new(),
            prepared: false,
        })
    }

    pub(super) fn register(
        &mut self,
        scope: Option<OutputScope>,
        deadline: Deadline,
    ) -> Result<()> {
        self.prepared = false;
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "preparing private output access",
            ));
        }
        self.flush(deadline)?;
        let Some(scope) = scope else {
            self.prepared = true;
            return Ok(());
        };
        if self
            .monitors
            .iter()
            .any(|(current, _)| !current.same_protection(&scope))
        {
            return Err(error("protected authority changed").before_dispatch());
        }
        if self.monitors.iter().any(|(current, _)| current == &scope) {
            self.flush(deadline)?;
            self.prepared = true;
            return Ok(());
        }
        if self.monitors.len() >= 64 {
            return Err(error("too many writable roots").before_dispatch());
        }
        let monitor = OutputMonitor::with_access(scope.clone(), Arc::clone(&self.access), deadline)
            .map_err(error)?;
        self.monitors.push((scope, monitor));
        self.flush(deadline)?;
        self.prepared = true;
        Ok(())
    }

    pub(super) fn before_start(&mut self, deadline: Deadline) -> Result<()> {
        if !std::mem::take(&mut self.prepared) {
            return Err(error("writable authority was not prepared").before_dispatch());
        }
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started(
                "checking private output access",
            ));
        }
        self.flush(deadline).map_err(GlassError::before_dispatch)?;
        Ok(())
    }

    pub(super) fn flush(&self, deadline: Deadline) -> Result<()> {
        for (_, monitor) in &self.monitors {
            monitor.flush(deadline).map_err(error)?;
        }
        Ok(())
    }
}

pub(super) fn checked<T>(primary: Result<T>, check: Result<()>) -> Result<T> {
    match (primary, check) {
        (result, Ok(())) => result,
        (Ok(_), Err(error)) => Err(error.after_dispatch()),
        (Err(primary), Err(check)) => {
            Err(
                GlassError::cleanup_failed("checking private output access", primary, check)
                    .after_dispatch(),
            )
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn output_failure_after_success_or_primary_failure_cannot_enable_replay() {
        for primary in [Ok(()), Err(GlassError::AxUnsupported.before_dispatch())] {
            let error = checked(primary, Err(error("observer refused"))).unwrap_err();
            assert_eq!(
                error.bound_dispatch(),
                Some(glass_core::BoundDispatch::MayHaveDispatched)
            );
        }
    }

    #[test]
    fn each_start_requires_fresh_unexpired_authority() {
        let mut outputs = Outputs::new(u32::MAX - 1).unwrap();
        assert!(outputs.before_start(Deadline::from_millis(3000)).is_err());
        assert!(outputs.register(None, Deadline::from_millis(0)).is_err());
        assert!(outputs.before_start(Deadline::from_millis(3000)).is_err());
        outputs.register(None, Deadline::from_millis(3000)).unwrap();
        outputs.before_start(Deadline::from_millis(3000)).unwrap();
        assert!(outputs.before_start(Deadline::from_millis(3000)).is_err());
    }

    fn creator_scope(path: &std::path::Path) -> OutputScope {
        let mut scope = serde_json::to_value(
            OutputScope::capture(path, u32::MAX - 1, &[])
                .unwrap()
                .for_worker(),
        )
        .unwrap();
        let owner = scope["owner"].clone();
        let principal = scope["owner_principal"].clone();
        scope["owner"] = scope["worker"].clone();
        scope["owner_principal"] = scope["worker_principal"].clone();
        scope["worker"] = owner;
        scope["worker_principal"] = principal;
        serde_json::from_value(scope).unwrap()
    }

    #[test]
    fn retained_watcher_failures_block_none_new_scope_and_start_readiness() {
        use std::os::unix::fs::PermissionsExt;
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let first = root.path().join("first");
        let second = root.path().join("second");
        std::fs::create_dir(&first).unwrap();
        std::fs::create_dir(&second).unwrap();
        let mut outputs = Outputs::new(u32::MAX - 1).unwrap();
        outputs
            .register(Some(creator_scope(&first)), Deadline::from_millis(3000))
            .unwrap();
        for index in 0..2 {
            let incoming = root.path().join(format!("incoming-{index}"));
            let blocked = first.join(format!("blocked-{index}"));
            std::fs::write(&incoming, "private output").unwrap();
            std::fs::set_permissions(&incoming, std::fs::Permissions::from_mode(0o000)).unwrap();
            assert!(
                std::process::Command::new("/bin/mv")
                    .arg(&incoming)
                    .arg(&blocked)
                    .status()
                    .unwrap()
                    .success()
            );
            let deadline = Deadline::from_millis(3000);
            while outputs.flush(deadline).is_ok() {
                assert!(!deadline.has_passed());
                std::thread::sleep(std::time::Duration::from_millis(20));
            }
            if index == 0 {
                assert!(outputs.register(None, deadline).is_err());
                assert!(
                    outputs
                        .register(Some(creator_scope(&second)), deadline)
                        .is_err()
                );
                assert_eq!(outputs.monitors.len(), 1);
            }
            assert_eq!(
                outputs.before_start(deadline).unwrap_err().bound_dispatch(),
                Some(glass_core::BoundDispatch::NotDispatched)
            );
            std::fs::set_permissions(&blocked, std::fs::Permissions::from_mode(0o600)).unwrap();
            outputs.register(None, Deadline::from_millis(3000)).unwrap();
        }
    }
}
