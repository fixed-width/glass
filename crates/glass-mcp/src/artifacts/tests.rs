use super::*;
use std::fs;
use std::sync::Arc;

#[cfg(target_os = "macos")]
#[test]
fn store_initializes_from_an_empty_root_on_macos() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();

    store.shutdown().unwrap();
}

#[cfg(unix)]
#[test]
fn retained_directory_enumeration_excludes_navigation_entries() {
    let root = tempfile::tempdir().unwrap();
    fs::write(root.path().join("owned"), "owned").unwrap();
    let handle = fs::File::open(root.path()).unwrap();

    let entries = super::fs::handle_directory_entries(&handle).unwrap();

    assert_eq!(entries, [std::ffi::OsString::from("owned")]);
}

#[cfg(target_os = "linux")]
#[test]
fn retained_directory_enumeration_preserves_non_utf8_names() {
    use std::os::unix::ffi::OsStringExt;

    let root = tempfile::tempdir().unwrap();
    let name = std::ffi::OsString::from_vec(vec![b'o', 0xff, b'k']);
    fs::write(root.path().join(&name), "owned").unwrap();
    let handle = fs::File::open(root.path()).unwrap();

    assert_eq!(
        super::fs::handle_directory_entries(&handle).unwrap(),
        [name]
    );
}

#[cfg(unix)]
#[test]
fn retained_directory_enumeration_treats_a_removed_directory_as_empty() {
    let parent = tempfile::tempdir().unwrap();
    let path = parent.path().join("removed");
    fs::create_dir(&path).unwrap();
    let handle = fs::File::open(&path).unwrap();
    fs::remove_dir(&path).unwrap();

    assert!(
        super::fs::handle_directory_entries(&handle)
            .unwrap()
            .is_empty()
    );
}

#[cfg(unix)]
#[test]
fn store_preserves_operator_root_and_keeps_owned_children_private() {
    use std::os::unix::fs::PermissionsExt;

    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("operator-root");
    fs::create_dir(&root).unwrap();
    fs::set_permissions(&root, fs::Permissions::from_mode(0o755)).unwrap();
    let unrelated = root.join("operator-owned.txt");
    fs::write(&unrelated, "leave me alone").unwrap();

    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    let process_dir = store.process_dir().to_owned();
    let lease_path = store.lease_path().to_owned();

    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(
        fs::metadata(&process_dir).unwrap().permissions().mode() & 0o777,
        0o700
    );
    assert_eq!(
        fs::metadata(&lease_path).unwrap().permissions().mode() & 0o777,
        0o600
    );
    assert_eq!(fs::read_to_string(&unrelated).unwrap(), "leave me alone");

    store.shutdown().unwrap();

    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o755
    );
    assert_eq!(fs::read_to_string(&unrelated).unwrap(), "leave me alone");
}

#[cfg(windows)]
#[test]
fn store_preserves_existing_operator_root_dacl_posture() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("operator-root");
    fs::create_dir(&root).unwrap();
    let unrelated = root.join("operator-owned.txt");
    fs::write(&unrelated, "leave me alone").unwrap();
    assert!(!glass_windows::path_has_private_dacl(&root).unwrap());

    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    assert!(!glass_windows::path_has_private_dacl(&root).unwrap());
    assert!(glass_windows::path_has_private_dacl(&store.process_dir()).unwrap());
    assert!(glass_windows::path_has_private_dacl(&store.lease_path()).unwrap());

    store.shutdown().unwrap();

    assert!(!glass_windows::path_has_private_dacl(&root).unwrap());
    assert_eq!(fs::read_to_string(&unrelated).unwrap(), "leave me alone");
}

#[cfg(windows)]
#[test]
fn host_integrity_cleanup_is_not_collapsed_to_other() {
    assert_eq!(
        map_host_cleanup(glass_windows::HostFsError::Integrity),
        ArtifactError::CleanupFailed(std::io::ErrorKind::InvalidData)
    );
}

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
fn absent_valid_current_server_id_is_expired_but_malformed_and_foreign_ids_are_not_found() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let unknown = format!("glass-artifact://{}/{}", store.server_id(), "a".repeat(32));

    assert_eq!(
        store.read(&unknown).unwrap_err(),
        ArtifactReadError::ExpiredOrUnavailable
    );
    assert_eq!(
        store
            .read("glass-artifact://foreign/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa")
            .unwrap_err(),
        ArtifactReadError::ResourceNotFound
    );
    assert_eq!(
        store
            .read(&format!("glass-artifact://{}/bad", store.server_id()))
            .unwrap_err(),
        ArtifactReadError::ResourceNotFound
    );
}

#[test]
fn sequence_overflow_is_bounded_and_rolls_back_publication() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    store.set_next_seq_for_test(u64::MAX);
    let prepared = store.prepare(draft("overflow")).unwrap();

    assert!(matches!(
        store.publish(vec![prepared]),
        Err(ArtifactError::InvalidOutputState)
    ));
    assert_eq!(store.registry_len(), 0);
    assert_eq!(fs::read_dir(store.process_dir()).unwrap().count(), 0);
}

#[test]
fn concurrent_publishers_reserve_the_final_sequence_once() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    store.set_next_seq_for_test(u64::MAX - 1);
    let first = store.prepare(draft("first")).unwrap();
    let second = store.prepare(draft("second")).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(3));
    let mut threads = Vec::new();
    for prepared in [first, second] {
        let store = store.clone();
        let barrier = Arc::clone(&barrier);
        threads.push(std::thread::spawn(move || {
            barrier.wait();
            store.publish(vec![prepared])
        }));
    }
    barrier.wait();
    let results = threads
        .into_iter()
        .map(|thread| thread.join().unwrap())
        .collect::<Vec<_>>();

    assert_eq!(
        results.iter().filter(|result| result.is_ok()).count(),
        1,
        "publication errors: {:?}",
        results
            .iter()
            .filter_map(|result| result.as_ref().err())
            .collect::<Vec<_>>()
    );
    assert_eq!(
        results
            .iter()
            .filter(|result| matches!(result, Err(ArtifactError::InvalidOutputState)))
            .count(),
        1
    );
    assert_eq!(store.registry_len(), 1);
}

#[test]
fn concurrent_publications_remain_readable_until_each_response_unpins() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1).unwrap();
    let barrier = Arc::new(std::sync::Barrier::new(9));
    let threads = (0..8)
        .map(|index| {
            let store = store.clone();
            let barrier = Arc::clone(&barrier);
            std::thread::spawn(move || {
                let text = format!("response {index}");
                let prepared = store.prepare(draft(&text)).unwrap();
                barrier.wait();
                (text, store.publish(vec![prepared]))
            })
        })
        .collect::<Vec<_>>();
    barrier.wait();
    let batches = threads
        .into_iter()
        .map(|thread| {
            let (text, result) = thread.join().unwrap();
            (text, result.unwrap())
        })
        .collect::<Vec<_>>();

    assert_eq!(store.registry_len(), batches.len());
    for (text, batch) in &batches {
        assert_eq!(
            store.read(batch.descriptors()[0].uri()).unwrap().text,
            *text
        );
    }
    drop(batches);
    assert_eq!(store.registry_len(), 0);
    assert_eq!(store.total_file_bytes().unwrap(), 0);
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
fn scavenger_preserves_a_non_directory_process_candidate() {
    let root = tempfile::tempdir().unwrap();
    let link = root.path().join("server-55555555555555555555555555555555");
    let lease = root
        .path()
        .join("server-55555555555555555555555555555555.lease");
    fs::write(&link, "uncertain").unwrap();
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

#[cfg(windows)]
#[test]
fn lease_removal_releases_its_lock_before_disposition() {
    let root = tempfile::tempdir().unwrap();
    let lease_name = std::ffi::OsStr::new("server-test.lease");
    let lease_path = root.path().join(lease_name);
    let root_handle = super::fs::open_root_directory(root.path()).unwrap();
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lease_path)
        .unwrap();
    lease.lock().unwrap();
    let mut lock_held = true;

    super::fs::remove_locked_lease_from_handle(&root_handle, lease_name, &lease, &mut lock_held)
        .unwrap();
    assert!(!lock_held);
    drop(lease);

    assert!(!lease_path.exists());
}

#[cfg(windows)]
#[test]
fn lease_removal_retry_reacquires_a_lock_released_by_a_failed_disposition() {
    let root = tempfile::tempdir().unwrap();
    let lease_name = std::ffi::OsStr::new("server-test.lease");
    let lease_path = root.path().join(lease_name);
    let root_handle = super::fs::open_root_directory(root.path()).unwrap();
    let lease = fs::OpenOptions::new()
        .read(true)
        .write(true)
        .create_new(true)
        .open(&lease_path)
        .unwrap();
    lease.lock().unwrap();
    let mut lock_held = true;
    let competing_lock = Arc::new(std::sync::Mutex::new(None));
    let competing_lock_during_remove = Arc::clone(&competing_lock);
    let lease_path_during_remove = lease_path.clone();

    assert_eq!(
        super::fs::remove_locked_lease_from_handle_with(
            &root_handle,
            lease_name,
            &lease,
            &mut lock_held,
            move |_| {
                let competitor = fs::OpenOptions::new()
                    .read(true)
                    .write(true)
                    .open(lease_path_during_remove)
                    .unwrap();
                competitor.lock().unwrap();
                *competing_lock_during_remove.lock().unwrap() = Some(competitor);
                Err(glass_windows::HostFsError::Open)
            },
        )
        .unwrap_err(),
        ArtifactError::LockFailed
    );
    assert!(!lock_held);
    drop(competing_lock.lock().unwrap().take());

    super::fs::remove_locked_lease_from_handle_with(
        &root_handle,
        lease_name,
        &lease,
        &mut lock_held,
        glass_windows::remove_by_handle,
    )
    .unwrap();
    drop(lease);

    assert!(!lease_path.exists());
}

#[cfg(windows)]
#[test]
fn artifact_root_handle_lacks_delete_access() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("artifact-root");
    fs::create_dir(&root).unwrap();
    let handle = super::fs::open_root_directory(&root).unwrap();

    assert_eq!(
        glass_windows::remove_by_handle(&handle),
        Err(glass_windows::HostFsError::Open)
    );
    assert!(root.exists());
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
fn shutdown_retry_advances_when_process_directory_was_removed_externally() {
    let root = tempfile::tempdir().unwrap();
    let store =
        ArtifactStore::for_test_with_fault(root.path(), 1024, FaultStage::ShutdownRemoveContents)
            .unwrap();
    let lease = store.lease_path();
    assert!(store.shutdown().is_err());
    fs::remove_dir_all(store.process_dir()).unwrap();

    store.shutdown().unwrap();

    assert!(!lease.exists());
}

#[cfg(target_os = "linux")]
#[test]
fn shutdown_removes_fifo_and_socket_residue() {
    use std::os::unix::net::UnixListener;

    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let fifo = store.process_dir().join("fifo");
    rustix::fs::mknodat(
        rustix::fs::CWD,
        &fifo,
        rustix::fs::FileType::Fifo,
        rustix::fs::Mode::RUSR | rustix::fs::Mode::WUSR,
        0,
    )
    .unwrap();
    let socket = store.process_dir().join("socket");
    let listener = UnixListener::bind(&socket).unwrap();

    store.shutdown().unwrap();
    drop(listener);

    assert!(!fifo.exists());
    assert!(!socket.exists());
}

#[cfg(unix)]
#[test]
fn retention_and_shutdown_do_not_follow_a_replaced_process_path() {
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "outside").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1).unwrap();
    let batch = store
        .publish(vec![store.prepare(draft("retained")).unwrap()])
        .unwrap();
    let detached = root.path().join("detached-process");
    fs::rename(store.process_dir(), &detached).unwrap();
    symlink(outside.path(), store.process_dir()).unwrap();

    drop(batch);
    assert_eq!(store.total_file_bytes().unwrap(), 0);
    assert!(store.shutdown().is_err());
    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "outside");
}

#[cfg(any(unix, windows))]
#[test]
fn accounting_does_not_follow_a_nested_directory_substituted_after_enumeration() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, vec![0_u8; 4096]).unwrap();
    let store = ArtifactStore::for_test(root.path(), 8192).unwrap();
    let nested = store.process_dir().join("nested");
    let detached = root.path().join("detached-nested");
    #[cfg(windows)]
    let windows_replacement = root.path().join("accounting-replacement");
    #[cfg(windows)]
    {
        fs::create_dir(&windows_replacement).unwrap();
        fs::write(windows_replacement.join("sentinel"), vec![0_u8; 4096]).unwrap();
    }
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("owned"), "owned").unwrap();
    let replacement = nested.clone();
    let hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_fired_in_hook = std::sync::Arc::clone(&hook_fired);
    #[cfg(unix)]
    let outside_path = outside.path().to_path_buf();
    set_fs_test_hook(TestHookPoint::AfterAccountingEnumeration, move || {
        hook_fired_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        fs::rename(&replacement, &detached).unwrap();
        #[cfg(unix)]
        symlink(outside_path, replacement).unwrap();
        #[cfg(windows)]
        fs::rename(windows_replacement, replacement).unwrap();
    });

    #[cfg(unix)]
    assert_eq!(store.total_file_bytes().unwrap(), 0);
    #[cfg(windows)]
    assert!(store.total_file_bytes().is_err());
    assert!(hook_fired.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(fs::metadata(&sentinel).unwrap().len(), 4096);
    #[cfg(windows)]
    assert_eq!(fs::metadata(nested.join("sentinel")).unwrap().len(), 4096);
}

#[cfg(any(unix, windows))]
#[test]
fn eviction_does_not_unlink_a_substitution_made_before_removal() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "outside").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1).unwrap();
    let batch = publish_text(&store, "artifact");
    let artifact = fs::read_dir(store.process_dir())
        .unwrap()
        .next()
        .unwrap()
        .unwrap()
        .path();
    let detached = root.path().join("detached-artifact");
    let detached_for_hook = detached.clone();
    let replacement = artifact.clone();
    let target = sentinel.clone();
    set_fs_test_hook(TestHookPoint::BeforeEvictionRemove, move || {
        fs::rename(&replacement, &detached_for_hook).unwrap();
        #[cfg(unix)]
        symlink(target, replacement).unwrap();
        #[cfg(windows)]
        fs::hard_link(target, replacement).unwrap();
    });

    drop(batch);

    assert_eq!(fs::read_to_string(sentinel).unwrap(), "outside");
    #[cfg(unix)]
    assert_eq!(fs::read_to_string(detached).unwrap(), "artifact");
    #[cfg(windows)]
    assert!(!detached.exists());
}

#[cfg(any(unix, windows))]
#[test]
fn shutdown_does_not_follow_a_nested_substitution_after_enumeration() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "outside").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let nested = store.process_dir().join("nested");
    let detached = root.path().join("detached-cleanup");
    let detached_for_hook = detached.clone();
    #[cfg(windows)]
    let windows_replacement = root.path().join("shutdown-replacement");
    #[cfg(windows)]
    {
        fs::create_dir(&windows_replacement).unwrap();
        fs::write(windows_replacement.join("sentinel"), "outside").unwrap();
    }
    fs::create_dir(&nested).unwrap();
    fs::write(nested.join("owned"), "owned").unwrap();
    let replacement = nested.clone();
    let hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_fired_in_hook = std::sync::Arc::clone(&hook_fired);
    #[cfg(unix)]
    let outside_path = outside.path().to_path_buf();
    set_fs_test_hook(TestHookPoint::AfterCleanupEnumeration, move || {
        hook_fired_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        fs::rename(&replacement, &detached_for_hook).unwrap();
        #[cfg(unix)]
        symlink(outside_path, replacement).unwrap();
        #[cfg(windows)]
        fs::rename(windows_replacement, replacement).unwrap();
    });

    #[cfg(unix)]
    store.shutdown().unwrap();
    #[cfg(windows)]
    assert!(store.shutdown().is_err());
    assert!(hook_fired.load(std::sync::atomic::Ordering::SeqCst));

    assert_eq!(fs::read_to_string(sentinel).unwrap(), "outside");
    #[cfg(unix)]
    assert_eq!(fs::read_to_string(detached.join("owned")).unwrap(), "owned");
    #[cfg(windows)]
    assert_eq!(
        fs::read_to_string(nested.join("sentinel")).unwrap(),
        "outside"
    );
}

#[cfg(any(unix, windows))]
#[test]
fn scavenging_does_not_open_a_process_directory_substituted_after_enumeration() {
    #[cfg(unix)]
    use std::os::unix::fs::symlink;

    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "outside").unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let process = root.path().join(format!("server-{id}"));
    let detached = root.path().join("detached-stale");
    let lease = root.path().join(format!("server-{id}.lease"));
    #[cfg(windows)]
    let windows_replacement = root.path().join("scavenge-replacement");
    #[cfg(windows)]
    {
        fs::create_dir(&windows_replacement).unwrap();
        fs::write(windows_replacement.join("sentinel"), "outside").unwrap();
    }
    fs::create_dir(&process).unwrap();
    fs::write(process.join("owned"), "owned").unwrap();
    fs::write(&lease, "").unwrap();
    let replacement = process.clone();
    let hook_fired = std::sync::Arc::new(std::sync::atomic::AtomicBool::new(false));
    let hook_fired_in_hook = std::sync::Arc::clone(&hook_fired);
    #[cfg(unix)]
    let outside_path = outside.path().to_path_buf();
    set_fs_test_hook(TestHookPoint::AfterScavengeEnumeration, move || {
        hook_fired_in_hook.store(true, std::sync::atomic::Ordering::SeqCst);
        fs::rename(&replacement, &detached).unwrap();
        #[cfg(unix)]
        symlink(outside_path, replacement).unwrap();
        #[cfg(windows)]
        fs::rename(windows_replacement, replacement).unwrap();
    });

    let current = ArtifactStore::for_test(root.path(), 1024).unwrap();

    assert!(hook_fired.load(std::sync::atomic::Ordering::SeqCst));
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "outside");
    #[cfg(unix)]
    assert!(lease.exists());
    #[cfg(windows)]
    assert!(lease.exists());
    #[cfg(windows)]
    assert_eq!(
        fs::read_to_string(process.join("sentinel")).unwrap(),
        "outside"
    );
    drop(current);
}

#[cfg(unix)]
#[test]
fn shutdown_preserves_a_replacement_lease() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let lease = store.lease_path();
    let retained = root.path().join("retained-lease");
    fs::rename(&lease, &retained).unwrap();
    fs::write(&lease, "replacement").unwrap();

    assert!(store.shutdown().is_err());
    assert_eq!(fs::read_to_string(&lease).unwrap(), "replacement");
}

#[cfg(unix)]
#[test]
fn shutdown_quarantine_never_overwrites_a_replacement_created_mid_cleanup() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let lease = store.lease_path();
    let replacement = lease.clone();
    store.set_test_hook(TestHookPoint::AfterLeaseQuarantine, move || {
        fs::write(&replacement, "replacement").unwrap();
    });

    assert!(store.shutdown().is_err());
    assert_eq!(fs::read_to_string(&lease).unwrap(), "replacement");
    assert_eq!(store.lease_quarantine_count_for_test(), 1);

    fs::remove_file(&lease).unwrap();
    store.shutdown().unwrap();
    assert_eq!(store.lease_quarantine_count_for_test(), 0);
}

#[cfg(unix)]
#[test]
fn startup_recovers_a_shutdown_quarantine_after_process_restart() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let lease = store.lease_path();
    let replacement = lease.clone();
    store.set_test_hook(TestHookPoint::AfterLeaseQuarantine, move || {
        fs::write(&replacement, "replacement").unwrap();
    });
    assert!(store.shutdown().is_err());
    drop(store);
    fs::remove_file(&lease).unwrap();

    let next = ArtifactStore::for_test(root.path(), 1024).unwrap();

    let cleanup_entries = fs::read_dir(root.path())
        .unwrap()
        .filter_map(Result::ok)
        .filter(|entry| {
            entry
                .file_name()
                .to_string_lossy()
                .contains("lease-cleanup")
        })
        .count();
    assert_eq!(cleanup_entries, 0);
    drop(next);
}

#[cfg(unix)]
#[test]
fn startup_recovers_strict_quarantine_with_stale_process_directory() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let process = root.path().join(format!("server-{id}"));
    let quarantine = root.path().join(format!(
        "server-{id}.lease-cleanup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    fs::create_dir(&process).unwrap();
    fs::write(process.join("residue"), "owned").unwrap();
    fs::write(&quarantine, "").unwrap();

    let next = ArtifactStore::for_test(root.path(), 1024).unwrap();

    assert!(!process.exists());
    assert!(!quarantine.exists());
    drop(next);
}

#[cfg(unix)]
#[test]
fn startup_preserves_replacement_installed_before_recovery_reservation() {
    use std::fs::OpenOptions;
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let process = root.path().join(format!("server-{id}"));
    let detached = root.path().join("detached-stale-process");
    let canonical = root.path().join(format!("server-{id}.lease"));
    let quarantine = root.path().join(format!(
        "server-{id}.lease-cleanup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    fs::create_dir(&process).unwrap();
    fs::write(process.join("stale"), "stale").unwrap();
    fs::write(&quarantine, "quarantine").unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let fired_hook = Arc::clone(&fired);
    let process_hook = process.clone();
    let canonical_hook = canonical.clone();
    let locked = Arc::new(std::sync::Mutex::new(None));
    let locked_hook = Arc::clone(&locked);
    set_fs_test_hook(TestHookPoint::AfterQuarantineCanonicalAbsent, move || {
        fired_hook.store(true, Ordering::SeqCst);
        fs::rename(&process_hook, &detached).unwrap();
        fs::create_dir(&process_hook).unwrap();
        fs::write(process_hook.join("replacement-sentinel"), "active").unwrap();
        let lease = OpenOptions::new()
            .read(true)
            .write(true)
            .create_new(true)
            .open(canonical_hook)
            .unwrap();
        lease.lock().unwrap();
        *locked_hook.lock().unwrap() = Some(lease);
    });

    let next = ArtifactStore::for_test(root.path(), 1024).unwrap();

    assert!(fired.load(Ordering::SeqCst));
    assert_eq!(
        fs::read_to_string(process.join("replacement-sentinel")).unwrap(),
        "active"
    );
    assert!(canonical.exists());
    assert!(quarantine.exists());
    drop(next);
}

#[cfg(unix)]
#[test]
fn startup_reservation_blocks_replacement_and_removes_only_stale_process() {
    use std::sync::{
        Arc,
        atomic::{AtomicBool, Ordering},
    };

    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let process = root.path().join(format!("server-{id}"));
    let canonical = root.path().join(format!("server-{id}.lease"));
    let quarantine = root.path().join(format!(
        "server-{id}.lease-cleanup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    fs::create_dir(&process).unwrap();
    fs::write(process.join("stale"), "stale").unwrap();
    fs::write(&quarantine, "quarantine").unwrap();
    let fired = Arc::new(AtomicBool::new(false));
    let blocked = Arc::new(AtomicBool::new(false));
    let fired_hook = Arc::clone(&fired);
    let blocked_hook = Arc::clone(&blocked);
    set_fs_test_hook(TestHookPoint::AfterRecoveryReservation, move || {
        fired_hook.store(true, Ordering::SeqCst);
        blocked_hook.store(
            fs::OpenOptions::new()
                .write(true)
                .create_new(true)
                .open(&canonical)
                .is_err(),
            Ordering::SeqCst,
        );
    });

    let next = ArtifactStore::for_test(root.path(), 1024).unwrap();

    assert!(fired.load(Ordering::SeqCst));
    assert!(blocked.load(Ordering::SeqCst));
    assert!(!process.exists());
    assert!(!quarantine.exists());
    drop(next);
}

#[cfg(unix)]
#[test]
fn startup_resumes_each_recovery_crash_state() {
    for point in [
        TestHookPoint::AfterRecoveryReservation,
        TestHookPoint::AfterRecoveryProcessRemoval,
        TestHookPoint::AfterRecoveryGuardRemoval,
    ] {
        let root = tempfile::tempdir().unwrap();
        let id = "0123456789abcdef0123456789abcdef";
        let process = root.path().join(format!("server-{id}"));
        let canonical = root.path().join(format!("server-{id}.lease"));
        let quarantine = root.path().join(format!(
            "server-{id}.lease-cleanup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
        ));
        fs::create_dir(&process).unwrap();
        fs::write(process.join("stale"), "stale").unwrap();
        fs::write(&quarantine, "quarantine").unwrap();
        set_fs_test_hook(point, || panic!("simulated recovery interruption"));

        assert!(std::panic::catch_unwind(|| ArtifactStore::for_test(root.path(), 1024)).is_err());

        let next = ArtifactStore::for_test(root.path(), 1024).unwrap();
        assert!(!process.exists(), "process remained after {point:?}");
        assert!(!canonical.exists(), "guard remained after {point:?}");
        assert!(!quarantine.exists(), "quarantine remained after {point:?}");
        drop(next);
    }
}

#[cfg(unix)]
#[test]
fn startup_preserves_an_obstructed_quarantine_until_a_later_pass() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let canonical = root.path().join(format!("server-{id}.lease"));
    let quarantine = root.path().join(format!(
        "server-{id}.lease-cleanup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    fs::write(&canonical, "replacement").unwrap();
    fs::write(&quarantine, "quarantined").unwrap();

    let first = ArtifactStore::for_test(root.path(), 1024).unwrap();
    assert_eq!(fs::read_to_string(&canonical).unwrap(), "replacement");
    assert_eq!(fs::read_to_string(&quarantine).unwrap(), "quarantined");
    drop(first);

    fs::remove_file(&canonical).unwrap();
    let second = ArtifactStore::for_test(root.path(), 1024).unwrap();
    assert!(!quarantine.exists());
    drop(second);
}

#[cfg(unix)]
#[test]
fn scavenger_quarantine_remains_recoverable_after_a_mid_cleanup_replacement() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let process = root.path().join(format!("server-{id}"));
    let canonical = root.path().join(format!("server-{id}.lease"));
    fs::create_dir(&process).unwrap();
    fs::write(process.join("residue"), "owned").unwrap();
    fs::write(&canonical, "stale").unwrap();
    let replacement = canonical.clone();
    set_fs_test_hook(TestHookPoint::AfterLeaseQuarantine, move || {
        fs::write(replacement, "replacement").unwrap();
    });

    assert!(ArtifactStore::for_test(root.path(), 1024).is_err());
    assert_eq!(fs::read_to_string(&canonical).unwrap(), "replacement");
    assert_eq!(
        fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("lease-cleanup"))
            .count(),
        1
    );

    fs::remove_file(&canonical).unwrap();
    let next = ArtifactStore::for_test(root.path(), 1024).unwrap();
    assert!(!process.exists());
    assert_eq!(
        fs::read_dir(root.path())
            .unwrap()
            .filter_map(Result::ok)
            .filter(|entry| entry
                .file_name()
                .to_string_lossy()
                .contains("lease-cleanup"))
            .count(),
        0
    );
    drop(next);
}

#[cfg(unix)]
#[test]
fn startup_preserves_obstructed_multiple_and_malformed_quarantines() {
    let root = tempfile::tempdir().unwrap();
    let id = "0123456789abcdef0123456789abcdef";
    let first = root.path().join(format!(
        "server-{id}.lease-cleanup-aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa"
    ));
    let second = root.path().join(format!(
        "server-{id}.lease-cleanup-bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb"
    ));
    let malformed = root.path().join("server-bad.lease-cleanup-not-owned");
    fs::write(&first, "first").unwrap();
    fs::write(&second, "second").unwrap();
    fs::write(&malformed, "malformed").unwrap();

    let next = ArtifactStore::for_test(root.path(), 1024).unwrap();

    assert_eq!(fs::read_to_string(first).unwrap(), "first");
    assert_eq!(fs::read_to_string(second).unwrap(), "second");
    assert_eq!(fs::read_to_string(malformed).unwrap(), "malformed");
    drop(next);
}

#[cfg(unix)]
#[test]
fn shutdown_uses_the_retained_root_after_ancestor_replacement() {
    let parent = tempfile::tempdir().unwrap();
    let root = parent.path().join("root");
    fs::create_dir(&root).unwrap();
    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    let detached = parent.path().join("detached-root");
    fs::rename(&root, &detached).unwrap();
    fs::create_dir(&root).unwrap();
    let sentinel = root.join("sentinel");
    fs::write(&sentinel, "replacement").unwrap();

    store.shutdown().unwrap();

    assert_eq!(fs::read_to_string(&sentinel).unwrap(), "replacement");
}

#[cfg(unix)]
#[test]
fn shutdown_uses_retained_root_when_ancestor_is_substituted_before_directory_removal() {
    let grandparent = tempfile::tempdir().unwrap();
    let parent = grandparent.path().join("parent");
    let root = parent.join("root");
    fs::create_dir_all(&root).unwrap();
    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    let detached = grandparent.path().join("detached-parent");
    let replacement_parent = grandparent.path().join("replacement-parent");
    let replacement_root = replacement_parent.join("root");
    let sentinel = replacement_root.join("sentinel");
    let installed_sentinel = parent.join("root").join("sentinel");
    let sentinel_for_hook = sentinel.clone();
    set_fs_test_hook(TestHookPoint::BeforeProcessDirectoryRemove, move || {
        fs::rename(&parent, &detached).unwrap();
        fs::create_dir_all(&replacement_root).unwrap();
        fs::write(sentinel_for_hook, "replacement").unwrap();
        fs::rename(replacement_parent, &parent).unwrap();
    });

    store.shutdown().unwrap();

    assert_eq!(
        fs::read_to_string(installed_sentinel).unwrap(),
        "replacement"
    );
}

#[cfg(unix)]
#[test]
fn shutdown_preserves_lease_substituted_immediately_before_removal() {
    let root = tempfile::tempdir().unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let lease = store.lease_path();
    let retained = root.path().join("retained-lease");
    let canonical = lease.clone();
    let canonical_for_hook = canonical.clone();
    set_fs_test_hook(TestHookPoint::BeforeLeaseRemove, move || {
        fs::rename(&canonical_for_hook, retained).unwrap();
        fs::write(canonical_for_hook, "replacement").unwrap();
    });

    assert!(store.shutdown().is_err());
    assert_eq!(fs::read_to_string(canonical).unwrap(), "replacement");
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
fn shutdown_unlinks_a_hard_link_without_touching_its_other_name() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    fs::hard_link(&sentinel, store.process_dir().join("link")).unwrap();

    store.shutdown().unwrap();

    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[test]
fn store_creates_random_private_child_and_holds_lease() {
    let root = tempfile::tempdir().unwrap();
    let canonical_root = root.path().canonicalize().unwrap();
    let store = ArtifactStore::for_test(root.path(), 64 * 1024 * 1024).unwrap();
    assert!(store.process_dir().starts_with(&canonical_root));
    assert_ne!(store.process_dir(), canonical_root);
    assert!(store.lease_path().exists());
    assert_eq!(store.process_dir().parent(), Some(canonical_root.as_path()));
    assert_eq!(store.lease_path().parent(), Some(canonical_root.as_path()));
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
    let parent_path = parent.path().canonicalize().unwrap();
    let root = parent_path.join("root");
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
    fs::rename(&root, parent_path.join("detached-root")).unwrap();
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
fn replacing_process_directory_cannot_redirect_read() {
    let root = tempfile::tempdir().unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(root.path(), 1024).unwrap();
    let published = store
        .publish(vec![store.prepare(draft("known")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    let replacement = root.path().join("replacement-process-dir");
    fs::create_dir(&replacement).unwrap();
    fs::write(
        replacement.join(descriptor.local_path().file_name().unwrap()),
        "known",
    )
    .unwrap();
    fs::rename(
        store.process_dir(),
        root.path().join("detached-process-dir"),
    )
    .unwrap();
    fs::rename(replacement, store.process_dir()).unwrap();

    assert_eq!(
        store.read(descriptor.uri()).unwrap_err(),
        ArtifactReadError::IntegrityFailed
    );
    assert_eq!(fs::read_to_string(sentinel).unwrap(), "keep");
}

#[cfg(unix)]
#[test]
fn replacing_root_ancestor_cannot_redirect_read() {
    let grandparent = tempfile::tempdir().unwrap();
    let parent = grandparent.path().join("parent");
    let root = parent.join("root");
    fs::create_dir_all(&root).unwrap();
    let outside = tempfile::tempdir().unwrap();
    let sentinel = outside.path().join("sentinel");
    fs::write(&sentinel, "keep").unwrap();
    let store = ArtifactStore::for_test(&root, 1024).unwrap();
    let canonical_root = root.canonicalize().unwrap();
    let published = store
        .publish(vec![store.prepare(draft("known")).unwrap()])
        .unwrap();
    let descriptor = &published.descriptors()[0];
    let relative = descriptor
        .local_path()
        .strip_prefix(&canonical_root)
        .unwrap();
    let replacement_parent = grandparent.path().join("replacement-parent");
    let replacement_root = replacement_parent.join("root");
    let redirected = replacement_root.join(relative);
    fs::create_dir_all(redirected.parent().unwrap()).unwrap();
    fs::write(&redirected, "known").unwrap();
    fs::rename(&parent, grandparent.path().join("detached-parent")).unwrap();
    fs::rename(replacement_parent, &parent).unwrap();

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
