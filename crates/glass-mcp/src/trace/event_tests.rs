use std::io::Read;
use std::path::Path;

use serde_json::{Value, json};

use crate::output::{TargetAccess, ToolEffect, ToolOutput};
use crate::output_policy::ToolCallOutcome;

use super::format::{CallRole, Event, EventKind, digest};
use super::tests::{private_root, start_recorder};
use super::{RequestGuard, export, inspect};

#[test]
fn event_kinds_preserve_v1_strings_and_call_roles() {
    for (kind, name, role) in [
        (EventKind::Inventory, "inventory", CallRole::Other),
        (EventKind::ClientCreated, "client_created", CallRole::Other),
        (EventKind::Shutdown, "shutdown", CallRole::Other),
        (EventKind::CallReceived, "call_received", CallRole::Start),
        (EventKind::ArgumentSize, "argument_size", CallRole::Other),
        (EventKind::Arguments, "arguments", CallRole::Other),
        (
            EventKind::ExecutionStarted,
            "execution_started",
            CallRole::Other,
        ),
        (
            EventKind::SessionContext,
            "session_context",
            CallRole::Other,
        ),
        (
            EventKind::LogicalOutcome,
            "logical_outcome",
            CallRole::Outcome,
        ),
        (
            EventKind::ResponseConstructed,
            "response_constructed",
            CallRole::Other,
        ),
        (
            EventKind::RequestAbandoned,
            "request_abandoned",
            CallRole::Other,
        ),
        (
            EventKind::RouterRejection,
            "router_rejection",
            CallRole::Outcome,
        ),
        (
            EventKind::WorkerUnavailable,
            "worker_unavailable",
            CallRole::Outcome,
        ),
        (EventKind::ResourceRead, "resource_read", CallRole::Other),
        (EventKind::TraceClosed, "trace_closed", CallRole::Other),
        (
            EventKind::Unknown("future_event".into()),
            "future_event",
            CallRole::Other,
        ),
    ] {
        let wire = format!(
            r#"{{"seq":1,"elapsed_us":2,"kind":"{name}","call":3,"client":4,"data":{{}}}}"#
        );
        let event: Event = serde_json::from_str(&wire).unwrap();
        assert_eq!(event.kind, kind);
        assert_eq!(event.kind.call_role(), role, "{name}");
        assert_eq!(serde_json::to_string(&event).unwrap(), wire);
    }
}

#[test]
fn v1_event_kind_rejects_non_strings() {
    for kind in [
        Value::Null,
        json!(1),
        json!(true),
        json!([]),
        json!({"call_received": null}),
    ] {
        let event = json!({"seq":1, "elapsed_us":2, "kind":kind, "call":3, "client":4, "data":{}});
        assert!(serde_json::from_value::<Event>(event).is_err(), "{kind}");
    }
}

#[tokio::test]
async fn call_events_preserve_payloads_and_finish_each_execution_path() {
    let root = private_root();
    let recorder = start_recorder(&root);
    let client = recorder.new_client();
    let call = recorder.begin_call("glass_test", client).unwrap();
    call.argument_size(&json!({}));
    call.arguments(&json!({}));
    call.execution_started(7, None, None);
    call.session_context(Some(7), Some("x11"));
    let outcome = ToolCallOutcome {
        tool: "glass_test",
        effect: ToolEffect::ReadOnly,
        is_error: false,
        target_access: TargetAccess::NoActiveTarget,
        output: ToolOutput(vec![]),
    };
    call.logical_outcome(&outcome);
    call.response_constructed(&outcome.output, false, outcome.target_access, None);
    let rejected = recorder.begin_call("glass_test", client).unwrap();
    rejected.router_rejection();
    let unavailable = recorder.begin_call("glass_test", client).unwrap();
    unavailable.worker_unavailable();
    unavailable.response_constructed(&outcome.output, true, outcome.target_access, None);
    let resource = recorder.begin_call("resources/read", client).unwrap();
    resource.resource_read(&Ok(rmcp::model::ReadResourceResult::new(vec![])));
    recorder.close().await;

    let report = inspect(recorder.path()).unwrap();
    assert!(report.complete, "{report:?}");
    assert_eq!(report.manifest["calls"], 4);
    let call_events: Vec<_> = report
        .events
        .iter()
        .filter(|event| !event["call"].is_null())
        .map(|event| {
            assert_eq!(event["client"], client);
            json!({"call":event["call"], "kind":event["kind"], "data":event["data"]})
        })
        .collect();
    let output = json!({"is_error":false, "target_access":"no_active_target", "content_blocks":0, "client_delivery":"unknown"});
    assert_eq!(
        call_events,
        vec![
            json!({"call":1, "kind":"call_received", "data":{"tool":"glass_test"}}),
            json!({"call":1, "kind":"argument_size", "data":{"compact_json_bytes":2}}),
            json!({"call":1, "kind":"arguments", "data":{}}),
            json!({"call":1, "kind":"execution_started", "data":{"execution_order":7, "session":null, "backend":null}}),
            json!({"call":1, "kind":"session_context", "data":{"session":7, "backend":"x11"}}),
            json!({"call":1, "kind":"logical_outcome", "data":output}),
            json!({"call":1, "kind":"response_constructed", "data":output}),
            json!({"call":2, "kind":"call_received", "data":{"tool":"glass_test"}}),
            json!({"call":2, "kind":"router_rejection", "data":{"category":"tool_or_arguments_rejected", "raw_arguments":"excluded"}}),
            json!({"call":3, "kind":"call_received", "data":{"tool":"glass_test"}}),
            json!({"call":3, "kind":"worker_unavailable", "data":{}}),
            json!({"call":3, "kind":"response_constructed", "data":{"is_error":true, "target_access":"no_active_target", "content_blocks":0, "client_delivery":"unknown"}}),
            json!({"call":4, "kind":"call_received", "data":{"tool":"resources/read"}}),
            json!({"call":4, "kind":"resource_read", "data":{"is_error":false}}),
            json!({"call":4, "kind":"logical_outcome", "data":{"is_error":false}}),
        ]
    );
    assert!(
        export(recorder.path(), &root.path().join("complete.zip"))
            .unwrap()
            .complete
    );
}

#[tokio::test]
async fn abandoned_request_without_execution_outcome_stays_incomplete() {
    let root = private_root();
    let recorder = start_recorder(&root);
    let call = recorder.begin_call("glass_test", 1).unwrap();
    drop(RequestGuard::new(call));
    recorder.close().await;
    let report = inspect(recorder.path()).unwrap();
    assert!(!report.complete);
    let abandoned = report
        .events
        .iter()
        .find(|event| event["kind"] == "request_abandoned")
        .unwrap();
    assert_eq!(
        abandoned["data"],
        json!({"execution_outcome":"observe_worker_outcome"})
    );
    assert_eq!(
        report.events.last().unwrap()["data"]["unfinished_calls"],
        json!([1])
    );
    assert_eq!(
        export(recorder.path(), &root.path().join("incomplete.zip"))
            .unwrap()
            .exit_code(),
        2
    );
}

// Refresh accounting so malformed fixtures exercise semantic checks, not stale hashes.
fn rewrite_journal(path: &Path, mut events: Vec<Value>, mut manifest: Value) -> Vec<u8> {
    let mut journal = Vec::new();
    for (index, event) in events.iter_mut().enumerate() {
        event["seq"] = json!(index + 1);
        serde_json::to_writer(&mut journal, event).unwrap();
        journal.push(b'\n');
    }
    manifest["events"] = json!(events.len());
    manifest["journal_bytes"] = json!(journal.len());
    manifest["journal_sha256"] = json!(digest(&journal));
    let blobs: u64 = std::fs::read_dir(path.join("blobs"))
        .unwrap()
        .map(|entry| entry.unwrap().metadata().unwrap().len())
        .sum();
    let manifest_bytes = loop {
        let bytes = serde_json::to_vec(&manifest).unwrap();
        let total = blobs + journal.len() as u64 + bytes.len() as u64;
        if manifest["stored_bytes"] == total {
            break bytes;
        }
        manifest["stored_bytes"] = json!(total);
    };
    std::fs::write(path.join("events.jsonl"), &journal).unwrap();
    std::fs::write(path.join("manifest.json"), manifest_bytes).unwrap();
    journal
}

#[tokio::test]
async fn unknown_events_remain_opaque_and_export_unchanged() {
    for has_outcome in [false, true] {
        let root = private_root();
        let recorder = start_recorder(&root);
        let call = recorder.begin_call("glass_test", 1).unwrap();
        call.worker_unavailable();
        recorder.close().await;
        let report = inspect(recorder.path()).unwrap();
        let mut events = report.events;
        let mut unknown = events[2].clone();
        unknown["kind"] = json!("future_event");
        unknown["data"] = json!({"future_field":[1,2,3]});
        events.insert(2, unknown.clone());
        if !has_outcome {
            events.remove(3);
        }
        let mut manifest = report.manifest;
        manifest["complete"] = json!(has_outcome);
        let journal = rewrite_journal(recorder.path(), events.clone(), manifest.clone());
        let inspected = inspect(recorder.path()).unwrap();
        assert_eq!(inspected.complete, has_outcome);
        assert_eq!(inspected.events[2], unknown);
        let output = root.path().join("unknown.zip");
        assert_eq!(
            export(recorder.path(), &output).unwrap().complete,
            has_outcome
        );
        let mut archive = zip::ZipArchive::new(std::fs::File::open(output).unwrap()).unwrap();
        let mut exported = Vec::new();
        archive
            .by_name("events.jsonl")
            .unwrap()
            .read_to_end(&mut exported)
            .unwrap();
        assert_eq!(exported, journal);

        events[2]["kind"] = json!("x".repeat(65));
        rewrite_journal(recorder.path(), events, manifest);
        assert_eq!(
            inspect(recorder.path()).unwrap_err().to_string(),
            "trace event metadata exceeds limits"
        );
    }
}

#[tokio::test]
async fn inspection_independently_rejects_inconsistent_call_lifecycles() {
    let root = private_root();
    let recorder = start_recorder(&root);
    recorder
        .begin_call("glass_test", 1)
        .unwrap()
        .worker_unavailable();
    recorder.close().await;
    let report = inspect(recorder.path()).unwrap();
    assert!(report.complete);
    for case in [
        "duplicate_start",
        "unknown_call",
        "duplicate_outcome",
        "unfinished",
        "missing_close",
        "call_count",
    ] {
        let mut events = report.events.clone();
        let mut manifest = report.manifest.clone();
        let expected = match case {
            "duplicate_start" => {
                events.insert(2, events[1].clone());
                "duplicate trace call ID"
            }
            "unknown_call" => {
                events[2]["call"] = json!(2);
                "event refers to an unknown call"
            }
            "duplicate_outcome" => {
                let mut duplicate = events[2].clone();
                duplicate["kind"] = json!("logical_outcome");
                events.insert(3, duplicate);
                "duplicate call execution outcome"
            }
            "unfinished" => {
                events.remove(2);
                "complete trace has missing evidence"
            }
            "missing_close" => {
                events.pop();
                "final trace event missing"
            }
            "call_count" => {
                manifest["calls"] = json!(2);
                "trace call count mismatch"
            }
            _ => unreachable!(),
        };
        rewrite_journal(recorder.path(), events, manifest);
        assert_eq!(
            inspect(recorder.path()).unwrap_err().to_string(),
            expected,
            "{case}"
        );
        let output = root.path().join(format!("{case}.zip"));
        assert_eq!(
            export(recorder.path(), &output).unwrap_err().to_string(),
            expected,
            "{case}"
        );
        assert!(!output.exists());
    }
}
