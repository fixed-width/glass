use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

use crate::adb::{Adb, build_argv};
use glass_core::{
    Deadline, GlassError, RecordedVideo, Recording, RecordingFormat, RecordingOptions, Result,
    WindowGeometry,
    recording_worker::{RecordingControl, start_recording_worker},
};

struct OwnedProcess(Child);
impl Drop for OwnedProcess {
    fn drop(&mut self) {
        if !matches!(self.0.try_wait(), Ok(Some(_))) {
            let _ = self.0.kill();
            let _ = self.0.wait();
        }
    }
}

pub(crate) fn start(
    adb: Adb,
    window: WindowGeometry,
    options: RecordingOptions,
    deadline: Deadline,
) -> Result<Box<dyn Recording>> {
    options.validate()?;
    if adb.serial().is_none() {
        return Err(GlassError::Backend(
            "recording requires a resolved Android device".into(),
        ));
    }
    start_recording_worker(options, deadline, move |control| {
        record(adb, window, control)
    })
}

fn record(adb: Adb, window: WindowGeometry, control: RecordingControl) -> Result<RecordedVideo> {
    let folder = format!("/data/local/tmp/glass-video-{}", control.options().id);
    let movie = format!("{folder}/clip.mp4");
    let pidfile = format!("{folder}/pid");
    let setup = format!("umask 077; mkdir {folder}");
    adb.run_until(["shell", setup.as_str()], Deadline::from_millis(3000))?;
    let outcome = if control.cancelled() {
        Err(GlassError::Backend("recording cancelled".into()))
    } else {
        record_in(&adb, &window, &control, &movie, &pidfile)
    };
    if outcome.is_err()
        && let Ok(pid) = adb.run_until(["shell", "cat", &pidfile], Deadline::from_millis(500))
        && let Ok(pid) = pid.trim().parse::<u32>()
        && pid > 1
    {
        let _ = stop_guest(&adb, pid, &movie, Deadline::from_millis(500));
    }
    let _ = adb.run_until(
        ["shell", "rm", "-f", movie.as_str(), pidfile.as_str()],
        Deadline::from_millis(500),
    );
    let _ = adb.run_until(
        ["shell", "rmdir", folder.as_str()],
        Deadline::from_millis(500),
    );
    outcome
}

fn record_in(
    adb: &Adb,
    window: &WindowGeometry,
    control: &RecordingControl,
    movie: &str,
    pidfile: &str,
) -> Result<RecordedVideo> {
    let scale = 1280.0 / f64::from(window.width.max(window.height).max(1));
    let scale = scale.min(1.0);
    let width = ((f64::from(window.width) * scale) as u32 / 2 * 2).max(2);
    let height = ((f64::from(window.height) * scale) as u32 / 2 * 2).max(2);
    let seconds = control.options().max_duration_ms.div_ceil(1000);
    // The identifier is validated hexadecimal; the resolved device, never caller shell text, owns the PID.
    let script = format!(
        "echo $$ > {pidfile}; exec screenrecord --size {width}x{height} --bit-rate 1500000 --time-limit {seconds} {movie}"
    );
    let mut command = Command::new(adb.bin());
    command
        .args(build_argv(adb.serial(), &["shell", &script]))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    #[cfg(unix)]
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = OwnedProcess(
        command
            .spawn()
            .map_err(|e| GlassError::Backend(format!("start Android screenrecord: {e}")))?,
    );
    let startup = Instant::now();
    let mut ready = false;
    let mut pid = None;
    let mut stopping = None;
    loop {
        if let Some(status) = child
            .0
            .try_wait()
            .map_err(|e| GlassError::Backend(format!("inspect screenrecord: {e}")))?
        {
            if !status.success() {
                return Err(GlassError::Backend(format!(
                    "Android screenrecord exited with {status}"
                )));
            }
            if stopping.is_none() {
                control.limit("duration_limit");
            }
            break;
        }
        if control.cancelled() {
            if let Some(pid) = pid {
                let _ = stop_guest(adb, pid, movie, Deadline::from_millis(500));
            }
            return Err(GlassError::Backend("recording cancelled".into()));
        }
        if pid.is_none() {
            pid = adb
                .run_until(["shell", "cat", pidfile], Deadline::from_millis(500))
                .ok()
                .and_then(|text| text.trim().parse::<u32>().ok())
                .filter(|pid| *pid > 1);
        }
        if let Ok(size) = adb.run_until(
            ["shell", "stat", "-c", "%s", movie],
            Deadline::from_millis(500),
        ) && let Ok(size) = size.trim().parse::<u64>()
        {
            control.observe_bytes(size);
            if size > 64 && pid.is_some() && !ready {
                control.ready();
                ready = true;
            }
        }
        if !ready && startup.elapsed() > Duration::from_secs(12) {
            if let Some(pid) = pid {
                let _ = stop_guest(adb, pid, movie, Deadline::from_millis(500));
            }
            return Err(GlassError::Backend(
                "Android screenrecord produced no video data".into(),
            ));
        }
        if control.should_stop() && stopping.is_none() {
            let pid = pid.ok_or_else(|| {
                GlassError::Backend("screenrecord did not disclose its owned PID".into())
            })?;
            stop_guest(adb, pid, movie, Deadline::from_millis(1000))?;
            stopping = Some(Instant::now());
        }
        if stopping.is_some_and(|start| start.elapsed() > Duration::from_secs(4)) {
            return Err(GlassError::Backend(
                "Android recorder did not finalize after SIGINT".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(100));
    }
    if !ready {
        return Err(GlassError::Backend(
            "Android recorder ended before video data".into(),
        ));
    }
    let size = adb
        .run_until(
            ["shell", "stat", "-c", "%s", movie],
            Deadline::from_millis(1000),
        )?
        .trim()
        .parse::<u64>()
        .map_err(|_| GlassError::Backend("invalid Android movie size".into()))?;
    control.observe_bytes(size);
    let mut data = Vec::new();
    if control.keep() && !control.cancelled() {
        if size > control.options().max_bytes {
            return Err(GlassError::Backend(
                "native movie exceeds recording byte limit".into(),
            ));
        }
        data = adb.run_bytes_until(["exec-out", "cat", movie], Deadline::from_millis(5000))?;
        if data.len() as u64 > control.options().max_bytes {
            return Err(GlassError::Backend(
                "movie grew beyond recording byte limit".into(),
            ));
        }
    }
    Ok(RecordedVideo {
        format: RecordingFormat::Mp4,
        progress: control.mark_finalized(),
        data,
    })
}

fn stop_guest(adb: &Adb, pid: u32, movie: &str, deadline: Deadline) -> Result<()> {
    // Refuse a reused PID: only this recording's command line authorizes its signal.
    let command = adb.run_until(["shell", "cat", &format!("/proc/{pid}/cmdline")], deadline)?;
    if !command.contains(movie) || !command.contains("screenrecord") {
        return Err(GlassError::Backend(
            "native recorder PID identity changed; signal refused".into(),
        ));
    }
    adb.run_until(["shell", "kill", "-2", &pid.to_string()], deadline)
        .map(|_| ())
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;
    use crate::adb::{Answer, FakeAdb};
    #[test]
    fn never_signals_a_pid_that_no_longer_names_this_movie() {
        let fake = FakeAdb::new(&[(
            "*",
            Answer::says("screenrecord\0/someone-elses-movie.mp4\0"),
        )]);
        assert!(
            stop_guest(
                fake.adb(),
                42,
                "/our-private-movie.mp4",
                Deadline::from_millis(1000)
            )
            .is_err()
        );
        assert!(!fake.called("kill"));
    }
    #[test]
    fn finalization_signals_only_the_identified_recorder() {
        let fake = FakeAdb::new(&[
            (
                "shell cat /proc/42/cmdline",
                Answer::says("screenrecord\0/our-private-movie.mp4\0"),
            ),
            ("shell kill -2 42", Answer::Silent),
        ]);
        stop_guest(
            fake.adb(),
            42,
            "/our-private-movie.mp4",
            Deadline::from_millis(1000),
        )
        .unwrap();
        assert!(fake.called("shell kill -2 42"));
        assert!(!fake.called("pkill"));
    }
}
