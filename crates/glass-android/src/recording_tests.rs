use super::*;
use crate::adb::FakeAdb;
use std::path::Path;

const RECORDER: &str = r#"#!/usr/bin/env python3
import os, pathlib, sys, time
root = pathlib.Path(__file__).parent
args = sys.argv[1:]
with (root / 'calls').open('a') as log:
    log.write(' '.join(args) + '\n')
assert args[:2] == ['-s', 'emulator-test']
args = args[2:]
def read(name, default):
    path = root / name
    return path.read_text() if path.exists() else default
def write(name, value):
    temporary = root / (name + '.tmp')
    temporary.write_text(value)
    temporary.replace(root / name)
if args[0] == 'shell' and 'exec screenrecord' in args[-1]:
    write('native-pid', str(os.getpid()))
    write('movie', args[-1].split()[-1])
    until = time.monotonic() + 3
    while not (root / 'exit').exists() and time.monotonic() < until:
        time.sleep(.005)
    sys.exit(int(read('exit-code', '0')))
elif args[:2] == ['shell', 'cat']:
    if args[2].endswith('/pid'):
        until = time.monotonic() + 1
        while not (root / 'movie').exists() and time.monotonic() < until:
            time.sleep(.001)
        print(read('pid', '42'))
    else:
        sys.stdout.write('screenrecord\0' + read('movie', 'missing') + '\0')
elif args[:2] == ['shell', 'stat']:
    count = int(read('stat-count', '0'))
    write('stat-count', str(count + 1))
    sizes = read('sizes', '200').split(',')
    print(read('final-size', '200') if (root / 'exit').exists() else sizes[min(count, len(sizes)-1)])
    if count + 1 == int(read('exit-after-stat', '-1')):
        (root / 'exit').touch()
elif args[:2] == ['shell', 'kill']:
    if read('ignore-stop', 'false') == 'false':
        (root / 'exit').touch()
elif args[0] == 'exec-out':
    sys.stdout.buffer.write(b'x' * int(read('data-size', '200')))
"#;

struct Fixture(FakeAdb);
impl Fixture {
    fn new() -> Self {
        let fixture = Self(FakeAdb::new(&[]));
        std::fs::write(fixture.0.adb().bin(), RECORDER).unwrap();
        fixture
    }
    fn root(&self) -> &Path {
        Path::new(self.0.adb().bin()).parent().unwrap()
    }
    fn set(&self, name: &str, value: impl ToString) {
        std::fs::write(self.root().join(name), value.to_string()).unwrap();
    }
    fn start(
        &self,
        max_bytes: u64,
        clock: impl FnMut() -> Instant + Send + 'static,
    ) -> Result<Box<dyn Recording>> {
        let adb = self.0.adb().with_serial("emulator-test");
        start_recording_worker(
            RecordingOptions {
                id: "0123456789abcdef0123456789abcdef".into(),
                max_duration_ms: 2000,
                max_bytes,
            },
            Deadline::from_millis(2000),
            move |control| {
                record(
                    adb,
                    WindowGeometry {
                        width: 1080,
                        height: 2400,
                        ..Default::default()
                    },
                    control,
                    clock,
                )
            },
        )
    }
    fn native_is_alive(&self) -> bool {
        let pid = std::fs::read_to_string(self.root().join("native-pid")).unwrap();
        Command::new("kill")
            .args(["-0", pid.trim()])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .unwrap()
            .success()
    }
}

fn clock_at(times_ms: &[u64]) -> impl FnMut() -> Instant + Send + 'static {
    let origin = Instant::now();
    let mut times: std::collections::VecDeque<_> = times_ms.iter().copied().collect();
    let mut last = 0;
    move || {
        last = times.pop_front().unwrap_or(last);
        origin + Duration::from_millis(last)
    }
}

#[test]
fn dimensions_preserve_aspect_ratio_round_to_even_and_never_upscale() {
    for (width, height, expected) in [
        (1080, 2400, (576, 1280)),
        (2400, 1080, (1280, 576)),
        (321, 241, (320, 240)),
        (1280, 1280, (1280, 1280)),
        (0, 0, (2, 2)),
        (1, 1, (2, 2)),
    ] {
        assert_eq!(
            recording_dimensions(&WindowGeometry {
                width,
                height,
                ..Default::default()
            }),
            expected
        );
    }
}

#[test]
fn actual_video_data_and_an_owned_pid_are_required_before_start_returns() {
    let fixture = Fixture::new();
    fixture.set("pid", 2);
    fixture.set("sizes", "64,65");
    let mut recorder = fixture.start(300_000, clock_at(&[0, 11_999])).unwrap();
    let count: u32 = std::fs::read_to_string(fixture.root().join("stat-count"))
        .unwrap()
        .parse()
        .unwrap();
    assert!(count >= 2, "64 bytes are only a header, not captured video");
    let video = recorder.finish(true, Deadline::from_millis(2000)).unwrap();
    assert_eq!(video.data, vec![b'x'; 200]);
    let calls = fixture.0.calls();
    assert!(
        calls
            .iter()
            .all(|call| call.starts_with("-s emulator-test "))
    );
    assert!(calls.iter().any(|call| {
        call.contains("screenrecord --size 576x1280 --bit-rate 1500000 --time-limit 2 ")
    }));
    assert!(fixture.0.called("shell kill -2 2"));
    assert!(fixture.0.called("shell rm -f /data/local/tmp/glass-video-"));
    assert!(fixture.0.called("shell rmdir /data/local/tmp/glass-video-"));
    assert!(!fixture.native_is_alive());
}

#[test]
fn startup_deadline_and_invalid_pids_cleanup_without_signalling_unowned_processes() {
    for pid in [0, 1, 2] {
        let fixture = Fixture::new();
        fixture.set("pid", pid);
        fixture.set("sizes", 64);
        fixture.set("ignore-stop", true);
        let error = fixture
            .start(300_000, clock_at(&[0, 12_000]))
            .err()
            .unwrap();
        assert!(
            error.to_string().contains("produced no video data"),
            "{error}"
        );
        assert_eq!(fixture.0.called("shell kill -2"), pid == 2);
        assert!(fixture.0.called("shell rmdir"));
        assert!(!fixture.native_is_alive());
    }
    for pid in [0, 1] {
        let fixture = Fixture::new();
        fixture.set("pid", pid);
        fixture.set("sizes", 65);
        assert!(fixture.start(300_000, clock_at(&[0, 12_001])).is_err());
        assert!(!fixture.0.called("shell kill -2"));
    }
}

#[test]
fn discard_finalizes_without_downloading_the_movie_or_stopping_early() {
    let fixture = Fixture::new();
    let mut recorder = fixture.start(300_000, Instant::now).unwrap();
    let until = Instant::now() + Duration::from_secs(1);
    while fixture
        .0
        .calls()
        .iter()
        .filter(|call| call.contains("shell stat"))
        .count()
        < 2
        && Instant::now() < until
    {
        std::thread::sleep(Duration::from_millis(5));
    }
    assert!(
        !fixture.0.called("shell kill"),
        "capture must keep running until disposition"
    );
    assert!(fixture.native_is_alive());
    assert!(
        recorder
            .finish(false, Deadline::from_millis(2000))
            .unwrap()
            .data
            .is_empty()
    );
    assert!(!fixture.0.called("exec-out"));
    assert!(!fixture.native_is_alive());
}

#[test]
fn final_movie_stat_and_download_each_enforce_the_exact_byte_allowance() {
    for (stat, data, accepted, message) in [
        (300_000, 300_000, true, ""),
        (300_001, 200, false, "native movie exceeds"),
        (200, 300_001, false, "movie grew beyond"),
        (200, 300_000, true, ""),
    ] {
        let fixture = Fixture::new();
        fixture.set("final-size", stat);
        fixture.set("data-size", data);
        let mut recorder = fixture.start(300_000, Instant::now).unwrap();
        let result = recorder.finish(true, Deadline::from_millis(2000));
        assert_eq!(result.is_ok(), accepted);
        if accepted {
            assert_eq!(result.unwrap().data.len(), data);
        } else {
            assert!(result.err().unwrap().to_string().contains(message));
        }
        assert!(!fixture.native_is_alive());
    }
}

#[test]
fn a_recorder_that_ignores_sigint_hits_the_finalization_deadline_and_is_reaped() {
    let fixture = Fixture::new();
    fixture.set("ignore-stop", true);
    let mut recorder = fixture.start(1, clock_at(&[0, 0, 4000])).unwrap();
    let error = recorder
        .finish(true, Deadline::from_millis(2000))
        .err()
        .unwrap();
    assert!(
        error.to_string().contains("did not finalize after SIGINT"),
        "{error}"
    );
    assert!(!fixture.native_is_alive());
    assert!(fixture.0.called("shell rmdir"));
}

#[test]
fn native_exit_before_data_is_a_failure_and_after_data_reports_a_limit() {
    for size in [64, 65] {
        let fixture = Fixture::new();
        fixture.set("sizes", size);
        fixture.set("exit-after-stat", 1);
        let result = fixture.start(300_000, clock_at(&[0, 0]));
        if size == 64 {
            assert!(
                result
                    .err()
                    .unwrap()
                    .to_string()
                    .contains("ended before video data")
            );
        } else {
            let mut recorder = result.unwrap();
            let until = Instant::now() + Duration::from_secs(1);
            while recorder.progress().state == glass_core::RecordingState::Recording
                && Instant::now() < until
            {
                std::thread::sleep(Duration::from_millis(5));
            }
            let video = recorder.finish(true, Deadline::from_millis(2000)).unwrap();
            assert_eq!(video.progress.reason.as_deref(), Some("duration_limit"));
        }
        assert!(!fixture.native_is_alive());
    }
}

#[test]
fn dropping_a_live_handle_cancels_the_recorder_and_removes_its_guest_files() {
    let fixture = Fixture::new();
    let recorder = fixture.start(300_000, Instant::now).unwrap();
    drop(recorder);
    assert!(fixture.0.wait_called("shell rmdir", Duration::from_secs(2)));
    assert!(!fixture.native_is_alive());
    assert!(!fixture.0.called("exec-out"));
}
