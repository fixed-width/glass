use super::*;
use std::fs;

fn draft(text: &str) -> ArtifactDraft {
    ArtifactDraft::content_block(text, "text/plain; charset=utf-8", true, 1)
}

fn publish_text(store: &ArtifactStore, text: &str) -> PublishedBatch {
    store
        .publish(vec![store.prepare(draft(text)).unwrap()])
        .unwrap()
}

#[test]
fn oldest_unpinned_artifact_is_evicted_and_reads_do_not_refresh_age() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 12).unwrap();
    let first = publish_text(&store, "aaaaaa");
    let first_uri = first.descriptors()[0].uri().to_owned();
    drop(first);
    let second = publish_text(&store, "bbbbbb");
    let second_uri = second.descriptors()[0].uri().to_owned();
    drop(second);
    assert_eq!(store.read(&first_uri).unwrap().text, "aaaaaa");

    let third = publish_text(&store, "cccccc");
    let third_uri = third.descriptors()[0].uri().to_owned();

    assert_eq!(
        store.read(&first_uri).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
    assert_eq!(store.read(&second_uri).unwrap().text, "bbbbbb");
    assert_eq!(store.read(&third_uri).unwrap().text, "cccccc");
}

#[test]
fn current_response_pin_can_temporarily_exceed_the_limit_then_evicts_on_drop() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 4).unwrap();
    let batch = publish_text(&store, "123456");
    let uri = batch.descriptors()[0].uri().to_owned();

    assert!(store.total_file_bytes().unwrap() >= 6);
    assert!(store.read(&uri).is_ok());
    drop(batch);

    assert_eq!(
        store.read(&uri).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
}

#[test]
fn read_pin_held_across_retention_keeps_artifact_until_read_drops() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 5).unwrap();
    let first = publish_text(&store, "first!");
    let first_uri = first.descriptors()[0].uri().to_owned();
    let held_read = store.read(&first_uri).unwrap();
    drop(first);

    let second = publish_text(&store, "second");
    let second_uri = second.descriptors()[0].uri().to_owned();
    drop(second);

    assert_eq!(held_read.text, "first!");
    assert!(store.total_file_bytes().unwrap() >= 6);
    assert_eq!(
        store.read(&second_uri).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
    drop(held_read);
    assert_eq!(
        store.read(&first_uri).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
}

#[test]
fn retention_leaves_overage_when_every_registered_artifact_is_pinned() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 4).unwrap();
    let batch = publish_text(&store, "123456");

    store.enforce_retention().unwrap();

    assert!(store.total_file_bytes().unwrap() >= 6);
    assert!(store.read(batch.descriptors()[0].uri()).is_ok());
}

#[test]
fn retention_counts_nested_unregistered_residue_without_refreshing_artifact_age() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 10).unwrap();
    let batch = publish_text(&store, "123456");
    let uri = batch.descriptors()[0].uri().to_owned();
    let residue_dir = store.process_dir().join("residue");
    fs::create_dir(&residue_dir).unwrap();
    fs::write(residue_dir.join("temporary"), "abcdef").unwrap();
    drop(batch);

    store.enforce_retention().unwrap();

    assert_eq!(
        store.read(&uri).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
    assert_eq!(
        fs::read_to_string(residue_dir.join("temporary")).unwrap(),
        "abcdef"
    );
}

#[test]
fn failed_eviction_keeps_the_registry_entry_available_for_diagnostics() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 5).unwrap();
    let batch = publish_text(&store, "123456");
    let path = batch.descriptors()[0].local_path().to_path_buf();
    fs::remove_file(&path).unwrap();
    fs::create_dir(&path).unwrap();
    fs::write(path.join("residue"), "123456").unwrap();
    drop(batch);

    assert!(store.enforce_retention().is_err());
    assert_eq!(store.registry_len(), 1);
}

#[test]
fn pin_drop_does_not_panic_when_the_registry_lock_is_poisoned() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let batch = publish_text(&store, "artifact");
    store.poison_state_for_test();

    assert!(std::panic::catch_unwind(|| drop(batch)).is_ok());
}

#[test]
fn failed_publication_rollback_disables_later_publication_but_preserves_existing_reads() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test_with_fault(
        root.path(),
        1024,
        FaultStage::PublicationRollbackCleanupFails,
    )
    .unwrap();
    let existing = publish_text(&store, "existing");
    let uri = existing.descriptors()[0].uri().to_owned();

    assert!(matches!(
        store.publish(vec![store.prepare(draft("trigger")).unwrap()]),
        Err(ArtifactError::RollbackFailed)
    ));

    assert_eq!(
        store.availability_error(),
        Some(ArtifactError::RollbackFailed)
    );
    assert_eq!(store.read(&uri).unwrap().text, "existing");
    assert!(matches!(
        store.prepare(draft("later")),
        Err(ArtifactError::InvalidOutputState)
    ));
}

fn make_stale_pair(root: &std::path::Path, id: &str) -> (std::path::PathBuf, std::path::PathBuf) {
    let process_dir = root.join(format!("server-{id}"));
    let lease_path = root.join(format!("server-{id}.lease"));
    fs::create_dir(&process_dir).unwrap();
    fs::write(process_dir.join("residue"), "stale").unwrap();
    fs::File::create(&lease_path).unwrap();
    (process_dir, lease_path)
}

#[test]
fn scavenger_removes_stale_pair_and_preserves_active_store() {
    let root = tempfile::tempdir().unwrap();
    let active = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let (stale_dir, stale_lease) = make_stale_pair(root.path(), "11111111111111111111111111111111");

    ArtifactStore::scavenge_for_test(root.path()).unwrap();

    assert!(active.process_dir().exists());
    assert!(active.lease_path().exists());
    assert!(!stale_dir.exists());
    assert!(!stale_lease.exists());
}

#[test]
fn scavenger_removes_nested_stale_contents_without_leaving_the_pair() {
    let root = tempfile::tempdir().unwrap();
    let (stale_dir, stale_lease) = make_stale_pair(root.path(), "66666666666666666666666666666666");
    let nested = stale_dir.join("nested");
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("residue"), "stale").unwrap();

    ArtifactStore::scavenge_for_test(root.path()).unwrap();

    assert!(!stale_dir.exists());
    assert!(!stale_lease.exists());
}

#[test]
fn scavenger_preserves_malformed_unpaired_and_lease_open_uncertainty() {
    let root = tempfile::tempdir().unwrap();
    let malformed = root.path().join("server-not-hex");
    fs::create_dir(&malformed).unwrap();
    let unpaired = root.path().join("server-22222222222222222222222222222222");
    fs::create_dir(&unpaired).unwrap();
    let (uncertain_dir, uncertain_lease) =
        make_stale_pair(root.path(), "33333333333333333333333333333333");

    ArtifactStore::scavenge_with_lease_open_fault_for_test(root.path(), &uncertain_lease).unwrap();

    assert!(malformed.exists());
    assert!(unpaired.exists());
    assert!(uncertain_dir.exists());
    assert!(uncertain_lease.exists());
}

#[cfg(unix)]
#[test]
fn scavenger_does_not_follow_symlink_candidate() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let link = root.path().join("server-44444444444444444444444444444444");
    let lease = root
        .path()
        .join("server-44444444444444444444444444444444.lease");
    symlink(outside.path(), &link).unwrap();
    fs::File::create(&lease).unwrap();

    ArtifactStore::scavenge_for_test(root.path()).unwrap();

    assert!(fs::symlink_metadata(&link).is_ok());
    assert!(lease.exists());
}

#[cfg(unix)]
#[test]
fn scavenger_unlinks_nested_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let (stale_dir, stale_lease) = make_stale_pair(root.path(), "77777777777777777777777777777777");
    symlink(&sentinel, stale_dir.join("link")).unwrap();

    ArtifactStore::scavenge_for_test(root.path()).unwrap();

    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
    assert!(!stale_dir.exists());
    assert!(!stale_lease.exists());
}

#[cfg(windows)]
#[test]
fn scavenger_does_not_follow_directory_reparse_candidate() {
    use std::os::windows::fs::symlink_dir;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let link = root.path().join("server-55555555555555555555555555555555");
    let lease = root
        .path()
        .join("server-55555555555555555555555555555555.lease");
    symlink_dir(outside.path(), &link).unwrap();
    fs::File::create(&lease).unwrap();

    ArtifactStore::scavenge_for_test(root.path()).unwrap();

    assert!(fs::symlink_metadata(&link).is_ok());
    assert!(lease.exists());
}

#[test]
fn shutdown_removes_registered_and_unregistered_files_and_is_idempotent() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let batch = publish_text(&store, "artifact");
    let process_dir = store.process_dir();
    let lease_path = store.lease_path();
    let residue_dir = process_dir.join("residue");
    fs::create_dir(&residue_dir).unwrap();
    fs::write(residue_dir.join("temporary"), "temporary").unwrap();

    store.shutdown().unwrap();
    store.shutdown().unwrap();

    assert!(!process_dir.exists());
    assert!(!lease_path.exists());
    drop(batch);
}

#[test]
fn shutdown_closing_state_rejects_reads_and_publication_until_retry_succeeds() {
    let root = tempfile::tempdir().unwrap();
    let store =
        ArtifactStore::for_test_with_fault(root.path(), 1024, FaultStage::ShutdownRemoveContents)
            .unwrap();
    let batch = publish_text(&store, "artifact");
    let uri = batch.descriptors()[0].uri().to_owned();

    assert!(store.shutdown().is_err());
    assert_eq!(
        store.read(&uri).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
    assert!(matches!(
        store.prepare(draft("later")),
        Err(ArtifactError::InvalidOutputState)
    ));
    store.shutdown().unwrap();
}

#[test]
fn shutdown_retries_each_incomplete_cleanup_phase() {
    for fault in [
        FaultStage::ShutdownRemoveContents,
        FaultStage::ShutdownRemoveDirectory,
        FaultStage::ShutdownCloseHandles,
        FaultStage::ShutdownRemoveLease,
    ] {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::for_test_with_fault(root.path(), 1024, fault).unwrap();
        let batch = publish_text(&store, "artifact");
        let process_dir = store.process_dir();
        let lease_path = store.lease_path();

        assert!(store.shutdown().is_err(), "fault {fault:?} did not fail");
        store.shutdown().unwrap();

        assert!(!process_dir.exists());
        assert!(!lease_path.exists());
        drop(batch);
    }
}

#[test]
fn concurrent_shutdown_calls_are_serialized_and_finish_closed() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let process_dir = store.process_dir();
    let lease_path = store.lease_path();
    let other = store.clone();

    let thread = std::thread::spawn(move || other.shutdown());
    store.shutdown().unwrap();
    thread.join().unwrap().unwrap();

    assert!(!process_dir.exists());
    assert!(!lease_path.exists());
}

#[cfg(unix)]
#[test]
fn shutdown_unlinks_symlink_without_touching_its_target() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    symlink(&sentinel, store.process_dir().join("link")).unwrap();

    store.shutdown().unwrap();

    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[cfg(windows)]
#[test]
fn shutdown_unlinks_directory_reparse_without_touching_its_target() {
    use std::os::windows::fs::symlink_dir;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    symlink_dir(outside.path(), store.process_dir().join("link")).unwrap();

    store.shutdown().unwrap();

    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[test]
fn store_creates_random_private_child_and_holds_lease() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 64 * 1024 * 1024).unwrap();
    assert!(store.process_dir().starts_with(root.path()));
    assert_ne!(store.process_dir(), root.path());
    assert!(store.lease_path().exists());
    assert_eq!(store.process_dir().parent(), Some(root.path()));
    assert_eq!(store.lease_path().parent(), Some(root.path()));
}

#[test]
fn prepared_text_publishes_atomically_and_reads_exact_bytes() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 64 * 1024 * 1024).unwrap();
    let prepared = store
        .prepare(ArtifactDraft::content_block(
            "wrapped application output",
            "text/plain; charset=utf-8",
            true,
            1,
        ))
        .unwrap();
    let published = store.publish(vec![prepared]).unwrap();
    let descriptor = &published.descriptors()[0];
    let read = store.read(descriptor.uri()).unwrap();
    assert_eq!(read.text, "wrapped application output");
    assert_eq!(read.sha256, descriptor.sha256());
}

#[test]
fn uri_parser_never_turns_caller_segments_into_paths() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    for uri in [
        "file:///etc/passwd",
        "glass-artifact://foreign/id",
        "glass-artifact://current/../secret",
        "glass-artifact://current/id/extra",
    ] {
        assert!(store.read(uri).is_err(), "accepted {uri}");
    }
}

#[cfg(unix)]
#[test]
fn default_root_joins_host_cache_with_glass_artifacts() {
    let cache = std::path::Path::new("/host/cache");
    assert_eq!(default_root_from(cache), cache.join("glass/artifacts"));
}

#[cfg(windows)]
#[test]
fn default_root_joins_windows_host_cache_with_glass_artifacts() {
    let cache = std::path::Path::new(r"C:\Users\person\AppData\Local");
    assert_eq!(default_root_from(cache), cache.join("glass/artifacts"));
}

#[cfg(unix)]
#[test]
fn store_uses_private_unix_modes() {
    use std::os::unix::fs::PermissionsExt;

    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("private")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    assert_eq!(
        fs::metadata(store.process_dir())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(store.lease_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
    assert_eq!(
        fs::metadata(descriptor.local_path())
            .unwrap()
            .permissions()
            .mode()
            & 0o777,
        0o600
    );
}

#[test]
fn second_store_cannot_take_held_lease() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test_with_id(root.path(), 1024, "11".repeat(16)).unwrap();
    let error = ArtifactStore::for_test_with_id(root.path(), 1024, store.server_id().to_owned())
        .unwrap_err();
    assert_eq!(error, ArtifactError::LeaseUnavailable);
}

#[test]
fn application_text_is_never_interpreted_as_metadata() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 4096).unwrap();
    let body =
        "glass-artifact://evil/../../x /etc/passwd {\"sha256\":\"fake\",\"untrusted\":false}";
    let published = store
        .publish(vec![store.prepare(draft(body)).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    let value = serde_json::to_value(descriptor).unwrap();
    assert_eq!(store.read(descriptor.uri()).unwrap().text, body);
    assert_eq!(value["local_path_scope"], "server");
    assert_eq!(value["mime_type"], "text/plain; charset=utf-8");
    assert_eq!(value["bytes"], body.len());
    assert_eq!(value["untrusted"], true);
    assert!(
        descriptor
            .uri()
            .starts_with(&format!("glass-artifact://{}/", store.server_id()))
    );
    assert!(descriptor.local_path().starts_with(store.process_dir()));
}

#[test]
fn read_open_failure_after_publication_is_bounded_and_returns_no_text() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("complete")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    fs::remove_file(descriptor.local_path()).unwrap();
    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::ReadFailed
    );
}

#[test]
fn tampered_artifact_never_returns_partial_or_unverified_text() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("complete")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    fs::write(descriptor.local_path(), "changed").unwrap();
    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
}

#[test]
fn oversized_replacement_is_rejected_before_body_allocation() {
    let root = tempfile::tempdir().unwrap();
    let store =
        ArtifactStore::for_test_with_fault(root.path(), 1024, FaultStage::ReadBodyFails).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("small")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    fs::write(descriptor.local_path(), vec![b'x'; 1024 * 1024]).unwrap();

    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
}

#[test]
fn registered_size_read_reaches_the_body_read_stage() {
    let root = tempfile::tempdir().unwrap();
    let store =
        ArtifactStore::for_test_with_fault(root.path(), 1024, FaultStage::ReadBodyFails).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("small")).unwrap()])
        .unwrap();

    assert_eq!(
        store.read(published.descriptors()[0].uri()).unwrap_err(),
        ArtifactReadError::ReadFailed
    );
}

#[test]
fn file_growth_during_read_is_rejected_without_partial_text() {
    let root = tempfile::tempdir().unwrap();
    let store =
        ArtifactStore::for_test_with_fault(root.path(), 1024, FaultStage::GrowDuringRead).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("stable")).unwrap()])
        .unwrap();

    assert_eq!(
        store.read(published.descriptors()[0].uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
}

#[cfg(unix)]
#[test]
fn replacing_process_directory_after_publication_cannot_redirect_read() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("known")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    let filename = descriptor.local_path().file_name().unwrap();
    fs::write(outside.path().join(filename), "known").unwrap();
    fs::rename(
        store.process_dir(),
        root.path().join("detached-process-dir"),
    )
    .unwrap();
    symlink(outside.path(), store.process_dir()).unwrap();

    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn replacing_root_ancestor_after_publication_cannot_redirect_read() {
    use std::os::unix::fs::symlink;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir(&root).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("known")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    let relative = descriptor.local_path().strip_prefix(&root).unwrap();
    let redirected = outside.path().join(relative);
    fs::create_dir_all(redirected.parent().unwrap()).unwrap();
    fs::write(&redirected, "known").unwrap();
    fs::rename(&root, parent.path().join("detached-root")).unwrap();
    symlink(outside.path(), &root).unwrap();

    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[test]
fn process_directory_cleanup_failure_overrides_initialization_error() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        ArtifactStore::for_test_with_fault(
            root.path(),
            1024,
            FaultStage::ProcessProtectionThenDirectoryCleanupFails,
        )
        .unwrap_err(),
        ArtifactError::CleanupFailed(std::io::ErrorKind::PermissionDenied)
    );
}

#[test]
fn lease_cleanup_failure_overrides_initialization_error() {
    let root = tempfile::tempdir().unwrap();
    assert_eq!(
        ArtifactStore::for_test_with_fault(
            root.path(),
            1024,
            FaultStage::DirectoryCreateThenLeaseCleanupFails,
        )
        .unwrap_err(),
        ArtifactError::CleanupFailed(std::io::ErrorKind::PermissionDenied)
    );
}

#[cfg(windows)]
#[test]
fn replacing_process_directory_with_reparse_target_cannot_redirect_read() {
    use std::os::windows::fs::symlink_dir;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("known")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    fs::write(
        outside
            .path()
            .join(descriptor.local_path().file_name().unwrap()),
        "known",
    )
    .unwrap();
    fs::rename(
        store.process_dir(),
        root.path().join("detached-process-dir"),
    )
    .unwrap();
    symlink_dir(outside.path(), store.process_dir()).unwrap();

    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[cfg(windows)]
#[test]
fn replacing_root_ancestor_with_reparse_target_cannot_redirect_read() {
    use std::os::windows::fs::symlink_dir;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir(&root).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("known")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    let relative = descriptor.local_path().strip_prefix(&root).unwrap();
    let redirected = outside.path().join(relative);
    fs::create_dir_all(redirected.parent().unwrap()).unwrap();
    fs::write(&redirected, "known").unwrap();
    fs::rename(&root, parent.path().join("detached-root")).unwrap();
    symlink_dir(outside.path(), &root).unwrap();

    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[test]
fn every_publication_stage_rolls_back_the_entire_batch() {
    for stage in FaultStage::publication_stages(2) {
        let root = tempfile::tempdir().unwrap();
        let store = ArtifactStore::for_test_with_fault(root.path(), 4096, stage).unwrap();
        let prepared = vec![
            store.prepare(draft("one")).unwrap(),
            store.prepare(draft("two")).unwrap(),
        ];
        assert!(store.publish(prepared).is_err(), "stage {stage:?}");
        assert_eq!(store.registry_len(), 0, "stage {stage:?}");
        assert_eq!(
            fs::read_dir(store.process_dir()).unwrap().count(),
            0,
            "stage {stage:?}"
        );
    }
}

#[cfg(unix)]
#[test]
fn rollback_does_not_follow_symlink_outside_process_directory() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store =
        ArtifactStore::for_test_with_fault(root.path(), 4096, FaultStage::TempCreated(0)).unwrap();
    symlink(outside.path(), store.process_dir().join("foreign-link")).unwrap();
    assert!(
        store
            .publish(vec![store.prepare(draft("one")).unwrap()])
            .is_err()
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn initialization_cleanup_removes_only_immediate_owned_children() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let id = "22".repeat(16);
    symlink(outside.path(), root.path().join(format!("server-{id}"))).unwrap();
    let lease = root.path().join(format!("server-{id}.lease"));
    assert_eq!(
        ArtifactStore::for_test_with_id(root.path(), 1024, id).unwrap_err(),
        ArtifactError::RootCreateFailed
    );
    assert!(!lease.exists());
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[cfg(windows)]
#[test]
fn real_store_applies_private_dacl_to_every_owned_path() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("private")).unwrap()])
        .unwrap();
    let paths = [
        store.process_dir(),
        store.lease_path(),
        published.descriptors()[0].local_path().to_owned(),
    ];
    assert!(
        paths
            .iter()
            .all(|path| glass_windows::path_has_private_dacl(path).unwrap())
    );
}
