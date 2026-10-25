//! Observe launchd teardown before replacing or removing a registered job.

#[cfg(target_os = "macos")]
use anyhow::Context;
use anyhow::bail;
use std::time::{Duration, Instant};

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Inspection {
    Loaded(String),
    MissingJob,
    MissingDomain,
    GuiUnavailable,
}

impl Inspection {
    fn into_job(self, target: &str) -> anyhow::Result<Option<String>> {
        match self {
            Self::Loaded(job) => Ok(Some(job)),
            Self::MissingJob | Self::MissingDomain => Ok(None),
            Self::GuiUnavailable => {
                bail!("cannot inspect launchd job {target}: graphical domain is unavailable (125)")
            }
        }
    }
}

fn target_parts(target: &str) -> Option<(&str, &str, &str)> {
    let mut parts = target.splitn(3, '/');
    let (domain, uid, label) = (parts.next()?, parts.next()?, parts.next()?);
    if !matches!(domain, "gui" | "user") || uid.parse::<u32>().is_err() || label.is_empty() {
        return None;
    }
    Some((domain, uid, label))
}

fn missing(target: &str, code: Option<i32>, stderr: &str) -> bool {
    let Some((domain, uid, label)) = target_parts(target) else {
        return false;
    };
    let expected = match code {
        Some(113) => {
            format!("Could not find service \"{label}\" in domain for user {domain}: {uid}")
        }
        Some(112) => format!("Could not find domain for user {domain}: {uid}"),
        _ => return false,
    };
    stderr.lines().last() == Some(expected.as_str())
}

fn interpret(
    target: &str,
    code: Option<i32>,
    stdout: Vec<u8>,
    stderr: Vec<u8>,
) -> anyhow::Result<Inspection> {
    if code == Some(0) {
        return Ok(Inspection::Loaded(String::from_utf8(stdout)?));
    }
    let stderr = String::from_utf8_lossy(&stderr);
    if missing(target, code, &stderr) {
        return Ok(if code == Some(112) {
            Inspection::MissingDomain
        } else {
            Inspection::MissingJob
        });
    }
    if matches!(target_parts(target), Some(("gui", _, _)))
        && code == Some(125)
        && stdout.is_empty()
        && stderr.trim_end_matches(['\r', '\n'])
            == "Could not print domain: 125: Domain does not support specified action"
    {
        return Ok(Inspection::GuiUnavailable);
    }
    bail!("unrecognized launchd inspection result: {}", stderr.trim())
}

#[cfg(target_os = "macos")]
pub(crate) fn inspect_for_setup(target: &str) -> anyhow::Result<Inspection> {
    let result = glass_core::run_bounded(
        std::process::Command::new("/bin/launchctl")
            .args(["print", target])
            .env("LC_ALL", "C"),
        Duration::from_secs(5),
        "inspecting the registered launchd job",
    )?;
    interpret(target, result.status.code(), result.stdout, result.stderr)
        .with_context(|| format!("cannot inspect launchd job {target}: {}", result.status))
}

#[cfg(target_os = "macos")]
pub(crate) fn inspect(target: &str) -> anyhow::Result<Option<String>> {
    inspect_for_setup(target)?.into_job(target)
}

fn wait_removed(
    timeout: Duration,
    interval: Duration,
    mut removed: impl FnMut() -> anyhow::Result<bool>,
) -> anyhow::Result<()> {
    let deadline = Instant::now() + timeout;
    loop {
        if removed()? {
            return Ok(());
        }
        if Instant::now() >= deadline {
            bail!("job or its original process has not finished stopping; registration retained");
        }
        std::thread::sleep(interval);
    }
}

#[cfg(target_os = "macos")]
pub(crate) fn stop(target: &str, uid: u32) -> anyhow::Result<()> {
    let Some(initial) = inspect(target)? else {
        return Ok(());
    };
    let process = match crate::setup::checked_loaded_agent_pid(&initial)? {
        Some(pid) => glass_macos::private_session::account_processes(uid)?
            .into_iter()
            .find(|process| process.matches_pid(pid)),
        None => None,
    };
    let result = glass_core::run_bounded(
        std::process::Command::new("/bin/launchctl")
            .args(["bootout", target])
            .env("LC_ALL", "C"),
        Duration::from_secs(5),
        "stopping the registered launchd job",
    );
    // A command can report an error while its teardown request still takes effect.
    wait_removed(Duration::from_secs(15), Duration::from_millis(50), || {
        let process_gone = match process {
            Some(process) => {
                !glass_macos::private_session::account_processes(uid)?.contains(&process)
            }
            None => true,
        };
        Ok(inspect(target)?.is_none() && process_gone)
    })
    .with_context(|| match result {
        Ok(result) => format!(
            "stopping {target}; bootout returned {}: {}",
            result.status,
            String::from_utf8_lossy(&result.stderr).trim()
        ),
        Err(error) => format!("stopping {target}; bootout did not finish: {error:#}"),
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn permission_and_foreign_job_errors_are_not_absence() {
        let target = "gui/501/tech.fixedwidth.glass";
        let absent = "Bad request.\nCould not find service \"tech.fixedwidth.glass\" in domain for user gui: 501\n";
        assert!(missing(target, Some(113), absent));
        assert!(!missing(target, Some(1), absent));
        assert!(!missing("gui/502/tech.fixedwidth.glass", Some(113), absent));
        assert!(!missing("gui/501/another.job", Some(113), absent));
        assert!(!missing(target, Some(113), "Operation not permitted\n"));
        assert!(missing(
            target,
            Some(112),
            "Bad request.\nCould not find domain for user gui: 501\n"
        ));
    }

    #[test]
    fn first_login_gui_unavailability_is_a_distinct_setup_state() {
        let observed = b"Could not print domain: 125: Domain does not support specified action\n";
        assert_eq!(
            interpret(
                "gui/501/dev.example.worker",
                Some(125),
                Vec::new(),
                observed.to_vec()
            )
            .unwrap(),
            Inspection::GuiUnavailable
        );
    }

    #[test]
    fn unavailable_gui_is_not_cleanup_confirmation() {
        let target = "gui/501/dev.example.worker";
        let observed = interpret(
            target,
            Some(125),
            Vec::new(),
            b"Could not print domain: 125: Domain does not support specified action\n".to_vec(),
        )
        .unwrap();
        assert!(observed.into_job(target).is_err());
    }

    #[test]
    fn setup_distinguishes_a_missing_service_from_a_missing_domain() {
        let target = "gui/501/dev.example.worker";
        assert_eq!(
            interpret(
                target,
                Some(113),
                Vec::new(),
                b"Could not find service \"dev.example.worker\" in domain for user gui: 501\n"
                    .to_vec()
            )
            .unwrap(),
            Inspection::MissingJob
        );
        assert_eq!(
            interpret(
                target,
                Some(112),
                Vec::new(),
                b"Could not find domain for user gui: 501\n".to_vec()
            )
            .unwrap(),
            Inspection::MissingDomain
        );
    }

    #[test]
    fn prelogin_response_does_not_hide_other_domains_or_inspection_failures() {
        let observed = "Could not print domain: 125: Domain does not support specified action\n";
        for (target, code, stdout, stderr) in [
            ("user/501/dev.example.worker", Some(125), "", observed),
            ("system/dev.example.worker", Some(125), "", observed),
            ("gui/not-a-uid/worker", Some(125), "", observed),
            ("gui/501/", Some(125), "", observed),
            ("gui/501/dev.example.worker", Some(1), "", observed),
            ("gui/501/dev.example.worker", None, "", observed),
            (
                "gui/501/dev.example.worker",
                Some(125),
                "job data",
                observed,
            ),
            (
                "gui/501/dev.example.worker",
                Some(125),
                "",
                "Operation not permitted\nCould not print domain: 125: Domain does not support specified action\n",
            ),
        ] {
            assert!(
                interpret(target, code, stdout.into(), stderr.into()).is_err(),
                "unexpectedly accepted {target}, {code:?}, {stdout:?}, {stderr:?}"
            );
        }
    }

    #[test]
    fn loaded_job_output_is_preserved_and_invalid_utf8_is_refused() {
        let target = "gui/501/dev.example.worker";
        assert_eq!(
            interpret(target, Some(0), b"loaded job\n".to_vec(), Vec::new())
                .unwrap()
                .into_job(target)
                .unwrap(),
            Some("loaded job\n".into())
        );
        assert!(interpret(target, Some(0), vec![0xff], Vec::new()).is_err());
    }

    #[test]
    fn removal_waits_for_both_job_and_original_process() {
        let observations = [(true, false), (true, false), (false, false), (false, true)];
        let mut next = observations.into_iter();
        let mut count = 0;
        wait_removed(Duration::from_secs(1), Duration::ZERO, || {
            count += 1;
            let (loaded, process_gone) = next.next().unwrap();
            Ok(!loaded && process_gone)
        })
        .unwrap();
        assert_eq!(count, 4);
    }

    #[test]
    fn unreadable_job_stops_the_wait_without_treating_it_as_removed() {
        let error = wait_removed(Duration::from_secs(1), Duration::ZERO, || {
            bail!("not authorised to inspect this domain")
        })
        .unwrap_err();
        assert!(error.to_string().contains("not authorised"));
    }

    #[test]
    fn persistent_job_returns_an_error_instead_of_allowing_replacement() {
        assert!(wait_removed(Duration::ZERO, Duration::ZERO, || Ok(false)).is_err());
    }

    #[cfg(target_os = "macos")]
    #[test]
    #[ignore = "requires the caller's graphical launchd domain; creates only an owned process job"]
    fn owned_job_with_delayed_exit_is_gone_before_stop_returns() {
        use std::process::Command;
        let uid = rustix::process::getuid().as_raw();
        let directory = tempfile::tempdir().unwrap();
        let ready = directory.path().join("ready");
        let label = format!(
            "tech.fixedwidth.glass.test.stop.{:032x}",
            rand::random::<u128>()
        );
        let target = format!("gui/{uid}/{label}");
        assert!(inspect(&target).unwrap().is_none());
        let script = "import os,signal,sys,time\nfrom pathlib import Path\ndef stop(*args):\n time.sleep(.5)\n sys.exit(0)\nsignal.signal(signal.SIGTERM,stop)\nsignal.signal(signal.SIGALRM,lambda *args:os._exit(0))\nsignal.alarm(20)\nPath(sys.argv[1]).write_text(str(os.getpid()))\nwhile True:signal.pause()\n";
        let mut job = plist::Dictionary::new();
        job.insert("Label".into(), label.into());
        job.insert(
            "ProgramArguments".into(),
            plist::Value::Array(vec![
                "/usr/bin/python3".into(),
                "-I".into(),
                "-B".into(),
                "-c".into(),
                script.into(),
                ready.to_string_lossy().into_owned().into(),
            ]),
        );
        job.insert("RunAtLoad".into(), true.into());
        job.insert("KeepAlive".into(), false.into());
        let path = directory.path().join("job.plist");
        plist::to_file_xml(&path, &job).unwrap();
        struct Cleanup(String);
        impl Drop for Cleanup {
            fn drop(&mut self) {
                let _ = Command::new("/bin/launchctl")
                    .args(["bootout", &self.0])
                    .output();
            }
        }
        let _cleanup = Cleanup(target.clone());
        let loaded = Command::new("/bin/launchctl")
            .args(["bootstrap", &format!("gui/{uid}")])
            .arg(path)
            .output()
            .unwrap();
        assert!(
            loaded.status.success(),
            "{}",
            String::from_utf8_lossy(&loaded.stderr)
        );
        let deadline = Instant::now() + Duration::from_secs(10);
        while !ready.exists() {
            assert!(Instant::now() < deadline, "owned process did not start");
            std::thread::sleep(Duration::from_millis(20));
        }
        let pid = std::fs::read_to_string(ready)
            .unwrap()
            .parse::<u32>()
            .unwrap();
        let process = glass_macos::private_session::account_processes(uid)
            .unwrap()
            .into_iter()
            .find(|process| process.matches_pid(pid))
            .unwrap();
        stop(&target, uid).unwrap();
        assert!(inspect(&target).unwrap().is_none());
        assert!(
            !glass_macos::private_session::account_processes(uid)
                .unwrap()
                .contains(&process)
        );
    }
}
