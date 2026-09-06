use super::format::EventKind;
use super::*;
use serde_json::json;

pub(super) fn start_recorder(root: &tempfile::TempDir) -> TraceRecorder {
    TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap()
}

#[cfg(windows)]
#[tokio::test]
async fn windows_private_files_junction_refusal_and_case_alias_overlap() {
    let root = private_root();
    let outside = private_root();
    let junction = root.path().join("redirected");
    assert!(
        std::process::Command::new("cmd.exe")
            .args(["/c", "mklink", "/J"])
            .arg(&junction)
            .arg(outside.path())
            .output()
            .unwrap()
            .status
            .success()
    );
    let config = TraceConfig::new(junction, None).unwrap();
    assert!(TraceRecorder::start(&config, crate::tool_profile::ToolProfile::Full, "test").is_err());
    assert_eq!(std::fs::read_dir(outside.path()).unwrap().count(), 0);
    let recorder = start_recorder(&root);
    recorder.close().await;
    for directory in [recorder.path().to_owned(), recorder.path().join("blobs")] {
        for entry in std::fs::read_dir(directory).unwrap() {
            let entry = entry.unwrap();
            if entry.file_type().unwrap().is_file() {
                let file = std::fs::File::open(entry.path()).unwrap();
                assert!(glass_windows::file_is_private_to_current_user(&file).unwrap());
            }
        }
    }
    let alias =
        std::path::PathBuf::from(recorder.path().to_str().unwrap().to_uppercase()).join("bad.zip");
    assert!(export(recorder.path(), &alias).is_err());
    assert!(!alias.exists());
}

#[tokio::test]
async fn shutdown_failure_and_hard_linked_evidence_are_not_complete() {
    let root = private_root();
    let recorder = start_recorder(&root);
    recorder.shutdown_event("timed_out");
    recorder.close().await;
    assert_eq!(inspect(recorder.path()).unwrap().exit_code(), 2);
    std::fs::hard_link(
        recorder.path().join("manifest.json"),
        root.path().join("external-manifest"),
    )
    .unwrap();
    assert!(inspect(recorder.path()).is_err());
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn shutdown_waits_for_admitted_capture_before_closing_the_journal() {
    let root = private_root();
    let recorder = start_recorder(&root);
    recorder.idle().await;
    let (entered_tx, entered) = std::sync::mpsc::sync_channel(1);
    let (release, release_rx) = std::sync::mpsc::sync_channel(1);
    let producer = recorder.clone();
    let thread = std::thread::spawn(move || {
        producer.record(
            EventKind::Unknown("capture_in_progress".into()),
            None,
            0,
            json!({}),
            |capture| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                capture.bytes(
                    super::recorder::evidence("late", "text/plain", "glass", None),
                    b"completed capture",
                );
            },
        )
    });
    entered
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    let closing = recorder.clone();
    let closed = tokio::spawn(async move { closing.close().await });
    tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    assert!(
        !closed.is_finished(),
        "writer closed ahead of admitted evidence"
    );
    release.send(()).unwrap();
    thread.join().unwrap();
    closed.await.unwrap();
    let report = inspect(recorder.path()).unwrap();
    assert!(report.complete);
    assert!(
        report
            .events
            .iter()
            .any(|event| event["kind"] == "capture_in_progress")
    );
}

#[tokio::test]
async fn queue_and_call_limits_preserve_inspectable_prefixes() {
    for queue_limit in [true, false] {
        let root = private_root();
        let recorder = start_recorder(&root);
        recorder.idle().await;
        if queue_limit {
            let (entered, release) = recorder.pause_next_write();
            recorder.record(
                EventKind::Unknown("queue_probe".into()),
                None,
                0,
                json!({}),
                |_| {},
            );
            entered
                .recv_timeout(std::time::Duration::from_secs(2))
                .unwrap();
            for _ in 0..super::config::MAX_PENDING_EVENTS + 2 {
                recorder.record(
                    EventKind::Unknown("queue_probe".into()),
                    None,
                    0,
                    json!({}),
                    |_| {},
                );
            }
            assert_eq!(recorder.status()["state"], "limited");
            release.send(()).unwrap();
        } else {
            recorder.set_calls_at_limit();
            assert!(recorder.begin_call("glass_type", 1).is_none());
        }
        recorder.close().await;
        let report = inspect(recorder.path()).unwrap();
        assert_eq!(report.exit_code(), 2);
        assert_eq!(report.manifest["state"], "limited");
        assert!(report.manifest["omissions"].as_u64().unwrap() > 0);
    }
}

#[tokio::test]
async fn concurrent_and_cancelled_close_wait_for_the_writer() {
    use std::future::Future;
    use std::task::{Context, Waker};

    let root = private_root();
    let recorder = start_recorder(&root);
    recorder.idle().await;
    let (entered, release) = recorder.pause_next_write();
    recorder.record(
        EventKind::Unknown("close_probe".into()),
        None,
        0,
        json!({}),
        |_| {},
    );
    entered
        .recv_timeout(std::time::Duration::from_secs(5))
        .unwrap();

    let mut first = Box::pin(recorder.close());
    let mut second = Box::pin(recorder.close());
    let mut context = Context::from_waker(Waker::noop());
    assert!(first.as_mut().poll(&mut context).is_pending());
    assert!(second.as_mut().poll(&mut context).is_pending());
    drop(first);
    assert!(inspect(recorder.path()).is_err());

    release.send(()).unwrap();
    second.await;
    assert!(inspect(recorder.path()).unwrap().complete);
    recorder.close().await;
}

#[tokio::test]
async fn stalled_writer_shutdown_is_bounded_and_never_claims_completeness() {
    let root = private_root();
    let recorder = start_recorder(&root);
    recorder.idle().await;
    let (entered, release) = recorder.pause_next_write();
    recorder.record(
        EventKind::Unknown("stall_probe".into()),
        None,
        0,
        json!({}),
        |_| {},
    );
    entered
        .recv_timeout(std::time::Duration::from_secs(2))
        .unwrap();
    tokio::time::pause();
    tokio::time::timeout(std::time::Duration::from_secs(3), recorder.close())
        .await
        .unwrap();
    tokio::time::resume();
    assert_eq!(recorder.status()["state"], "failed");
    release.send(()).unwrap();
    recorder.writer_finished().await;
    let report = inspect(recorder.path()).unwrap();
    assert_eq!(report.exit_code(), 2);
    assert_eq!(report.manifest["state"], "failed");
}

#[tokio::test]
async fn retained_trace_exports_after_writer_stops() {
    let root = private_root();
    let config = TraceConfig::new(root.path().to_owned(), None).unwrap();
    let recorder =
        TraceRecorder::start(&config, crate::tool_profile::ToolProfile::Full, "test").unwrap();
    let call = recorder.begin_call("glass_type", 1).unwrap();
    call.arguments(&json!({"text": "hello"}));
    call.record(EventKind::LogicalOutcome, json!({"is_error": false}));
    assert!(inspect(recorder.path()).is_err());
    recorder.close().await;
    let report = inspect(recorder.path()).unwrap();
    assert!(report.complete, "{report:?}");
    let output = root.path().join("trace.zip");
    assert!(export(recorder.path(), &output).unwrap().complete);
    let mut archive = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
    assert!(archive.by_name("events.jsonl").is_ok());
    assert!(archive.by_name("READING.txt").is_ok());
}

#[tokio::test]
async fn missing_committed_evidence_refuses_export() {
    let root = private_root();
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Lean,
        "test",
    )
    .unwrap();
    recorder.close().await;
    let blob = std::fs::read_dir(recorder.path().join("blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    std::fs::write(blob, b"corrupt").unwrap();
    let out = root.path().join("bad.zip");
    assert!(export(recorder.path(), &out).is_err());
    assert!(!out.exists());
}

pub(super) fn private_root() -> tempfile::TempDir {
    let root = tempfile::tempdir_in(std::env::temp_dir().canonicalize().unwrap()).unwrap();
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
    }
    #[cfg(windows)]
    {
        use std::os::windows::fs::OpenOptionsExt;
        use windows::Win32::Storage::FileSystem::{FILE_ALL_ACCESS, FILE_FLAG_BACKUP_SEMANTICS};
        let file = std::fs::OpenOptions::new()
            .access_mode(FILE_ALL_ACCESS.0)
            .custom_flags(FILE_FLAG_BACKUP_SEMANTICS.0)
            .open(root.path())
            .unwrap();
        glass_windows::restrict_file_to_current_user(&file).unwrap();
    }
    root
}

#[tokio::test]
async fn total_limit_preserves_prefix_and_exports_an_incomplete_bundle() {
    let root = private_root();
    let limit = super::config::MIN_MAX_BYTES;
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), Some(limit)).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap();
    recorder.idle().await;
    let call = recorder.begin_call("glass_screenshot", 1).unwrap();
    call.arguments(&json!({}));
    call.capture(
        EventKind::LogicalOutcome,
        json!({"is_error": false}),
        |capture| {
            capture.bytes(
                super::recorder::evidence("image", "image/webp", "untrusted_application", Some(0)),
                &vec![3; limit as usize],
            );
        },
    );
    recorder.idle().await;
    assert_eq!(recorder.status()["state"], "limited");
    recorder.close().await;
    let report = inspect(recorder.path()).unwrap();
    assert!(!report.complete);
    assert_eq!(report.exit_code(), 2);
    assert!(report.events.iter().any(|e| e["kind"] == "call_received"));
    assert!(
        report
            .events
            .iter()
            .flat_map(|e| e["evidence"].as_array().into_iter().flatten())
            .any(|e| e["omitted"] == "total_byte_limit")
    );
    let total: u64 = [recorder.path().to_owned(), recorder.path().join("blobs")]
        .iter()
        .flat_map(|dir| std::fs::read_dir(dir).unwrap())
        .map(|entry| entry.unwrap().metadata().unwrap())
        .filter(|meta| meta.is_file())
        .map(|meta| meta.len())
        .sum();
    assert!(total <= limit, "stored {total}, cap {limit}");
    assert_eq!(
        export(recorder.path(), &root.path().join("limited.zip"))
            .unwrap()
            .exit_code(),
        2
    );
}

#[tokio::test]
async fn oversized_payload_is_omitted_whole_and_later_small_evidence_survives() {
    let root = private_root();
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap();
    let call = recorder.begin_call("glass_test", 1).unwrap();
    call.arguments(&json!({}));
    call.capture(
        EventKind::LogicalOutcome,
        json!({"is_error": false}),
        |capture| {
            capture.bytes(
                super::recorder::evidence("large", "text/plain", "untrusted_application", Some(0)),
                &vec![b'x'; super::config::MAX_PAYLOAD_BYTES + 1],
            );
            capture.bytes(
                super::recorder::evidence("small", "text/plain", "untrusted_application", Some(1)),
                b"later evidence",
            );
        },
    );
    recorder.close().await;
    let report = inspect(recorder.path()).unwrap();
    assert_eq!(report.exit_code(), 2);
    let event = report
        .events
        .iter()
        .find(|e| e["kind"] == "logical_outcome")
        .unwrap();
    assert_eq!(event["evidence"][0]["omitted"], "payload_limit");
    assert!(event["evidence"][0].get("payload").is_none());
    assert_eq!(
        std::fs::read(
            recorder
                .path()
                .join(event["evidence"][1]["payload"]["path"].as_str().unwrap())
        )
        .unwrap(),
        b"later evidence"
    );
}

#[tokio::test]
async fn writer_failure_keeps_an_inspectable_interrupted_prefix() {
    let root = private_root();
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap();
    recorder.idle().await;
    recorder.fail_next_write();
    let call = recorder.begin_call("glass_test", 1).unwrap();
    call.arguments(&json!({}));
    recorder.idle().await;
    assert_eq!(recorder.status()["state"], "failed");
    recorder.close().await;
    let report = inspect(recorder.path()).unwrap();
    assert!(!report.complete);
    assert!(report.manifest["errors"].as_u64().unwrap() > 0);
    assert_eq!(
        export(recorder.path(), &root.path().join("failed.zip"))
            .unwrap()
            .exit_code(),
        2
    );
}

#[tokio::test]
async fn interrupted_tail_recovers_but_tampered_committed_journal_refuses() {
    let root = private_root();
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap();
    recorder.close().await;
    let path = recorder.path();
    let manifest_path = path.join("manifest.json");
    let mut manifest: serde_json::Value =
        serde_json::from_slice(&std::fs::read(&manifest_path).unwrap()).unwrap();
    manifest["complete"] = json!(false);
    manifest["finalization"] = serde_json::Value::Null;
    std::fs::write(manifest_path, serde_json::to_vec(&manifest).unwrap()).unwrap();
    use std::io::Write;
    std::fs::OpenOptions::new()
        .append(true)
        .open(path.join("events.jsonl"))
        .unwrap()
        .write_all(b"{torn")
        .unwrap();
    let report = inspect(path).unwrap();
    assert_eq!(report.uncommitted_tail_bytes, 5);
    assert_eq!(report.exit_code(), 2);
    assert_eq!(
        export(path, &root.path().join("recovered.zip"))
            .unwrap()
            .exit_code(),
        2
    );
    let mut journal = std::fs::read(path.join("events.jsonl")).unwrap();
    journal[0] = b'!';
    std::fs::write(path.join("events.jsonl"), journal).unwrap();
    assert!(inspect(path).is_err());
}

#[cfg(unix)]
#[tokio::test]
async fn symlinked_root_and_payload_are_refused_without_following_them() {
    use std::os::unix::fs::symlink;
    let root = private_root();
    let other = private_root();
    symlink(other.path(), root.path().join("redirected")).unwrap();
    let config = TraceConfig::new(root.path().join("redirected"), None).unwrap();
    assert!(TraceRecorder::start(&config, crate::tool_profile::ToolProfile::Full, "test").is_err());
    assert_eq!(std::fs::read_dir(other.path()).unwrap().count(), 0);
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap();
    recorder.close().await;
    let blob = std::fs::read_dir(recorder.path().join("blobs"))
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let outside = other.path().join("outside");
    std::fs::rename(&blob, &outside).unwrap();
    symlink(&outside, &blob).unwrap();
    assert!(inspect(recorder.path()).is_err());
}

#[tokio::test]
async fn export_refuses_existing_destinations_and_source_overlap() {
    let root = private_root();
    let recorder = TraceRecorder::start(
        &TraceConfig::new(root.path().to_owned(), None).unwrap(),
        crate::tool_profile::ToolProfile::Full,
        "test",
    )
    .unwrap();
    recorder.close().await;
    let existing = root.path().join("existing.zip");
    std::fs::write(&existing, b"keep").unwrap();
    assert!(export(recorder.path(), &existing).is_err());
    assert_eq!(std::fs::read(existing).unwrap(), b"keep");
    assert!(export(recorder.path(), &recorder.path().join("overlap.zip")).is_err());
}

#[test]
fn trace_cli_bounds_and_offline_commands_parse_without_creating_storage() {
    use crate::cli::{Cli, Command, TraceCommand};
    use clap::Parser;
    let root = private_root();
    let path = root.path().to_str().unwrap();
    assert!(
        Cli::try_parse_from(["glass-mcp"])
            .unwrap()
            .trace_dir
            .is_none()
    );
    assert!(Cli::try_parse_from(["glass-mcp", "--trace-max-bytes", "1048576"]).is_err());
    for invalid in ["0", "1048575", "2147483649"] {
        assert!(
            Cli::try_parse_from([
                "glass-mcp",
                "--trace-dir",
                path,
                "--trace-max-bytes",
                invalid
            ])
            .is_err()
        );
    }
    for valid in ["1048576", "2147483648"] {
        assert!(
            Cli::try_parse_from([
                "glass-mcp",
                "serve",
                "--http",
                "--trace-dir",
                path,
                "--trace-max-bytes",
                valid
            ])
            .is_ok()
        );
    }
    let cli = Cli::try_parse_from(["glass-mcp", "trace", "inspect", path, "--json"]).unwrap();
    assert!(matches!(
        cli.command,
        Some(Command::Trace {
            command: TraceCommand::Inspect { json: true, .. }
        })
    ));
    assert_eq!(std::fs::read_dir(root.path()).unwrap().count(), 0);
}
