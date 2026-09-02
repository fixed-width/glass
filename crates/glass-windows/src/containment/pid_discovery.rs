use std::process::Command;
use std::time::Duration;

use glass_core::{Deadline, GlassError, Result};

use super::config;

#[cfg_attr(not(windows), allow(dead_code))]
const LIST_PIDS_BUDGET: Duration = Duration::from_secs(5);
const DISCOVERY_OP: &str = "Windows accessibility process discovery";
const LIST_PIDS_OP: &str = "Sandboxie process discovery";

fn require_time(deadline: Deadline, started: bool, operation: &str) -> Result<()> {
    if !deadline.has_passed() {
        return Ok(());
    }
    if started {
        Err(GlassError::caller_deadline_elapsed(operation))
    } else {
        Err(GlassError::deadline_not_started(operation))
    }
}

pub(super) fn collect_pids_by_with(
    deadline: Deadline,
    primary: impl FnOnce(Deadline) -> Result<Vec<u32>>,
    descendants: impl FnOnce(Deadline) -> Result<Vec<u32>>,
) -> Result<Vec<u32>> {
    require_time(deadline, false, DISCOVERY_OP)?;
    let mut pids = primary(deadline)?;
    require_time(deadline, true, DISCOVERY_OP)?;
    for pid in descendants(deadline)? {
        if !pids.contains(&pid) {
            pids.push(pid);
        }
    }
    require_time(deadline, true, DISCOVERY_OP)?;
    Ok(pids)
}

#[cfg_attr(not(windows), allow(dead_code))]
pub(super) fn sandboxie_list_pids_by(
    executable: &str,
    box_name: &str,
    deadline: Deadline,
) -> Result<Vec<u32>> {
    let mut command = Command::new(executable);
    command.args([&format!("/box:{box_name}"), "/listpids"]);
    list_pids_command_by_with_budget(&mut command, deadline, LIST_PIDS_BUDGET)
}

fn list_pids_command_by_with_budget(
    command: &mut Command,
    deadline: Deadline,
    budget: Duration,
) -> Result<Vec<u32>> {
    list_pids_by_with(deadline, |received| {
        let output = glass_core::run_bounded_until(command, budget, received, LIST_PIDS_OP)?;
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    })
}

fn list_pids_by_with(
    deadline: Deadline,
    run: impl FnOnce(Deadline) -> Result<String>,
) -> Result<Vec<u32>> {
    require_time(deadline, false, LIST_PIDS_OP)?;
    let output = run(deadline)?;
    require_time(deadline, true, LIST_PIDS_OP)?;
    Ok(config::parse_listpids(&output))
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;
    use std::process::Command;
    use std::time::{Duration, Instant};

    use glass_core::{BoundDispatch, BoundKind, Deadline, Whose};

    use super::{collect_pids_by_with, list_pids_by_with, list_pids_command_by_with_budget};

    #[test]
    fn launched_pid_collection_threads_one_deadline_and_deduplicates_sources() {
        let deadline = Deadline::from_millis(1_000);
        let primary_deadline = Cell::new(None);
        let descendant_deadline = Cell::new(None);

        let pids = collect_pids_by_with(
            deadline,
            |received| {
                primary_deadline.set(Some(received));
                Ok(vec![1234, 5678])
            },
            |received| {
                descendant_deadline.set(Some(received));
                Ok(vec![5678, 9012])
            },
        )
        .unwrap();

        assert_eq!(primary_deadline.get(), Some(deadline));
        assert_eq!(descendant_deadline.get(), Some(deadline));
        assert_eq!(pids, [1234, 5678, 9012]);
    }

    #[test]
    fn a_late_primary_pid_scan_does_not_start_descendant_discovery() {
        let descendants_started = Cell::new(false);
        let deadline = Deadline::from_millis(10);

        let error = collect_pids_by_with(
            deadline,
            |_| {
                std::thread::sleep(Duration::from_millis(30));
                Ok(vec![1234])
            },
            |_| {
                descendants_started.set(true);
                Ok(vec![5678])
            },
        )
        .expect_err("late primary PID results must not start another discovery phase");

        assert!(!descendants_started.get());
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn the_exact_absolute_deadline_reaches_the_listpids_runner() {
        let deadline = Deadline::from_millis(1_000);
        let seen = Cell::new(None);

        let pids = list_pids_by_with(deadline, |received| {
            seen.set(Some(received));
            Ok("2\r\n1234\r\n5678\r\n".to_string())
        })
        .unwrap();

        assert_eq!(seen.get(), Some(deadline));
        assert_eq!(pids, [1234, 5678]);
    }

    #[test]
    fn a_success_returned_after_the_deadline_is_rejected() {
        let deadline = Deadline::from_millis(10);

        let error = list_pids_by_with(deadline, |_| {
            std::thread::sleep(Duration::from_millis(30));
            Ok("1\r\n1234\r\n".to_string())
        })
        .expect_err("late listpids output must not become semantic context");

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn a_spent_deadline_starts_no_listpids_child() {
        let error = list_pids_command_by_with_budget(
            &mut Command::new("glass-nonexistent-listpids-helper"),
            Deadline::at(Instant::now()),
            Duration::from_secs(1),
        )
        .expect_err("a spent deadline must be rejected before spawn");

        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    #[cfg(unix)]
    fn a_wedged_listpids_child_is_killed_and_reaped_at_the_caller_deadline() {
        let pid_file = std::env::temp_dir().join(format!(
            "glass-listpids-wedge-{}-{}",
            std::process::id(),
            Instant::now().elapsed().as_nanos()
        ));
        let mut command = Command::new("/bin/sh");
        command
            .args(["-c", "echo $$ > \"$PID_FILE\"; exec sleep 30"])
            .env("PID_FILE", &pid_file);
        let started = Instant::now();

        let error = list_pids_command_by_with_budget(
            &mut command,
            Deadline::from_millis(100),
            Duration::from_secs(5),
        )
        .expect_err("a wedged Start.exe seam must stop at the shared deadline");

        assert!(started.elapsed() < Duration::from_secs(2));
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("the helper wrote its pid before wedging")
            .trim()
            .parse()
            .unwrap();
        assert!(
            !std::path::Path::new(&format!("/proc/{pid}")).exists(),
            "the timed-out listpids child {pid} was not reaped"
        );
        let _ = std::fs::remove_file(pid_file);
    }

    #[test]
    #[cfg(unix)]
    fn the_listpids_own_ceiling_keeps_callee_ownership() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "exec sleep 30"]);

        let error = list_pids_command_by_with_budget(
            &mut command,
            Deadline::UNBOUNDED,
            Duration::from_millis(50),
        )
        .expect_err("the Start.exe seam must also have its own bounded ceiling");

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Callee));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    #[cfg(unix)]
    fn listpids_output_completed_with_time_remaining_is_preserved() {
        let mut command = Command::new("/bin/sh");
        command.args(["-c", "printf '2\\n1234\\n5678\\n'"]);
        let deadline = Deadline::from_millis(5_000);

        let pids = list_pids_command_by_with_budget(&mut command, deadline, Duration::from_secs(5))
            .expect("output completed with caller time remaining");

        assert_eq!(pids, [1234, 5678]);
    }

    #[test]
    #[ignore = "subprocess helper for the Windows-target listpids deadline tests"]
    #[cfg(windows)]
    fn listpids_child() {
        let Ok(delay_ms) = std::env::var("GLASS_LISTPIDS_TEST_DELAY_MS") else {
            return;
        };
        if let Ok(path) = std::env::var("GLASS_LISTPIDS_TEST_PID_FILE") {
            let pending = format!("{path}.pending");
            std::fs::write(&pending, std::process::id().to_string()).unwrap();
            std::fs::rename(pending, path).unwrap();
        }
        std::thread::sleep(Duration::from_millis(delay_ms.parse().unwrap()));
        if std::env::var_os("GLASS_LISTPIDS_TEST_OUTPUT").is_some() {
            println!("2\r\n1234\r\n5678");
        }
    }

    #[cfg(windows)]
    fn windows_helper(delay_ms: u64, output: bool, pid_file: Option<&std::path::Path>) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command.args([
            "--exact",
            "containment::pid_discovery::tests::listpids_child",
            "--ignored",
            "--nocapture",
        ]);
        command.env("GLASS_LISTPIDS_TEST_DELAY_MS", delay_ms.to_string());
        if output {
            command.env("GLASS_LISTPIDS_TEST_OUTPUT", "1");
        }
        if let Some(path) = pid_file {
            command.env("GLASS_LISTPIDS_TEST_PID_FILE", path);
        }
        command
    }

    #[test]
    #[cfg(windows)]
    fn windows_production_runner_kills_a_wedged_listpids_child() {
        let temp = tempfile::tempdir().unwrap();
        let pid_file = temp.path().join("pid");
        let mut command = windows_helper(30_000, false, Some(&pid_file));

        let error = list_pids_command_by_with_budget(
            &mut command,
            Deadline::from_millis(2_000),
            Duration::from_secs(5),
        )
        .expect_err("the Windows-target command runner must kill a wedged child");

        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(
            error.bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
        let pid: u32 = std::fs::read_to_string(&pid_file)
            .expect("the helper wrote its pid before wedging")
            .trim()
            .parse()
            .unwrap();
        // SAFETY: OpenProcess receives a numeric PID written by the child. Any returned handle is
        // closed immediately and is used only to ask whether the process is still active.
        let still_active = unsafe {
            use windows::Win32::Foundation::{CloseHandle, STILL_ACTIVE};
            use windows::Win32::System::Threading::{
                GetExitCodeProcess, OpenProcess, PROCESS_QUERY_LIMITED_INFORMATION,
            };
            match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
                Ok(handle) => {
                    let mut exit_code = 0;
                    let active = GetExitCodeProcess(handle, &mut exit_code).is_ok()
                        && exit_code == STILL_ACTIVE.0 as u32;
                    let _ = CloseHandle(handle);
                    active
                }
                Err(_) => false,
            }
        };
        assert!(
            !still_active,
            "the timed-out listpids child {pid} is still active"
        );
    }

    #[test]
    #[cfg(windows)]
    fn windows_listpids_output_completed_with_time_remaining_is_preserved() {
        let mut command = windows_helper(0, true, None);

        let pids = list_pids_command_by_with_budget(
            &mut command,
            Deadline::from_millis(5_000),
            Duration::from_secs(5),
        )
        .expect("the Windows-target helper completed with caller time remaining");

        // Libtest writes progress around the helper's payload.
        assert!(pids.contains(&1234), "missing first payload PID: {pids:?}");
        assert!(pids.contains(&5678), "missing second payload PID: {pids:?}");
    }
}
