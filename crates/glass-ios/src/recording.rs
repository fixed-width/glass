use std::fs;
use std::io::Read;
use std::os::unix::fs::PermissionsExt;
use std::process::{Child, Command, Stdio};
use std::sync::{
    Arc,
    atomic::{AtomicBool, Ordering},
};
use std::time::{Duration, Instant};

use crate::simctl::Simctl;
use glass_core::{
    Deadline, GlassError, RecordedVideo, Recording, RecordingFormat, RecordingOptions, Result,
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

struct AcknowledgementReader {
    stop: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl Drop for AcknowledgementReader {
    fn drop(&mut self) {
        self.stop.store(true, Ordering::Release);
        if let Some(thread) = self.thread.take() {
            let _ = thread.join();
        }
    }
}

pub(crate) fn start(
    simctl: Simctl,
    udid: String,
    options: RecordingOptions,
    deadline: Deadline,
) -> Result<Box<dyn Recording>> {
    start_recording_worker(options, deadline, move |control| {
        record(simctl, udid, control)
    })
}

fn record(simctl: Simctl, udid: String, control: RecordingControl) -> Result<RecordedVideo> {
    let directory = tempfile::Builder::new()
        .prefix("glass-recording-")
        .permissions(fs::Permissions::from_mode(0o700))
        .tempdir()
        .map_err(|e| GlassError::Backend(format!("recording temporary directory: {e}")))?;
    let path = directory.path().join("clip.mov");
    let output = path
        .to_str()
        .ok_or_else(|| GlassError::Backend("recording path is not UTF-8".into()))?;
    let mut command = Command::new(simctl.program());
    command
        .args(simctl.full_args(&[
            "io",
            &udid,
            "recordVideo",
            "--codec=h264",
            "--mask=black",
            output,
        ]))
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::piped());
    std::os::unix::process::CommandExt::process_group(&mut command, 0);
    let mut child = OwnedProcess(
        command
            .spawn()
            .map_err(|e| GlassError::Backend(format!("start Simulator recorder: {e}")))?,
    );
    let first_frame = Arc::new(AtomicBool::new(false));
    let ready = first_frame.clone();
    let mut stderr = child
        .0
        .stderr
        .take()
        .ok_or_else(|| GlassError::Backend("recorder stderr unavailable".into()))?;
    let flags = rustix::fs::fcntl_getfl(&stderr).map_err(|e| GlassError::Backend(e.to_string()))?;
    rustix::fs::fcntl_setfl(&stderr, flags | rustix::fs::OFlags::NONBLOCK)
        .map_err(|e| GlassError::Backend(e.to_string()))?;
    let stop_reader = Arc::new(AtomicBool::new(false));
    let reader_stop = stop_reader.clone();
    let reader = std::thread::Builder::new()
        .name("glass-recording-ready".into())
        .spawn(move || {
            let mut buffer = [0_u8; 1024];
            let mut tail = Vec::new();
            while !reader_stop.load(Ordering::Acquire) {
                let count = match stderr.read(&mut buffer) {
                    Ok(count) => count,
                    Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => {
                        std::thread::sleep(Duration::from_millis(5));
                        continue;
                    }
                    Err(_) => break,
                };
                if count == 0 {
                    break;
                }
                tail.extend_from_slice(&buffer[..count]);
                if tail
                    .windows(b"Recording started".len())
                    .any(|s| s == b"Recording started")
                {
                    ready.store(true, Ordering::Release);
                }
                if tail.len() > 2048 {
                    tail.drain(..tail.len() - 1024);
                }
            }
        })
        .map_err(|e| GlassError::Backend(format!("recorder acknowledgement reader: {e}")))?;
    let reader = AcknowledgementReader {
        stop: stop_reader,
        thread: Some(reader),
    };
    let startup = Instant::now();
    let mut acknowledged = false;
    let mut stopping = None;
    loop {
        if let Some(status) = child
            .0
            .try_wait()
            .map_err(|e| GlassError::Backend(format!("inspect Simulator recorder: {e}")))?
        {
            if !status.success() {
                return Err(GlassError::Backend(format!(
                    "Simulator recorder exited with {status}"
                )));
            }
            break;
        }
        if control.cancelled() {
            return Err(GlassError::Backend("recording cancelled".into()));
        }
        if let Ok(metadata) = fs::metadata(&path) {
            control.observe_bytes(metadata.len());
        }
        if first_frame.load(Ordering::Acquire) && !acknowledged {
            control.ready();
            acknowledged = true;
        }
        if !acknowledged && startup.elapsed() > Duration::from_secs(12) {
            return Err(GlassError::Backend(
                "Simulator recorder acknowledged no video frame".into(),
            ));
        }
        if control.should_stop() && stopping.is_none() {
            let pid = rustix::process::Pid::from_raw(child.0.id() as i32)
                .ok_or_else(|| GlassError::Backend("invalid recorder process identity".into()))?;
            rustix::process::kill_process(pid, rustix::process::Signal::INT)
                .map_err(|e| GlassError::Backend(format!("stop Simulator recorder: {e}")))?;
            stopping = Some(Instant::now());
        }
        if stopping.is_some_and(|start| start.elapsed() > Duration::from_secs(4)) {
            return Err(GlassError::Backend(
                "Simulator recorder did not finalize after SIGINT".into(),
            ));
        }
        std::thread::sleep(Duration::from_millis(50));
    }
    drop(reader);
    if !acknowledged {
        return Err(GlassError::Backend(
            "Simulator recorder ended before video data".into(),
        ));
    }
    let final_size = fs::metadata(&path)
        .map_err(|e| GlassError::Backend(format!("inspect finalized recording: {e}")))?
        .len();
    control.observe_bytes(final_size);
    let mut data = Vec::new();
    if control.keep() && !control.cancelled() {
        let file = fs::File::open(&path)
            .map_err(|e| GlassError::Backend(format!("read recording: {e}")))?;
        if file
            .metadata()
            .map_err(|e| GlassError::Backend(e.to_string()))?
            .len()
            > control.options().max_bytes
        {
            return Err(GlassError::Backend(
                "native movie exceeds recording byte limit".into(),
            ));
        }
        file.take(control.options().max_bytes + 1)
            .read_to_end(&mut data)
            .map_err(|e| GlassError::Backend(format!("read recording: {e}")))?;
        if data.len() as u64 > control.options().max_bytes {
            return Err(GlassError::Backend(
                "movie grew beyond recording byte limit".into(),
            ));
        }
    }
    Ok(RecordedVideo {
        format: RecordingFormat::QuickTime,
        progress: control.mark_finalized(),
        data,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    #[test]
    fn native_process_acknowledges_a_frame_and_finalizes_on_sigint() {
        let root = tempfile::tempdir().unwrap();
        let program = root.path().join("simctl");
        let mut expected = Vec::new();
        for (kind, data) in [
            (b"ftyp", b"qt  ".as_slice()),
            (b"mdat", b"frame".as_slice()),
            (b"moov", b"index".as_slice()),
        ] {
            expected.extend(((data.len() + 8) as u32).to_be_bytes());
            expected.extend(kind);
            expected.extend(data);
        }
        fs::write(program.with_extension("mov"), &expected).unwrap();
        fs::write(
            &program,
            r#"#!/bin/sh
[ "$1" = --glass-probe ] && exit 0
[ "$#" -eq 7 ] || exit 64
[ "$1 $2 $3 $4 $5 $6" = 'simctl io owned-udid recordVideo --codec=h264 --mask=black' ] || exit 64
trap 'printf stopped > "$0.stopped"; exit 0' INT
cp "$0.mov" "$7" || exit
printf 'Recording started\n' >&2
remaining=1000
while [ "$remaining" -gt 0 ] && kill -0 "$PPID" 2>/dev/null; do
    sleep .01
    remaining=$((remaining - 1))
done
exit 70
"#,
        )
        .unwrap();
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        // Finish fixture setup before measuring the recorder, including transient exec races.
        let runnable_by = Instant::now() + Duration::from_secs(5);
        loop {
            match Command::new(&program).arg("--glass-probe").status() {
                Ok(status) => {
                    assert!(status.success());
                    break;
                }
                Err(error)
                    if error.kind() == std::io::ErrorKind::ExecutableFileBusy
                        && Instant::now() < runnable_by =>
                {
                    std::thread::sleep(Duration::from_millis(10));
                }
                Err(error) => panic!("recorder fixture is not executable: {error}"),
            }
        }
        let options = RecordingOptions {
            id: "123456789012345678901234567890ab".into(),
            max_duration_ms: 30_000,
            max_bytes: 8 * 1024 * 1024,
        };
        let mut handle = start(
            Simctl::at(program.to_string_lossy()),
            "owned-udid".into(),
            options,
            Deadline::from_millis(3000),
        )
        .unwrap();
        assert_eq!(
            handle.progress().state,
            glass_core::RecordingState::Recording
        );
        let video = handle.finish(true, Deadline::from_millis(3000)).unwrap();
        glass_core::recording::validate_video(&video.data).unwrap();
        assert_eq!(video.data, expected);
        assert_eq!(
            fs::read_to_string(program.with_extension("stopped")).unwrap(),
            "stopped"
        );
        assert_eq!(video.format, RecordingFormat::QuickTime);
        assert_eq!(video.progress.bytes, video.data.len() as u64);
    }
}
