//! Activate existing, identity-bound jobs after an enrolled graphical login returns.

use super::*;

fn bootstrap_job(domain: &str, path: &Path) -> anyhow::Result<()> {
    let result = glass_core::run_bounded(
        Command::new("/bin/launchctl")
            .arg("bootstrap")
            .arg(domain)
            .arg(path)
            .env("LC_ALL", "C"),
        std::time::Duration::from_secs(15),
        "activating the registered development job",
    )?;
    ensure!(
        result.status.success(),
        "launchd bootstrap returned {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
            .trim()
            .chars()
            .take(2048)
            .collect::<String>()
    );
    Ok(())
}

fn binding(loaded: &str, account: &Enrollment, binary: &Path) -> anyhow::Result<()> {
    ensure!(
        loaded_job_matches(
            loaded,
            &[
                &binary.to_string_lossy(),
                "macos-worker",
                &account.owner_uid.to_string()
            ]
        ),
        "loaded worker belongs to another installation; registration retained"
    );
    Ok(())
}

fn socket_identity(root: &File) -> anyhow::Result<Option<(u64, u64)>> {
    match rustix::fs::statat(root, "worker.sock", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(info) => Ok(Some((info.st_dev as u64, info.st_ino))),
        Err(rustix::io::Errno::NOENT) => Ok(None),
        Err(error) => Err(error.into()),
    }
}

fn start_if_missing_with(
    root: &File,
    account: &Enrollment,
    binary: &Path,
    loaded: &str,
    start: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    binding(loaded, account, binary)?;
    if socket_identity(root)?.is_none() && crate::setup::checked_loaded_agent_pid(loaded)?.is_none()
    {
        // First-login domains can defer even socket setup until the job has explicit demand.
        if let Err(error) = start()
            && socket_identity(root)?.is_none()
        {
            return Err(error);
        }
    }
    Ok(())
}

fn start_job(target: &str) -> anyhow::Result<()> {
    let result = glass_core::run_bounded(
        Command::new("/bin/launchctl")
            .args(["kickstart", target])
            .env("LC_ALL", "C"),
        std::time::Duration::from_secs(15),
        "starting the registered development worker",
    )?;
    ensure!(
        result.status.success(),
        "launchd worker start returned {}: {}",
        result.status,
        String::from_utf8_lossy(&result.stderr)
            .trim()
            .chars()
            .take(2048)
            .collect::<String>()
    );
    Ok(())
}

/// Caller holds the owner and worker mutation locks after enabling registration.
pub(super) fn start_if_missing(
    root: &File,
    account: &Enrollment,
    binary: &Path,
) -> anyhow::Result<()> {
    let target = format!("gui/{}/{LABEL}", account.uid);
    let crate::launchd::Inspection::Loaded(loaded) = crate::launchd::inspect_for_setup(&target)?
    else {
        bail!("registered development worker disappeared before startup; registration retained");
    };
    start_if_missing_with(root, account, binary, &loaded, || start_job(&target))
}

fn activate_with(
    root: &File,
    account: &Enrollment,
    binary: &Path,
    mut inspect: impl FnMut() -> anyhow::Result<crate::launchd::Inspection>,
    mut stop: impl FnMut() -> anyhow::Result<()>,
    mut bootstrap: impl FnMut() -> anyhow::Result<()>,
    start: impl FnOnce() -> anyhow::Result<()>,
) -> anyhow::Result<()> {
    let state = inspect()?;
    if let crate::launchd::Inspection::Loaded(loaded) = &state {
        binding(loaded, account, binary)?;
    }
    let before =
        match rustix::fs::statat(root, "worker.sock", rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
            Ok(info) => {
                validate_socket(&info, account.owner_uid, account.uid)?;
                Some((info.st_dev as u64, info.st_ino))
            }
            Err(rustix::io::Errno::NOENT) => None,
            Err(error) => return Err(error.into()),
        };
    match state {
        crate::launchd::Inspection::Loaded(loaded) => {
            ensure!(
                crate::setup::checked_loaded_agent_pid(&loaded)?.is_none(),
                "the registered worker is still starting or busy; it was not interrupted"
            );
            stop()?;
        }
        crate::launchd::Inspection::MissingJob => {}
        crate::launchd::Inspection::MissingDomain | crate::launchd::Inspection::GuiUnavailable => {
            bail!(
                "the development graphical login exists, but its launchd GUI domain is not available; no worker was bootstrapped"
            )
        }
    }
    remove_socket(root, account.owner_uid, account.uid)?;
    let requested = bootstrap();
    let observed = inspect();
    let loaded = match observed {
        Ok(crate::launchd::Inspection::Loaded(loaded)) => loaded,
        Ok(_) => {
            if let Err(error) = requested {
                return Err(error.context("worker bootstrap did not leave its registered job"));
            }
            bail!("worker bootstrap returned without its registered job");
        }
        Err(error) => {
            return Err(match requested {
                Ok(()) => error,
                Err(request) => error.context(format!("worker bootstrap: {request:#}")),
            });
        }
    };
    start_if_missing_with(root, account, binary, &loaded, start)?;
    wait_for_socket(root, account.owner_uid, account.uid)?;
    ensure!(
        before.is_none() || socket_identity(root)? != before,
        "worker bootstrap did not replace its stale activation socket"
    );
    Ok(())
}

fn onboarding_loaded(account: &Enrollment, binary: &Path) -> anyhow::Result<()> {
    let domain = format!("gui/{}", account.uid);
    let target = format!("{domain}/{ONBOARD_LABEL}");
    let executable = binary.to_string_lossy();
    let owner = account.owner_uid.to_string();
    let expected = [executable.as_ref(), "macos-onboarding", owner.as_str()];
    match crate::launchd::inspect_for_setup(&target)? {
        crate::launchd::Inspection::Loaded(loaded) => {
            ensure!(
                loaded_job_matches(&loaded, &expected),
                "loaded permission helper changed; registration retained"
            );
        }
        crate::launchd::Inspection::MissingJob => {
            let result = bootstrap_job(
                &domain,
                &account.home.join("Library/LaunchAgents").join(ONBOARD_JOB),
            );
            match crate::launchd::inspect_for_setup(&target)? {
                crate::launchd::Inspection::Loaded(loaded) => {
                    ensure!(
                        loaded_job_matches(&loaded, &expected),
                        "permission helper bootstrap loaded a different job"
                    );
                }
                _ => {
                    result?;
                    bail!("permission helper bootstrap returned without its registered job");
                }
            }
        }
        _ => bail!("development GUI domain is unavailable for its registered permission helper"),
    }
    Ok(())
}

/// Caller holds owner/worker mutation locks and the authenticated owner holds the account lease.
pub(super) fn activate(root: &File, account: &Enrollment) -> anyhow::Result<()> {
    ensure!(
        root_owner()? == account.owner_uid,
        "worker activation owner changed"
    );
    ensure!(
        stored_file(root, "disabled")?.is_none(),
        "account enrollment is disabled"
    );
    let binary = std::env::current_exe()?.canonicalize()?;
    ensure!(
        crate::setup::is_inside_app_bundle(&binary),
        "activate the installed Glass app"
    );
    let requirement = private_session::signing_requirement()?;
    ensure!(
        stored_file(root, "worker.requirement")?.as_deref() == Some(requirement.as_slice()),
        "installed worker requirement changed; registration retained"
    );
    let agents = directory(
        &account.home.join("Library/LaunchAgents"),
        account.uid,
        false,
    )?;
    ensure!(
        same_file(&agents, JOB, &job(account, &binary)?)?,
        "the persistent worker job is missing; open application setup to repair registration"
    );
    let onboarding = onboarding_job(account, &binary)?;
    ensure!(
        same_file(root, ONBOARD_JOB, &onboarding)? && same_file(&agents, ONBOARD_JOB, &onboarding)?,
        "the registered permission helper is missing or changed; registration retained"
    );
    let target = format!("gui/{}/{LABEL}", account.uid);
    activate_with(
        root,
        account,
        &binary,
        || crate::launchd::inspect_for_setup(&target),
        || stop_registered_worker(&target, account),
        || {
            bootstrap_job(
                &format!("gui/{}", account.uid),
                &account.home.join("Library/LaunchAgents").join(JOB),
            )
        },
        || start_job(&target),
    )?;
    onboarding_loaded(account, &binary)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::cell::{Cell, RefCell};
    use std::collections::VecDeque;
    use std::os::unix::fs::PermissionsExt;
    use std::os::unix::net::{UnixListener, UnixStream};

    fn account() -> Enrollment {
        Enrollment {
            version: 1,
            owner_uid: rustix::process::getuid().as_raw(),
            uid: 59998,
            gid: 20,
            name: "development".into(),
            record_uuid: "test-record".into(),
            home: "/Users/development".into(),
        }
    }
    fn binary() -> &'static Path {
        Path::new("/Applications/GlassMcp.app/Contents/MacOS/glass-mcp")
    }
    fn loaded(account: &Enrollment, pid: Option<u32>) -> String {
        let process = pid
            .map(|pid| format!("\tpid = {pid}\n"))
            .unwrap_or_default();
        format!(
            "worker = {{\n{process}\tprogram = {}\n\targuments = {{\n\t\t{}\n\t\tmacos-worker\n\t\t{}\n\t}}\n}}\n",
            binary().display(),
            binary().display(),
            account.owner_uid
        )
    }
    fn listener(path: &Path) -> UnixListener {
        let listener = UnixListener::bind(path).unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600)).unwrap();
        listener
    }

    #[test]
    fn stale_missing_job_is_bootstrapped_once_before_a_fresh_listener_is_exposed() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let old = listener(&path);
        drop(old); // A valid node with no listener is the observed failure.
        assert_eq!(
            UnixStream::connect(&path).unwrap_err().kind(),
            std::io::ErrorKind::ConnectionRefused
        );
        let root = File::open(directory.path()).unwrap();
        let before = socket_identity(&root).unwrap();
        let account = account();
        let states = RefCell::new(VecDeque::from([
            crate::launchd::Inspection::MissingJob,
            crate::launchd::Inspection::Loaded(loaded(&account, None)),
        ]));
        let bootstraps = Cell::new(0);
        let live = RefCell::new(None);
        activate_with(
            &root,
            &account,
            binary(),
            || Ok(states.borrow_mut().pop_front().unwrap()),
            || panic!("a missing job must not be stopped"),
            || {
                bootstraps.set(bootstraps.get() + 1);
                assert!(!path.exists());
                *live.borrow_mut() = Some(listener(&path));
                Ok(())
            },
            || panic!("an existing socket does not need an explicit start"),
        )
        .unwrap();
        assert_eq!(bootstraps.get(), 1);
        assert_ne!(socket_identity(&root).unwrap(), before);
        let _connection = UnixStream::connect(&path).unwrap();
    }

    #[test]
    fn unrelated_loaded_job_is_refused_before_socket_removal_or_stop() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let _listener = listener(&path);
        let root = File::open(directory.path()).unwrap();
        let before = socket_identity(&root).unwrap();
        let account = account();
        let wrong = loaded(&account, None).replace("macos-worker", "other-command");
        assert!(
            activate_with(
                &root,
                &account,
                binary(),
                || Ok(crate::launchd::Inspection::Loaded(wrong.clone())),
                || panic!("unbound stop"),
                || panic!("unbound bootstrap"),
                || panic!("unbound start")
            )
            .is_err()
        );
        assert_eq!(socket_identity(&root).unwrap(), before);
    }

    #[test]
    fn active_loaded_worker_is_never_stopped() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let account = account();
        assert!(
            activate_with(
                &root,
                &account,
                binary(),
                || Ok(crate::launchd::Inspection::Loaded(loaded(
                    &account,
                    Some(123)
                ))),
                || panic!("active worker stop"),
                || panic!("active worker bootstrap"),
                || panic!("active worker start")
            )
            .is_err()
        );
    }

    #[test]
    fn unavailable_domain_never_removes_or_bootstraps() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let _listener = listener(&path);
        let root = File::open(directory.path()).unwrap();
        let before = socket_identity(&root).unwrap();
        for unavailable in [false, true] {
            assert!(
                activate_with(
                    &root,
                    &account(),
                    binary(),
                    || Ok(if unavailable {
                        crate::launchd::Inspection::GuiUnavailable
                    } else {
                        crate::launchd::Inspection::MissingDomain
                    }),
                    || panic!("domain stop"),
                    || panic!("domain bootstrap"),
                    || panic!("domain start")
                )
                .is_err()
            );
        }
        assert_eq!(socket_identity(&root).unwrap(), before);
    }

    #[test]
    fn owned_idle_registration_is_observed_stopped_before_socket_replacement() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let old = RefCell::new(Some(listener(&path)));
        let root = File::open(directory.path()).unwrap();
        let account = account();
        let state = loaded(&account, None);
        let stopped = Cell::new(false);
        let live = RefCell::new(None);
        activate_with(
            &root,
            &account,
            binary(),
            || Ok(crate::launchd::Inspection::Loaded(state.clone())),
            || {
                old.borrow_mut().take();
                stopped.set(true);
                Ok(())
            },
            || {
                assert!(stopped.get());
                assert!(!path.exists());
                *live.borrow_mut() = Some(listener(&path));
                Ok(())
            },
            || panic!("a bootstrapped listener does not need an explicit start"),
        )
        .unwrap();
        let _connection = UnixStream::connect(&path).unwrap();
    }

    #[test]
    fn post_dispatch_error_is_reconciled_without_replaying_bootstrap() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let root = File::open(directory.path()).unwrap();
        let account = account();
        let states = RefCell::new(VecDeque::from([
            crate::launchd::Inspection::MissingJob,
            crate::launchd::Inspection::Loaded(loaded(&account, None)),
        ]));
        let calls = Cell::new(0);
        let live = RefCell::new(None);
        activate_with(
            &root,
            &account,
            binary(),
            || Ok(states.borrow_mut().pop_front().unwrap()),
            || panic!("no prior job"),
            || {
                calls.set(calls.get() + 1);
                *live.borrow_mut() = Some(listener(&path));
                bail!("command reported failure after job creation")
            },
            || panic!("an existing listener does not need an explicit start"),
        )
        .unwrap();
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn failed_bootstrap_is_not_replayed_or_reported_as_active() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let calls = Cell::new(0);
        assert!(
            activate_with(
                &root,
                &account(),
                binary(),
                || Ok(crate::launchd::Inspection::MissingJob),
                || panic!("no prior job"),
                || {
                    calls.set(calls.get() + 1);
                    bail!("failed before dispatch")
                },
                || panic!("failed bootstrap must not demand a start")
            )
            .is_err()
        );
        assert_eq!(calls.get(), 1);
    }

    #[test]
    fn deferred_socket_setup_gets_one_explicit_start_after_bound_bootstrap() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let root = File::open(directory.path()).unwrap();
        let account = account();
        let states = RefCell::new(VecDeque::from([
            crate::launchd::Inspection::MissingJob,
            crate::launchd::Inspection::Loaded(loaded(&account, None)),
        ]));
        let bootstrapped = Cell::new(false);
        let starts = Cell::new(0);
        let live = RefCell::new(None);
        activate_with(
            &root,
            &account,
            binary(),
            || Ok(states.borrow_mut().pop_front().unwrap()),
            || panic!("a missing job cannot be stopped"),
            || {
                bootstrapped.set(true);
                Ok(())
            },
            || {
                assert!(bootstrapped.get());
                assert!(!path.exists());
                starts.set(starts.get() + 1);
                *live.borrow_mut() = Some(listener(&path));
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(starts.get(), 1);
        let _connection = UnixStream::connect(path).unwrap();
    }

    #[test]
    fn a_replaced_job_after_bootstrap_is_never_demand_started() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let account = account();
        let wrong = loaded(&account, None).replace("macos-worker", "other-command");
        let states = RefCell::new(VecDeque::from([
            crate::launchd::Inspection::MissingJob,
            crate::launchd::Inspection::Loaded(wrong),
        ]));
        assert!(
            activate_with(
                &root,
                &account,
                binary(),
                || Ok(states.borrow_mut().pop_front().unwrap()),
                || panic!("no prior job"),
                || Ok(()),
                || panic!("unbound demand start"),
            )
            .is_err()
        );
        assert!(!directory.path().join("worker.sock").exists());
    }

    #[test]
    fn a_running_worker_waits_for_its_socket_without_another_start_request() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let account = account();
        start_if_missing_with(
            &root,
            &account,
            binary(),
            &loaded(&account, Some(123)),
            || panic!("running worker must not be started again"),
        )
        .unwrap();
        assert!(!directory.path().join("worker.sock").exists());
    }

    #[test]
    fn a_failed_start_without_a_socket_is_not_replayed_or_hidden() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let account = account();
        let calls = Cell::new(0);
        let error =
            start_if_missing_with(&root, &account, binary(), &loaded(&account, None), || {
                calls.set(calls.get() + 1);
                bail!("start was refused")
            })
            .unwrap_err();
        assert_eq!(calls.get(), 1);
        assert_eq!(error.to_string(), "start was refused");
    }

    #[test]
    fn ambiguous_pid_output_never_authorizes_a_stop_or_start() {
        let directory = tempfile::tempdir().unwrap();
        let root = File::open(directory.path()).unwrap();
        let account = account();
        for field in ["pid = 0", "pid = invalid", "pid=42", "pid = 42\n\tpid = 43"] {
            let job = loaded(&account, None).replacen('\n', &format!("\n\t{field}\n"), 1);
            assert!(
                start_if_missing_with(&root, &account, binary(), &job, || panic!(
                    "ambiguous start"
                ))
                .is_err(),
                "{field}"
            );
            assert!(
                activate_with(
                    &root,
                    &account,
                    binary(),
                    || Ok(crate::launchd::Inspection::Loaded(job.clone())),
                    || panic!("ambiguous stop"),
                    || panic!("ambiguous bootstrap"),
                    || panic!("ambiguous start"),
                )
                .is_err(),
                "{field}"
            );
        }
    }

    #[test]
    fn a_start_error_after_socket_publication_is_reconciled_without_replay() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("worker.sock");
        let root = File::open(directory.path()).unwrap();
        let account = account();
        let calls = Cell::new(0);
        let live = RefCell::new(None);
        start_if_missing_with(&root, &account, binary(), &loaded(&account, None), || {
            calls.set(calls.get() + 1);
            *live.borrow_mut() = Some(listener(&path));
            bail!("start completed before reporting an error")
        })
        .unwrap();
        wait_for_socket(&root, account.owner_uid, account.uid).unwrap();
        assert_eq!(calls.get(), 1);
        let _connection = UnixStream::connect(path).unwrap();
    }
}
