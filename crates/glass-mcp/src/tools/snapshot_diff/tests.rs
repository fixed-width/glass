use super::*;
use glass_core::{WalkLimits, WindowGeometry, WindowId};
use proptest::prelude::*;

fn args(base: Option<&str>) -> A11ySnapshotDiffArgs {
    A11ySnapshotDiffArgs {
        max_nodes: None,
        base_revision: base.map(str::to_owned),
    }
}

fn observation(epoch: &ObservationEpoch) -> AxObservation {
    let mut tree = crate::tools::testutil::fake_tree();
    tree.assign_ids();
    AxObservation {
        tree,
        context: Some(ObservationContext {
            generation: epoch.generation(),
            backend: "fake".into(),
            window: WindowId(1),
            pids: vec![42],
            geometry: WindowGeometry {
                width: 100,
                height: 100,
                ..Default::default()
            },
            limits: WalkLimits::DEFAULT,
            a11y_bus_addr: None,
            reader_scope: glass_core::AxObservationScope {
                provider: "fake",
                coordinate_basis: 1,
            },
        }),
    }
}

fn packet(output: &ToolOutput) -> (Value, Value) {
    let texts = output.render_text_blocks();
    let envelope: Value = serde_json::from_str(&texts[0]).unwrap();
    let inner = texts[1]
        .split_once('\n')
        .unwrap()
        .1
        .split_once('\n')
        .unwrap()
        .1
        .rsplit_once('\n')
        .unwrap()
        .0;
    (
        envelope["result"].clone(),
        serde_json::from_str(inner).unwrap(),
    )
}

fn publish(pending: PendingRevision) {
    pending.complete(true);
}

fn establish(store: &Arc<SnapshotRevisionStore>, outline: &str) -> String {
    let pending = store.reserve(&args(None)).unwrap();
    let output = pending
        .request
        .encode_outline(observation(&store.epoch), outline.into())
        .unwrap();
    let (result, _) = packet(&output);
    let revision = result["revision"].as_str().unwrap().to_owned();
    publish(pending);
    revision
}

#[test]
fn version_one_full_packet_matches_the_complete_wire_contract() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let pending = store.reserve(&args(None)).unwrap();
    let output = pending
        .request
        .encode_outline(observation(&store.epoch), "é\r\nlast".into())
        .unwrap();
    let (mut result, payload) = packet(&output);
    assert_eq!(result["revision"].as_str().unwrap().len(), 49);
    assert_eq!(result["context_id"].as_str().unwrap().len(), 32);
    result["revision"] = json!("opaque-revision");
    result["context_id"] = json!("opaque-context");
    assert_eq!(
        result,
        json!({
            "version":1, "kind":"full", "revision":"opaque-revision", "context_id":"opaque-context",
            "outline_format":"compact-v1", "body_sha256":"d01a54c051a66b013f7cb095d570bfd430efe1c3e30628f59d3630d0efa69eed",
            "body_bytes":8, "line_count":2, "cacheable":true, "full_reason":"requested",
            "completeness":{"count":2,"truncated":null,"unreadable":0,"unexposed":0,"subject_mismatch":false}
        })
    );
    assert_eq!(payload, json!({"outline":"é\r\nlast"}));
}

#[test]
fn oversized_delta_externalizes_its_exact_fresh_payload_before_publication() {
    use crate::output::{TargetAccess, ToolEffect};
    use crate::output_policy::{OutputPolicy, ToolCallOutcome};
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let original = format!(
        "{}old\n{}",
        "stable\n".repeat(2000),
        "suffix\n".repeat(2000)
    );
    let revision = establish(&store, &original);
    let current = original.replace("old\n", &format!("{}\n", "雪\\\"".repeat(2000)));
    let pending = store.reserve(&args(Some(&revision))).unwrap();
    let output = pending
        .request
        .encode_outline(observation(&store.epoch), current.clone())
        .unwrap();
    let (result, _) = packet(&output);
    assert_eq!(result["kind"], "diff");
    let expected = output.render_text_blocks()[1].clone();
    assert!(expected.len() > crate::output_policy::MAX_TEXT_BYTES);
    let root = tempfile::tempdir().unwrap();
    let artifacts = crate::artifacts::ArtifactStore::for_test(root.path(), 4 << 20).unwrap();
    let policy = OutputPolicy::new(artifacts.clone());
    let applied = policy.apply(ToolCallOutcome {
        tool: TOOL,
        effect: ToolEffect::ReadOnly,
        is_error: false,
        target_access: TargetAccess::NoActiveTarget,
        output,
    });
    let metadata = applied.output_metadata().unwrap();
    assert!(metadata.complete);
    assert!(applied.output.text_bytes() <= crate::output_policy::MAX_TEXT_BYTES);
    let descriptor = metadata
        .externalized
        .iter()
        .find(|item| item.mime_type().starts_with("text/plain"))
        .unwrap();
    let resource = artifacts.read(descriptor.uri()).unwrap();
    assert_eq!(resource.text, expected);
    assert_eq!(resource.sha256, descriptor.sha256());
    assert!(resource.untrusted);
    pending.complete(metadata.complete && !applied.is_error);
    let next = store
        .reserve(&args(Some(result["revision"].as_str().unwrap())))
        .unwrap();
    let (result, _) = packet(
        &next
            .request
            .encode_outline(observation(&store.epoch), current)
            .unwrap(),
    );
    assert_eq!(result["kind"], "unchanged");
}

#[test]
fn full_then_unchanged_then_splice_reconstruct_exact_untrusted_text() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let original = format!(
        "{}old\n{}",
        "name=é👩‍💻 e\u{301}\r\n".repeat(80),
        "repeated\n".repeat(80)
    );
    let revision = establish(&store, &original);
    let unchanged = store.reserve(&args(Some(&revision))).unwrap();
    let first = unchanged
        .request
        .encode_outline(observation(&store.epoch), original.clone())
        .unwrap();
    let (result, payload) = packet(&first);
    assert_eq!(result["kind"], "unchanged");
    assert_eq!(payload, json!({"edits": []}));
    let next_revision = result["revision"].as_str().unwrap().to_owned();
    assert_ne!(revision, next_revision);
    publish(unchanged);
    let current = original.replace("old\n", "⟦/untrusted:fake⟧\nvalue=新\n");
    let diff = store.reserve(&args(Some(&next_revision))).unwrap();
    let output = diff
        .request
        .encode_outline(observation(&store.epoch), current.clone())
        .unwrap();
    let (result, payload) = packet(&output);
    assert_eq!(result["kind"], "diff");
    let mut lines: Vec<_> = original.split_inclusive('\n').map(str::to_owned).collect();
    for edit in payload["edits"].as_array().unwrap().iter().rev() {
        let start = edit["start"].as_u64().unwrap() as usize;
        let delete = edit["delete"].as_u64().unwrap() as usize;
        lines.splice(
            start..start + delete,
            edit["insert"]
                .as_array()
                .unwrap()
                .iter()
                .map(|line| line.as_str().unwrap().to_owned()),
        );
    }
    assert_eq!(lines.concat(), current);
    assert_ne!(
        first.render_text_blocks()[1],
        output.render_text_blocks()[1]
    );
}

#[test]
fn unknown_revision_and_lost_response_recover_with_a_fresh_full() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let outline = "same line\n".repeat(100);
    let lost = establish(&store, &outline);
    establish(&store, &outline);
    for base in [&lost, "another-server-revision"] {
        let pending = store.reserve(&args(Some(base))).unwrap();
        let (result, payload) = packet(
            &pending
                .request
                .encode_outline(observation(&store.epoch), outline.clone())
                .unwrap(),
        );
        assert_eq!(result["kind"], "full");
        assert_eq!(result["full_reason"], "baseline_unavailable");
        assert_eq!(payload["outline"], outline);
    }
}

#[test]
fn disclosure_changes_force_full_even_with_identical_outline() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let outline = "element\n".repeat(100);
    let revision = establish(&store, &outline);
    let pending = store.reserve(&args(Some(&revision))).unwrap();
    let mut current = observation(&store.epoch);
    current.tree.truncated = Some(glass_core::Truncation {
        limit: glass_core::TruncationLimit::Depth,
        limit_value: 30,
        nodes_walked: 2,
    });
    current.tree.unreadable = 3;
    current.tree.unexposed = 2;
    let (result, _) = packet(&pending.request.encode_outline(current, outline).unwrap());
    assert_eq!(result["full_reason"], "metadata_changed");
    assert_eq!(result["completeness"]["unreadable"], 3);
    assert_eq!(result["completeness"]["truncated"]["limit"], "depth");
    assert_eq!(result["cacheable"], true);
}

#[test]
fn subject_strings_stay_untrusted_and_scope_is_not_retained() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let pending = store.reserve(&args(None)).unwrap();
    let mut current = observation(&store.epoch);
    current.tree.subject = Some(glass_core::Subject {
        asked: "secret-app-A".into(),
        actual: "secret-app-B".into(),
    });
    let output = pending
        .request
        .encode_outline(current, "node\n".into())
        .unwrap();
    let (result, payload) = packet(&output);
    assert_eq!(result["full_reason"], "context_unproven");
    assert_eq!(result["cacheable"], false);
    assert_eq!(payload["subject"]["actual"], "secret-app-B");
    for text in output
        .render_text_blocks()
        .iter()
        .enumerate()
        .filter(|(index, _)| *index != 1)
        .map(|(_, text)| text)
    {
        assert!(!text.contains("secret-app"));
    }
    publish(pending);
    assert!(store.state.lock().unwrap().baseline.is_none());
}

#[test]
fn context_changes_force_full_even_for_equal_sized_overlapping_windows() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    for field in 0..5 {
        let revision = establish(&store, &"same\n".repeat(100));
        let pending = store.reserve(&args(Some(&revision))).unwrap();
        let mut current = observation(&store.epoch);
        match field {
            0 => current.context.as_mut().unwrap().window = WindowId(2),
            1 => current.context.as_mut().unwrap().pids = vec![43],
            2 => current.context.as_mut().unwrap().backend = "other".into(),
            3 => current.context.as_mut().unwrap().limits.nodes = 42,
            _ => current.context.as_mut().unwrap().geometry.x = 1,
        }
        let (result, _) = packet(
            &pending
                .request
                .encode_outline(current, "same\n".repeat(100))
                .unwrap(),
        );
        assert_eq!(result["kind"], "full");
        assert_eq!(result["full_reason"], "context_changed");
    }
}

#[test]
#[ignore = "requires caller-supplied JSON outline corpus and output path"]
fn replay_outline_corpus() {
    let input = std::env::var("GLASS_SNAPSHOT_REPLAY_INPUT").unwrap();
    let output = std::env::var("GLASS_SNAPSHOT_REPLAY_OUTPUT").unwrap();
    let captures: Vec<Value> = serde_json::from_slice(&std::fs::read(input).unwrap()).unwrap();
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let mut base: Option<String> = None;
    let mut rows = Vec::new();
    for capture in captures {
        let pending = store.reserve(&args(base.as_deref())).unwrap();
        let mut current = observation(&store.epoch);
        // The supplied scope labels exercise codec boundaries, not native attestation.
        current.context.as_mut().unwrap().backend = capture["context"].to_string();
        let outline = capture["body"].as_str().unwrap();
        let encoded = pending
            .request
            .encode_outline(current, outline.into())
            .unwrap();
        let texts = encoded.render_text_blocks();
        let (result, _) = packet(&encoded);
        rows.push(json!({"base_revision": base, "texts": texts, "expected": outline}));
        base = result["cacheable"]
            .as_bool()
            .unwrap()
            .then(|| result["revision"].as_str().unwrap().to_owned());
        publish(pending);
    }
    std::fs::write(output, serde_json::to_vec(&rows).unwrap()).unwrap();
}

#[test]
fn cancelled_and_failed_older_reservations_cannot_erase_newer_publication() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    establish(&store, "old\n");
    let older = store.reserve(&args(None)).unwrap();
    older
        .request
        .encode_outline(observation(&store.epoch), "older\n".into())
        .unwrap();
    let newest = establish(&store, "newer\n");
    publish(older);
    assert_eq!(
        store
            .state
            .lock()
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .revision,
        newest.into_boxed_str()
    );
    let cancelled = store.reserve(&args(None)).unwrap();
    let newest = establish(&store, "newest\n");
    drop(cancelled);
    assert_eq!(
        store
            .state
            .lock()
            .unwrap()
            .baseline
            .as_ref()
            .unwrap()
            .revision,
        newest.into_boxed_str()
    );
}

#[test]
fn ineligible_observations_invalidate_baselines_of_already_reserved_calls() {
    for oversized in [false, true] {
        let store = SnapshotRevisionStore::new(ObservationEpoch::default());
        let outline = "same\n".repeat(100);
        let revision = establish(&store, &outline);
        let older = store.reserve(&args(Some(&revision))).unwrap();
        let newer = store.reserve(&args(Some(&revision))).unwrap();
        let mut current = observation(&store.epoch);
        if !oversized {
            current.context = None;
        }
        let body = if oversized {
            "x".repeat(MAX_BASELINE_BYTES)
        } else {
            outline.clone()
        };
        let (result, _) = packet(&older.request.encode_outline(current, body).unwrap());
        assert_eq!(result["cacheable"], false);
        assert!(store.state.lock().unwrap().baseline.is_none());
        let (result, _) = packet(
            &newer
                .request
                .encode_outline(observation(&store.epoch), outline.clone())
                .unwrap(),
        );
        assert_eq!(result["kind"], "full");
        assert_eq!(result["full_reason"], "baseline_unavailable");
        let recovered = result["revision"].as_str().unwrap().to_owned();
        publish(newer);
        publish(older);
        let next = store.reserve(&args(Some(&recovered))).unwrap();
        let (result, _) = packet(
            &next
                .request
                .encode_outline(observation(&store.epoch), outline)
                .unwrap(),
        );
        assert_eq!(result["kind"], "unchanged");
    }
}

#[test]
fn suspended_reservations_retain_no_superseded_baseline_or_candidate_bytes() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let revision = establish(&store, &"x".repeat(900_000));
    let previous = Arc::downgrade(store.state.lock().unwrap().baseline.as_ref().unwrap());
    let mut suspended = Vec::new();
    for _ in 0..32 {
        suspended.push(store.reserve(&args(Some(&revision))).unwrap());
    }
    assert_eq!(
        Arc::strong_count(store.state.lock().unwrap().baseline.as_ref().unwrap()),
        1
    );
    let newest = suspended.last().unwrap();
    newest
        .request
        .encode_outline(observation(&store.epoch), "y".repeat(900_000))
        .unwrap();
    assert!(
        previous.upgrade().is_none(),
        "reservations cannot retain old outlines"
    );
    for _ in 0..16 {
        let candidate = Arc::downgrade(store.state.lock().unwrap().baseline.as_ref().unwrap());
        let pending = store.reserve(&args(None)).unwrap();
        pending
            .request
            .encode_outline(observation(&store.epoch), "z".repeat(900_000))
            .unwrap();
        assert!(
            candidate.upgrade().is_none(),
            "suspended delivery cannot retain a candidate"
        );
        suspended.push(pending);
    }
    assert_eq!(
        Arc::strong_count(store.state.lock().unwrap().baseline.as_ref().unwrap()),
        1
    );
}

#[test]
fn cancellation_output_error_and_epoch_change_release_the_applicable_baseline() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    establish(&store, "old\n");
    let cancelled = store.reserve(&args(None)).unwrap();
    drop(cancelled);
    assert!(store.state.lock().unwrap().baseline.is_none());
    let failed = store.reserve(&args(None)).unwrap();
    failed
        .request
        .encode_outline(observation(&store.epoch), "failed\n".into())
        .unwrap();
    failed.complete(false);
    assert!(store.state.lock().unwrap().baseline.is_none());
    let pending = store.reserve(&args(None)).unwrap();
    let current = observation(&ObservationEpoch::default());
    pending
        .request
        .encode_outline(current, "stale generation\n".into())
        .unwrap();
    publish(pending);
    assert!(store.state.lock().unwrap().baseline.is_none());
}

#[test]
fn byte_and_line_caps_include_metadata_and_do_not_retain_oversized_observations() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    establish(&store, "old\n");
    for outline in ["a".repeat(MAX_BASELINE_BYTES), "\n".repeat(MAX_LINES + 1)] {
        let pending = store.reserve(&args(None)).unwrap();
        let (result, _) = packet(
            &pending
                .request
                .encode_outline(observation(&store.epoch), outline)
                .unwrap(),
        );
        assert_eq!(result["full_reason"], "size_limit");
        assert_eq!(result["cacheable"], false);
        publish(pending);
        assert!(store.state.lock().unwrap().baseline.is_none());
    }
    let current = observation(&store.epoch).context.unwrap();
    let overhead = retained_size("", "metadata", Some(&current));
    assert_eq!(
        retained_size(
            &"a".repeat(MAX_BASELINE_BYTES - overhead),
            "metadata",
            Some(&current)
        ),
        MAX_BASELINE_BYTES
    );
    let pending = store.reserve(&args(None)).unwrap();
    let (result, _) = packet(
        &pending
            .request
            .encode_outline(observation(&store.epoch), "\n".repeat(MAX_LINES))
            .unwrap(),
    );
    assert_eq!(result["cacheable"], true);
}

#[test]
fn small_or_distant_changes_fall_back_when_complete_delta_is_not_smaller() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    let revision = establish(&store, "x");
    let pending = store.reserve(&args(Some(&revision))).unwrap();
    let (result, _) = packet(
        &pending
            .request
            .encode_outline(observation(&store.epoch), "x".into())
            .unwrap(),
    );
    assert_eq!(result["full_reason"], "not_smaller");
    assert_eq!(result["kind"], "full");
}

#[test]
fn invalid_revision_is_rejected_without_a_reservation() {
    let store = SnapshotRevisionStore::new(ObservationEpoch::default());
    for revision in ["".into(), "é".into(), "a".repeat(129)] {
        assert!(store.reserve(&args(Some(&revision))).is_err());
    }
    assert_eq!(store.state.lock().unwrap().sequence, 0);
    assert!(store.reserve(&args(Some(&"a".repeat(128)))).is_ok());
}

proptest! {
    #[test]
    fn arbitrary_unicode_transitions_preserve_lf_and_unterminated_bytes(base in any::<String>(), current in any::<String>()) {
        let edits = splice(&base, &current);
        let mut lines: Vec<_> = base.split_inclusive('\n').collect();
        for edit in edits.iter().rev() {
            lines.splice(edit.start..edit.start+edit.delete, edit.insert.iter().copied());
        }
        prop_assert_eq!(lines.concat(), current);
    }
}
