use super::*;
use std::fs;

fn draft(text: &str) -> ArtifactDraft {
    ArtifactDraft::content_block(text, "text/plain; charset=utf-8", true, 1)
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
