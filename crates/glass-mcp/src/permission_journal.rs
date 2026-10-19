//! write-ahead ACL evidence owned by the normal backend process.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use glass_core::Deadline;
use glass_macos::private_session::{
    ProcessIdentity, account_processes, boot_identity, session_exists,
};
use glass_macos::workspace_access::{PermissionChange, RecordChanges};
use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};

const LIMIT: u64 = 64 * 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Header {
    version: u32,
    owner: u32,
    worker: u32,
    account: String,
    baseline: Vec<ProcessIdentity>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    boot: Option<[u8; 16]>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    session: Option<u32>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Event {
    Header { value: Header },
    Prepared { id: u64, change: PermissionChange },
    Settled { id: u64 },
    Closed,
}

pub(crate) struct PermissionJournal {
    root: PathBuf,
    directory: File,
    file: File,
    header: Header,
    pending: BTreeMap<u64, PermissionChange>,
    next: u64,
    closed: bool,
    loaded: bool,
    poisoned: bool,
    admission: Option<ProcessIdentity>,
}

pub(crate) fn private_root(path: &Path, owner: u32) -> io::Result<File> {
    use std::os::unix::fs::DirBuilderExt;
    let mut builder = std::fs::DirBuilder::new();
    builder
        .recursive(true)
        .mode(0o700)
        .create(path)
        .or_else(|failure| {
            if failure.kind() == io::ErrorKind::AlreadyExists {
                Ok(())
            } else {
                Err(failure)
            }
        })?;
    let directory = File::from(open(
        path,
        OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::empty(),
    )?);
    let metadata = directory.metadata()?;
    if metadata.uid() != owner || metadata.mode() & 0o077 != 0 {
        return Err(io::Error::new(
            io::ErrorKind::PermissionDenied,
            "permission recovery directory is not private",
        ));
    }
    Ok(directory)
}

fn prior_work_gone(
    header: &Header,
    current_boot: impl FnOnce() -> io::Result<[u8; 16]>,
    exists: impl FnOnce(u32, u32) -> io::Result<bool>,
) -> io::Result<bool> {
    let Some(boot) = header.boot else {
        return Ok(false);
    };
    if boot != current_boot()? {
        return Ok(true);
    }
    match header.session {
        Some(session) => exists(session, header.worker).map(|present| !present),
        None => Ok(false),
    }
}

impl PermissionJournal {
    pub(crate) fn root() -> io::Result<PathBuf> {
        directories::BaseDirs::new()
            .map(|dirs| {
                dirs.data_local_dir()
                    .join("glass/macos/permission-recovery")
            })
            .ok_or_else(|| io::Error::other("permission recovery directory is unavailable"))
    }

    pub(crate) fn open(
        root: &Path,
        worker: u32,
        account: String,
        session: Option<u32>,
        admission: Option<ProcessIdentity>,
    ) -> io::Result<Self> {
        let owner = rustix::process::geteuid().as_raw();
        if owner == 0
            || owner != rustix::process::getuid().as_raw()
            || worker == 0
            || worker == owner
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission recovery requires its ordinary owner",
            ));
        }
        let directory = private_root(root, owner)?;
        let file = File::from(rustix::fs::openat(
            &directory,
            "pending.jsonl",
            OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        let metadata = file.metadata()?;
        if !metadata.is_file()
            || metadata.uid() != owner
            || metadata.mode() & 0o077 != 0
            || metadata.nlink() != 1
            || metadata.len() > LIMIT
        {
            return Err(io::Error::new(
                io::ErrorKind::PermissionDenied,
                "permission recovery file is not private",
            ));
        }
        glass_macos::workspace_access::require_owner_private(&directory)?;
        glass_macos::workspace_access::require_owner_private(&file)?;
        fs4::FileExt::try_lock(&file)
            .map_err(|_| io::Error::other("permission recovery is already in use"))?;
        directory.sync_all()?;
        let mut result = Self {
            root: root.to_path_buf(),
            directory,
            file,
            header: Header {
                version: 2,
                owner,
                worker,
                account,
                baseline: Vec::new(),
                boot: Some(boot_identity()?),
                session,
            },
            pending: BTreeMap::new(),
            next: 1,
            closed: false,
            loaded: false,
            poisoned: false,
            admission,
        };
        result.read()?;
        if result.loaded && !result.closed {
            result.recover(Deadline::from_millis(8000))?;
        }
        result.header.version = 2;
        result.header.boot = Some(boot_identity()?);
        result.header.session = session;
        result.header.baseline = account_processes(worker)?;
        result.reset()?;
        Ok(result)
    }

    fn read(&mut self) -> io::Result<()> {
        self.file.seek(SeekFrom::Start(0))?;
        let mut bytes = Vec::new();
        Read::by_ref(&mut self.file)
            .take(LIMIT + 1)
            .read_to_end(&mut bytes)?;
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other(
                "permission recovery journal exceeds its bound",
            ));
        }
        if bytes.is_empty() {
            return Ok(());
        }
        let end = bytes
            .iter()
            .rposition(|&byte| byte == b'\n')
            .ok_or_else(|| {
                io::Error::new(
                    io::ErrorKind::InvalidData,
                    "permission recovery header is incomplete",
                )
            })?
            + 1;
        let mut header = None;
        for line in bytes[..end]
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
        {
            if self.closed {
                return Err(io::Error::new(
                    io::ErrorKind::InvalidData,
                    "permission events follow a closed lease",
                ));
            }
            let event: Event = serde_json::from_slice(line).map_err(io::Error::other)?;
            match event {
                Event::Header { value } if header.is_none() && self.pending.is_empty() => {
                    if !matches!(value.version, 1 | 2)
                        || (value.version == 2 && value.boot.is_none())
                        || (value.version == 1 && (value.boot.is_some() || value.session.is_some()))
                        || value.session.is_some_and(|id| id == 0 || id == u32::MAX)
                        || value.owner != self.header.owner
                        || value.worker == 0
                        || value.worker == value.owner
                        || value.account.is_empty()
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "permission recovery account binding changed",
                        ));
                    }
                    header = Some(value);
                }
                Event::Prepared { id, change } if header.is_some() && id == self.next => {
                    self.pending.insert(id, change);
                    self.next = self
                        .next
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("permission recovery sequence overflow"))?;
                }
                Event::Settled { id } if header.is_some() && id < self.next => {
                    if self.pending.remove(&id).is_none() {
                        return Err(io::Error::new(
                            io::ErrorKind::InvalidData,
                            "permission settlement has no pending change",
                        ));
                    }
                }
                Event::Closed if header.is_some() && self.pending.is_empty() => {
                    self.closed = true;
                }
                _ => {
                    return Err(io::Error::new(
                        io::ErrorKind::InvalidData,
                        "invalid permission recovery event sequence",
                    ));
                }
            }
        }
        let header = header.ok_or_else(|| {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "permission recovery header missing",
            )
        })?;
        if (header.worker != self.header.worker || header.account != self.header.account)
            && (!self.closed || end != bytes.len())
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidData,
                "permission recovery account binding changed",
            ));
        }
        // A fully closed lease has no recovery work tying the next lease to its account.
        if !self.closed {
            self.header = header;
        }
        self.loaded = true;
        // An incomplete final event was never followed by a successful write-ahead sync.
        self.file.set_len(end as u64)?;
        self.file.sync_all()?;
        self.file.seek(SeekFrom::End(0))?;
        Ok(())
    }

    fn append(&mut self, event: &Event) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other(
                "permission journal write previously failed",
            ));
        }
        let mut bytes = serde_json::to_vec(event).map_err(io::Error::other)?;
        bytes.push(b'\n');
        let settlement_reserve = if matches!(event, Event::Prepared { .. }) {
            (self.pending.len() as u64)
                .saturating_add(1)
                .saturating_mul(64)
                .saturating_add(128)
        } else {
            0
        };
        if self
            .file
            .metadata()?
            .len()
            .saturating_add(bytes.len() as u64)
            .saturating_add(settlement_reserve)
            > LIMIT
        {
            return Err(io::Error::other(
                "permission recovery journal exceeds its bound",
            ));
        }
        let result = (|| {
            self.file.seek(SeekFrom::End(0))?;
            self.file.write_all(&bytes)?;
            self.file.sync_all()
        })();
        if result.is_err() {
            self.poisoned = true;
        }
        result
    }

    fn reset(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() {
            return Err(io::Error::other("permission recovery is incomplete"));
        }
        let temporary = format!("checkpoint-{:016x}", rand::random::<u64>());
        let mut checkpoint = File::from(rustix::fs::openat(
            &self.directory,
            &temporary,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        let result = (|| {
            glass_macos::workspace_access::require_owner_private(&checkpoint)?;
            fs4::FileExt::try_lock(&checkpoint).map_err(io::Error::other)?;
            serde_json::to_writer(
                &mut checkpoint,
                &Event::Header {
                    value: self.header.clone(),
                },
            )
            .map_err(io::Error::other)?;
            checkpoint.write_all(b"\n")?;
            checkpoint.sync_all()?;
            rustix::fs::renameat(
                &self.directory,
                &temporary,
                &self.directory,
                "pending.jsonl",
            )?;
            self.directory.sync_all()
        })();
        if result.is_err() {
            let _ = rustix::fs::unlinkat(&self.directory, &temporary, rustix::fs::AtFlags::empty());
            return result;
        }
        self.file = checkpoint;
        self.next = 1;
        self.closed = false;
        Ok(())
    }

    pub(crate) fn protected_root(&self) -> &Path {
        &self.root
    }

    /// A failed bootstrap must stop its broker before checking the child-process baseline.
    pub(crate) fn wait_admission_exit(&self, deadline: Deadline) -> io::Result<()> {
        while let Some(admission) = &self.admission {
            if !account_processes(self.header.worker)?.contains(admission) {
                return Ok(());
            }
            if deadline.has_passed() {
                return Err(io::Error::other(
                    "private bootstrap broker is still running; recovery deferred",
                ));
            }
            std::thread::sleep(
                deadline
                    .remaining()
                    .unwrap_or(Duration::from_millis(100))
                    .min(Duration::from_millis(100)),
            );
        }
        Ok(())
    }

    pub(crate) fn wait_idle(&self, deadline: Deadline) -> io::Result<()> {
        let mut stable = 0;
        loop {
            let current = account_processes(self.header.worker)?;
            if current.iter().all(|process| {
                self.header.baseline.contains(process) || self.admission.as_ref() == Some(process)
            }) {
                stable += 1;
                if stable == 2 {
                    return Ok(());
                }
            } else {
                stable = 0;
                if prior_work_gone(&self.header, boot_identity, session_exists)? {
                    return Ok(());
                }
            }
            if deadline.has_passed() {
                return Err(io::Error::other(
                    "private account still has processes created during the permission lease; recovery deferred",
                ));
            }
            std::thread::sleep(
                deadline
                    .remaining()
                    .map_or(Duration::from_millis(100), |left| {
                        left.min(Duration::from_millis(100))
                    }),
            );
        }
    }

    pub(crate) fn recover(&mut self, deadline: Deadline) -> io::Result<()> {
        self.wait_idle(deadline)?;
        let ids: Vec<_> = self.pending.keys().copied().rev().collect();
        for id in ids {
            if deadline.has_passed() {
                return Err(io::Error::other("permission recovery deadline elapsed"));
            }
            self.pending[&id].restore(self.header.worker)?;
            self.settled(id)?;
        }
        Ok(())
    }

    pub(crate) fn finish(&mut self, deadline: Deadline) -> io::Result<()> {
        self.recover(deadline)?;
        self.append(&Event::Closed)?;
        self.closed = true;
        Ok(())
    }
}

impl RecordChanges for PermissionJournal {
    fn prepared(&mut self, change: &PermissionChange) -> io::Result<u64> {
        if self.closed {
            return Err(io::Error::other("permission lease is already closed"));
        }
        let id = self.next;
        let next = self
            .next
            .checked_add(1)
            .ok_or_else(|| io::Error::other("permission recovery sequence overflow"))?;
        self.append(&Event::Prepared {
            id,
            change: change.clone(),
        })?;
        self.pending.insert(id, change.clone());
        self.next = next;
        Ok(id)
    }

    fn settled(&mut self, id: u64) -> io::Result<()> {
        if !self.pending.contains_key(&id) {
            return Ok(());
        }
        self.append(&Event::Settled { id })?;
        self.pending.remove(&id);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    fn test_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn journal(root: &Path) -> PermissionJournal {
        PermissionJournal::open(root, u32::MAX, "test-account".into(), None, None).unwrap()
    }

    #[test]
    fn an_empty_journal_does_not_close_while_its_bootstrap_broker_can_spawn_work() {
        let root = test_root();
        let admission = glass_macos::private_session::current_process().unwrap();
        let mut journal = PermissionJournal::open(
            root.path(),
            u32::MAX,
            "test-account".into(),
            None,
            Some(admission),
        )
        .unwrap();
        journal.header.worker = rustix::process::geteuid().as_raw();
        assert!(journal.pending.is_empty());
        assert!(
            journal
                .wait_admission_exit(Deadline::from_millis(5))
                .is_err()
        );
        assert!(!journal.closed);
        let events = std::fs::read_to_string(root.path().join("pending.jsonl")).unwrap();
        assert!(!events.contains("Closed"));
    }

    #[test]
    fn a_closed_lease_can_reopen_with_a_fresh_baseline() {
        let root = test_root();
        let mut first = journal(root.path());
        first.finish(Deadline::from_millis(1000)).unwrap();
        drop(first);
        let next = journal(root.path());
        assert!(!next.closed && next.pending.is_empty());
    }

    fn change() -> PermissionChange {
        serde_json::from_value(serde_json::json!({
            "identity": glass_macos::workspace_access::FileIdentity::default(),
            "before": [],
            "additions": [],
        }))
        .unwrap()
    }

    fn closed_journal(root: &Path) {
        let mut journal = journal(root);
        for _ in 0..3 {
            let id = journal.prepared(&change()).unwrap();
            journal.settled(id).unwrap();
        }
        journal.finish(Deadline::from_millis(1000)).unwrap();
    }

    fn assert_rebinding_refused(root: &Path) {
        let path = root.join("pending.jsonl");
        let before = std::fs::read(&path).unwrap();
        assert!(
            PermissionJournal::open(root, u32::MAX - 1, "another-account".into(), None, None)
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn closed_history_can_start_a_different_account_lease() {
        for (worker, account) in [
            (u32::MAX, "another-account"),
            (u32::MAX - 1, "test-account"),
            (u32::MAX - 1, "another-account"),
        ] {
            let root = test_root();
            closed_journal(root.path());
            let next = PermissionJournal::open(root.path(), worker, account.into(), Some(42), None)
                .unwrap();
            assert_eq!(
                (next.header.worker, next.header.account.as_str()),
                (worker, account)
            );
            assert_eq!(next.header.session, Some(42));
            assert!(!next.closed && next.pending.is_empty());
            let bytes = std::fs::read(root.path().join("pending.jsonl")).unwrap();
            let Event::Header { value } = serde_json::from_slice(&bytes).unwrap() else {
                panic!("fresh lease must retain its requested account header");
            };
            assert_eq!(value, next.header);
        }
    }

    #[test]
    fn unfinished_account_recovery_cannot_be_rebound() {
        for settled in [false, true] {
            let root = test_root();
            let mut first = journal(root.path());
            let id = first.prepared(&change()).unwrap();
            if settled {
                first.settled(id).unwrap();
            }
            drop(first);
            let path = root.path().join("pending.jsonl");
            let before = std::fs::read(&path).unwrap();
            let error = PermissionJournal::open(
                root.path(),
                u32::MAX - 1,
                "another-account".into(),
                None,
                None,
            )
            .err()
            .unwrap();
            assert_eq!(
                error.to_string(),
                "permission recovery account binding changed"
            );
            assert_eq!(std::fs::read(path).unwrap(), before);
        }
    }

    #[test]
    fn closed_legacy_history_can_start_a_new_account_lease() {
        let root = test_root();
        closed_journal(root.path());
        let path = root.path().join("pending.jsonl");
        let text = std::fs::read_to_string(&path).unwrap();
        let (first, rest) = text.split_once('\n').unwrap();
        let Event::Header { mut value } = serde_json::from_str(first).unwrap() else {
            panic!("expected the old lease header");
        };
        value.version = 1;
        value.boot = None;
        value.session = None;
        std::fs::write(
            &path,
            format!(
                "{}\n{rest}",
                serde_json::to_string(&Event::Header { value }).unwrap()
            ),
        )
        .unwrap();
        let next = PermissionJournal::open(
            root.path(),
            u32::MAX - 1,
            "another-account".into(),
            Some(42),
            None,
        )
        .unwrap();
        assert_eq!(next.header.version, 2);
        assert_eq!(next.header.worker, u32::MAX - 1);
        assert_eq!(next.header.account, "another-account");
        assert_eq!(next.header.session, Some(42));
    }

    #[test]
    fn a_closed_marker_with_unsettled_changes_cannot_authorize_rebinding() {
        let root = test_root();
        let mut first = journal(root.path());
        first.prepared(&change()).unwrap();
        first.append(&Event::Closed).unwrap();
        drop(first);
        assert_rebinding_refused(root.path());
    }

    #[test]
    fn orphan_or_duplicate_settlements_cannot_authorize_rebinding() {
        for prepared in [false, true] {
            let root = test_root();
            let mut first = journal(root.path());
            let id = if prepared {
                let id = first.prepared(&change()).unwrap();
                first.settled(id).unwrap();
                id
            } else {
                0
            };
            first.append(&Event::Settled { id }).unwrap();
            first.append(&Event::Closed).unwrap();
            drop(first);
            assert_rebinding_refused(root.path());
        }
    }

    #[test]
    fn records_after_a_closed_lease_cannot_authorize_rebinding() {
        for tail in [
            b"{\"kind\":\"Settled\",\"id\":1}\n".as_slice(),
            b"{\"kind\":\"Prepared\",",
        ] {
            let root = test_root();
            closed_journal(root.path());
            std::fs::OpenOptions::new()
                .append(true)
                .open(root.path().join("pending.jsonl"))
                .unwrap()
                .write_all(tail)
                .unwrap();
            assert_rebinding_refused(root.path());
        }
    }

    #[test]
    fn invalid_closed_headers_cannot_authorize_rebinding() {
        for field in ["owner", "worker", "account", "version", "boot", "session"] {
            let root = test_root();
            closed_journal(root.path());
            let path = root.path().join("pending.jsonl");
            let text = std::fs::read_to_string(&path).unwrap();
            let (first, rest) = text.split_once('\n').unwrap();
            let mut row: serde_json::Value = serde_json::from_str(first).unwrap();
            row["value"][field] = match field {
                "owner" => serde_json::json!(rustix::process::getuid().as_raw() + 1),
                "account" => serde_json::json!(""),
                "boot" => serde_json::Value::Null,
                _ => serde_json::json!(0),
            };
            std::fs::write(
                &path,
                format!("{}\n{rest}", serde_json::to_string(&row).unwrap()),
            )
            .unwrap();
            assert_rebinding_refused(root.path());
        }
    }

    #[test]
    fn simultaneous_journals_cannot_own_the_same_file() {
        let root = test_root();
        let _first = journal(root.path());
        assert!(
            PermissionJournal::open(root.path(), u32::MAX, "test-account".into(), None, None)
                .is_err()
        );
    }

    #[test]
    fn checkpoint_replaces_the_file_without_releasing_its_lease() {
        let root = test_root();
        let mut first = journal(root.path());
        let old = first.file.metadata().unwrap().ino();
        first.reset().unwrap();
        assert_ne!(first.file.metadata().unwrap().ino(), old);
        assert_eq!(
            first.file.metadata().unwrap().ino(),
            std::fs::metadata(root.path().join("pending.jsonl"))
                .unwrap()
                .ino()
        );
        assert!(
            PermissionJournal::open(root.path(), u32::MAX, "test-account".into(), None, None)
                .is_err()
        );
    }

    #[test]
    fn partial_final_record_is_trimmed_without_inventing_a_mutation() {
        let root = test_root();
        drop(journal(root.path()));
        std::fs::OpenOptions::new()
            .append(true)
            .open(root.path().join("pending.jsonl"))
            .unwrap()
            .write_all(b"{\"kind\":\"Prepared\",")
            .unwrap();
        assert!(journal(root.path()).pending.is_empty());
    }

    #[test]
    fn a_complete_invalid_record_is_retained_and_refused() {
        let root = test_root();
        drop(journal(root.path()));
        let path = root.path().join("pending.jsonl");
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(b"invalid\n")
            .unwrap();
        let before = std::fs::read(&path).unwrap();
        assert!(
            PermissionJournal::open(root.path(), u32::MAX, "test-account".into(), None, None)
                .is_err()
        );
        assert_eq!(std::fs::read(path).unwrap(), before);
    }

    #[test]
    fn changed_account_binding_is_refused_without_rewriting_evidence() {
        let root = test_root();
        drop(journal(root.path()));
        let before = std::fs::read(root.path().join("pending.jsonl")).unwrap();
        assert!(
            PermissionJournal::open(
                root.path(),
                u32::MAX,
                "different-account".into(),
                None,
                None
            )
            .is_err()
        );
        assert_eq!(
            std::fs::read(root.path().join("pending.jsonl")).unwrap(),
            before
        );
    }

    #[test]
    fn symlinked_file_is_refused() {
        let root = test_root();
        let target = root.path().join("target");
        std::fs::write(&target, "sentinel").unwrap();
        std::os::unix::fs::symlink(&target, root.path().join("pending.jsonl")).unwrap();
        assert!(
            PermissionJournal::open(root.path(), u32::MAX, "test-account".into(), None, None)
                .is_err()
        );
        assert_eq!(std::fs::read_to_string(target).unwrap(), "sentinel");
    }

    #[test]
    fn readable_by_others_journal_is_refused() {
        let root = test_root();
        drop(journal(root.path()));
        let path = root.path().join("pending.jsonl");
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            PermissionJournal::open(root.path(), u32::MAX, "test-account".into(), None, None)
                .is_err()
        );
    }
    fn header() -> Header {
        Header {
            version: 2,
            owner: 501,
            worker: 504,
            account: "example".into(),
            baseline: Vec::new(),
            boot: Some([1; 16]),
            session: Some(42),
        }
    }

    #[test]
    fn a_different_boot_proves_old_work_gone_without_querying_reused_session_ids() {
        assert!(
            prior_work_gone(
                &header(),
                || Ok([2; 16]),
                |_, _| panic!("old session IDs are scoped to their boot")
            )
            .unwrap()
        );
    }

    #[test]
    fn a_retired_login_can_recover_despite_new_login_processes() {
        assert!(prior_work_gone(&header(), || Ok([1; 16]), |_, _| Ok(false)).unwrap());
        assert!(!prior_work_gone(&header(), || Ok([1; 16]), |_, _| Ok(true)).unwrap());
    }

    #[test]
    fn unavailable_boot_or_session_queries_cannot_authorize_recovery() {
        assert!(
            prior_work_gone(
                &header(),
                || Err(io::Error::other("unknown boot")),
                |_, _| Ok(false)
            )
            .is_err()
        );
        assert!(
            prior_work_gone(
                &header(),
                || Ok([1; 16]),
                |_, _| Err(io::Error::other("unknown session"))
            )
            .is_err()
        );
    }

    #[test]
    fn legacy_leases_keep_the_process_baseline_gate() {
        let mut old = header();
        old.version = 1;
        old.boot = None;
        old.session = None;
        assert!(
            !prior_work_gone(
                &old,
                || panic!("legacy boot unavailable"),
                |_, _| panic!("legacy session unavailable")
            )
            .unwrap()
        );
    }
}
