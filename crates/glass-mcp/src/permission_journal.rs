//! write-ahead ACL evidence owned by the normal backend process.

use std::collections::BTreeMap;
use std::fs::File;
use std::io::{self, Read, Seek, SeekFrom, Write};
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use glass_core::Deadline;
use glass_macos::private_session::work::{self, WorkObserver, WorkScope, WorkSnapshot};
use glass_macos::private_session::{
    ProcessIdentity, account_processes, boot_identity, session_exists,
};
use glass_macos::workspace_access::outputs::OutputScope;
use glass_macos::workspace_access::{PermissionChange, RecordChanges};
use rustix::fs::{Mode, OFlags, open};
use serde::{Deserialize, Serialize};

const LIMIT: u64 = 64 * 1024 * 1024;
type IdleInspector<'a> =
    dyn FnMut(&[ProcessIdentity], Deadline) -> io::Result<Vec<ProcessIdentity>> + 'a;
type OutputInspector<'a> = dyn FnMut(&[OutputScope], Deadline) -> io::Result<()> + 'a;

#[derive(Default)]
struct IdleSamples {
    previous: Option<Vec<ProcessIdentity>>,
}

impl IdleSamples {
    fn observe(
        &mut self,
        current: &[ProcessIdentity],
        unknown: &[ProcessIdentity],
        idle: &[ProcessIdentity],
    ) -> io::Result<bool> {
        if idle
            .iter()
            .enumerate()
            .any(|(index, process)| !unknown.contains(process) || idle[..index].contains(process))
        {
            return Err(io::Error::other(
                "idle-service response included an unrequested or duplicate process",
            ));
        }
        if unknown.iter().any(|process| !idle.contains(process)) {
            self.previous = None;
            return Ok(false);
        }
        let stable = self.previous.as_deref() == Some(current);
        self.previous = Some(current.to_vec());
        Ok(stable)
    }
}

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    work: Option<WorkScope>,
}

#[derive(Serialize, Deserialize)]
#[serde(tag = "kind", deny_unknown_fields)]
enum Event {
    Header {
        value: Header,
    },
    Prepared {
        id: u64,
        change: PermissionChange,
    },
    Settled {
        id: u64,
    },
    Closed,
    WorkStarted,
    WorkObserved {
        snapshot: WorkSnapshot,
        start_finished: bool,
    },
    OutputPending {
        id: u64,
        scope: OutputScope,
    },
    OutputReconciled {
        id: u64,
    },
    Checkpoint {
        value: Checkpoint,
    },
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct Checkpoint {
    next: u64,
    next_output: u64,
    pending: Vec<(u64, PermissionChange)>,
    outputs: Vec<(u64, OutputScope)>,
}

impl Checkpoint {
    fn validate(&self, header: &Header) -> io::Result<()> {
        let invalid = || {
            io::Error::new(
                io::ErrorKind::InvalidData,
                "invalid output recovery checkpoint",
            )
        };
        if header.version != 4
            || self.next == 0
            || self.next_output == 0
            || self.outputs.is_empty()
            || self.outputs.len() > 64
        {
            return Err(invalid());
        }
        let mut ids = std::collections::BTreeSet::new();
        let boundary = &self.outputs[0].1;
        for (id, scope) in &self.outputs {
            if *id == 0 || *id >= self.next_output || !ids.insert(*id) {
                return Err(invalid());
            }
            scope.validate_binding(header.owner, header.worker)?;
            if !scope.same_protection(boundary) {
                return Err(invalid());
            }
        }
        ids.clear();
        for (id, change) in &self.pending {
            if *id == 0 || *id >= self.next || !ids.insert(*id) {
                return Err(invalid());
            }
            boundary.validate_guard(change)?;
        }
        Ok(())
    }
}

pub(crate) struct PermissionJournal {
    root: PathBuf,
    directory: File,
    file: File,
    header: Header,
    pending: BTreeMap<u64, PermissionChange>,
    outputs: BTreeMap<u64, OutputScope>,
    next_output: u64,
    next: u64,
    closed: bool,
    loaded: bool,
    poisoned: bool,
    admission: Option<ProcessIdentity>,
}

impl Drop for PermissionJournal {
    fn drop(&mut self) {
        // A concurrent fork can retain this open file description until exec.
        if let Err(error) = fs4::FileExt::unlock(&self.file) {
            eprintln!("glass: permission journal unlock failed: {error}");
        }
    }
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

    #[cfg(test)]
    pub(crate) fn open(
        root: &Path,
        worker: u32,
        account: String,
        session: Option<u32>,
        admission: Option<ProcessIdentity>,
    ) -> io::Result<Self> {
        Self::open_with(root, worker, account, session, admission, &mut |_, _| {
            Ok(Vec::new())
        })
    }

    #[cfg(test)]
    pub(crate) fn open_with(
        root: &Path,
        worker: u32,
        account: String,
        session: Option<u32>,
        admission: Option<ProcessIdentity>,
        inspect: &mut IdleInspector<'_>,
    ) -> io::Result<Self> {
        Self::open_with_outputs(
            root,
            worker,
            account,
            session,
            admission,
            inspect,
            &mut |_, _| Err(io::Error::other("output recovery executor is unavailable")),
        )
    }

    pub(crate) fn open_with_outputs(
        root: &Path,
        worker: u32,
        account: String,
        session: Option<u32>,
        admission: Option<ProcessIdentity>,
        inspect: &mut IdleInspector<'_>,
        outputs: &mut OutputInspector<'_>,
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
                work: None,
            },
            pending: BTreeMap::new(),
            outputs: BTreeMap::new(),
            next_output: 1,
            next: 1,
            closed: false,
            loaded: false,
            poisoned: false,
            admission,
        };
        result.read()?;
        if result.loaded && !result.closed {
            // A recovery inspector may need one launchd-throttled activation before observing idle.
            result.recover_with_outputs(Deadline::from_millis(30_000), inspect, outputs)?;
        }
        result.header.version = 4;
        result.header.boot = Some(boot_identity()?);
        result.header.session = session;
        let baseline = work::snapshot(worker)?;
        result.header.baseline = baseline.iter().map(|origin| origin.process).collect();
        result.header.work = Some(WorkScope {
            baseline,
            work: WorkSnapshot::default(),
            launch_pending: false,
        });
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
        for (index, line) in bytes[..end]
            .split(|&byte| byte == b'\n')
            .filter(|line| !line.is_empty())
            .enumerate()
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
                    if !matches!(value.version, 1..=4)
                        || (value.version >= 2 && value.boot.is_none())
                        || (value.version == 1 && (value.boot.is_some() || value.session.is_some()))
                        || (value.work.is_some() != (value.version >= 3))
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
                    if let Some(scope) = &value.work {
                        scope.validate()?;
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
                Event::Closed
                    if header.is_some() && self.pending.is_empty() && self.outputs.is_empty() =>
                {
                    self.closed = true;
                }
                Event::WorkStarted if header.as_ref().is_some_and(|header| header.version >= 3) => {
                    let scope = header
                        .as_mut()
                        .and_then(|header| header.work.as_mut())
                        .ok_or_else(|| io::Error::other("work scope missing"))?;
                    if scope.launch_pending {
                        return Err(io::Error::other("duplicate pending application launch"));
                    }
                    scope.launch_pending = true;
                }
                Event::WorkObserved {
                    snapshot,
                    start_finished,
                } if header.as_ref().is_some_and(|header| header.version >= 3) => {
                    let scope = header
                        .as_mut()
                        .and_then(|header| header.work.as_mut())
                        .ok_or_else(|| io::Error::other("work scope missing"))?;
                    if start_finished && !scope.launch_pending {
                        return Err(io::Error::other(
                            "application launch settlement was not prepared",
                        ));
                    }
                    scope.merge(snapshot, start_finished)?;
                }
                Event::OutputPending { id, scope }
                    if header.as_ref().is_some_and(|header| header.version == 4)
                        && id == self.next_output
                        && self.outputs.len() < 64 =>
                {
                    let binding = header
                        .as_ref()
                        .ok_or_else(|| io::Error::other("output authority has no header"))?;
                    scope.validate_binding(binding.owner, binding.worker)?;
                    if self
                        .outputs
                        .values()
                        .any(|current| !current.same_protection(&scope))
                    {
                        return Err(io::Error::other(
                            "protected output authority changed during replay",
                        ));
                    }
                    self.outputs.insert(id, scope);
                    self.next_output = self
                        .next_output
                        .checked_add(1)
                        .ok_or_else(|| io::Error::other("output authority sequence overflow"))?;
                }
                Event::Checkpoint { value }
                    if index == 1 && header.as_ref().is_some_and(|header| header.version == 4) =>
                {
                    value.validate(
                        header
                            .as_ref()
                            .ok_or_else(|| io::Error::other("checkpoint header missing"))?,
                    )?;
                    self.next = value.next;
                    self.next_output = value.next_output;
                    self.pending = value.pending.into_iter().collect();
                    self.outputs = value.outputs.into_iter().collect();
                }
                Event::OutputReconciled { id }
                    if header.as_ref().is_some_and(|header| header.version == 4)
                        && self.pending.values().all(|change| !change.has_allow()) =>
                {
                    if self.outputs.remove(&id).is_none() {
                        return Err(io::Error::other(
                            "output settlement has no pending authority",
                        ));
                    }
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
        let settlement_reserve = if !matches!(
            event,
            Event::Settled { .. } | Event::OutputReconciled { .. } | Event::Closed
        ) {
            (self.pending.len() as u64)
                .saturating_add(u64::from(matches!(event, Event::Prepared { .. })))
                .saturating_mul(64)
                .saturating_add(
                    (self.outputs.len() as u64)
                        .saturating_add(1)
                        .saturating_mul(64),
                )
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

    fn replace_file(
        &mut self,
        bytes: &[u8],
        sync_directory: impl FnOnce(&File) -> io::Result<()>,
    ) -> io::Result<()> {
        if self.poisoned {
            return Err(io::Error::other("permission journal is poisoned"));
        }
        let temporary = format!("checkpoint-{:016x}", rand::random::<u64>());
        let mut checkpoint = File::from(rustix::fs::openat(
            &self.directory,
            &temporary,
            OFlags::RDWR | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
            Mode::from_raw_mode(0o600),
        )?);
        let prepared = (|| {
            glass_macos::workspace_access::require_owner_private(&checkpoint)?;
            fs4::FileExt::try_lock(&checkpoint).map_err(io::Error::other)?;
            checkpoint.write_all(bytes)?;
            checkpoint.sync_all()?;
            rustix::fs::renameat(
                &self.directory,
                &temporary,
                &self.directory,
                "pending.jsonl",
            )?;
            Ok::<_, io::Error>(())
        })();
        if let Err(error) = prepared {
            let _ = rustix::fs::unlinkat(&self.directory, &temporary, rustix::fs::AtFlags::empty());
            return Err(error);
        }
        // The namespace now names this locked inode, even if the directory sync fails.
        self.file = checkpoint;
        if let Err(error) = sync_directory(&self.directory) {
            self.poisoned = true;
            return Err(error);
        }
        Ok(())
    }

    fn reset(&mut self) -> io::Result<()> {
        if !self.pending.is_empty() || !self.outputs.is_empty() {
            return Err(io::Error::other("permission recovery is incomplete"));
        }
        let mut bytes = serde_json::to_vec(&Event::Header {
            value: self.header.clone(),
        })
        .map_err(io::Error::other)?;
        bytes.push(b'\n');
        if bytes.len() as u64 > LIMIT {
            return Err(io::Error::other("permission header exceeds its bound"));
        }
        self.replace_file(&bytes, File::sync_all)?;
        self.next = 1;
        self.next_output = 1;
        self.closed = false;
        Ok(())
    }

    fn checkpoint(&mut self) -> io::Result<()> {
        if self.header.version != 4
            || self.outputs.is_empty()
            || self.file.metadata()?.len() <= LIMIT / 2
        {
            return Ok(());
        }
        let value = Checkpoint {
            next: self.next,
            next_output: self.next_output,
            pending: self
                .pending
                .iter()
                .map(|(id, change)| (*id, change.clone()))
                .collect(),
            outputs: self
                .outputs
                .iter()
                .map(|(id, scope)| (*id, scope.clone()))
                .collect(),
        };
        value.validate(&self.header)?;
        let mut bytes = serde_json::to_vec(&Event::Header {
            value: self.header.clone(),
        })
        .map_err(io::Error::other)?;
        bytes.push(b'\n');
        serde_json::to_writer(&mut bytes, &Event::Checkpoint { value })
            .map_err(io::Error::other)?;
        bytes.push(b'\n');
        let reserve = (self.pending.len() as u64)
            .saturating_add(self.outputs.len() as u64)
            .saturating_add(1)
            .saturating_mul(64)
            .saturating_add(128);
        if (bytes.len() as u64).saturating_add(reserve) > LIMIT {
            return Err(io::Error::other("output checkpoint exceeds its bound"));
        }
        self.replace_file(&bytes, File::sync_all)
    }

    pub(crate) fn protected_root(&self) -> &Path {
        &self.root
    }

    pub(crate) fn work_started(&mut self) -> io::Result<()> {
        let mut scope = self
            .header
            .work
            .clone()
            .ok_or_else(|| io::Error::other("private work scope is unavailable"))?;
        if scope.launch_pending {
            return Err(io::Error::other("an application launch is already pending"));
        }
        scope.launch_pending = true;
        self.append(&Event::WorkStarted)?;
        self.header.work = Some(scope);
        Ok(())
    }

    pub(crate) fn output_pending(&mut self, scope: OutputScope) -> io::Result<()> {
        scope.validate_owner(self.header.worker)?;
        if self
            .outputs
            .values()
            .any(|current| !current.same_protection(&scope))
        {
            return Err(io::Error::other(
                "protected output authority changed during the lease",
            ));
        }
        if self.header.version != 4 || self.outputs.len() >= 64 {
            return Err(io::Error::other("output authority cannot be recorded"));
        }
        if self.outputs.values().any(|current| current == &scope) {
            return Ok(());
        }
        let id = self.next_output;
        let next = id
            .checked_add(1)
            .ok_or_else(|| io::Error::other("output authority sequence overflow"))?;
        self.append(&Event::OutputPending {
            id,
            scope: scope.clone(),
        })?;
        self.outputs.insert(id, scope);
        self.next_output = next;
        Ok(())
    }

    pub(crate) fn observe_work(
        &mut self,
        snapshot: WorkSnapshot,
        start_finished: bool,
    ) -> io::Result<()> {
        let mut scope = self
            .header
            .work
            .clone()
            .ok_or_else(|| io::Error::other("private work scope is unavailable"))?;
        if start_finished && !scope.launch_pending {
            return Err(io::Error::other("application launch was not prepared"));
        }
        if scope.merge(snapshot.clone(), start_finished)? {
            self.append(&Event::WorkObserved {
                snapshot,
                start_finished,
            })?;
            self.header.work = Some(scope);
        }
        Ok(())
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

    pub(crate) fn wait_idle_with(
        &mut self,
        deadline: Deadline,
        inspect: &mut IdleInspector<'_>,
    ) -> io::Result<()> {
        if let Some(scope) = self.header.work.clone()
            && !scope.work.roots.is_empty()
        {
            return self.wait_owned_work(&scope, deadline, inspect);
        }
        let mut samples = IdleSamples::default();
        loop {
            if deadline.has_passed() {
                return Err(io::Error::other("permission recovery deadline elapsed"));
            }
            let mut current = account_processes(self.header.worker)?;
            current.sort_by_key(ProcessIdentity::pid);
            let unknown: Vec<_> = current
                .iter()
                .copied()
                .filter(|process| {
                    !self.header.baseline.contains(process)
                        && self.admission.as_ref() != Some(process)
                })
                .collect();
            let idle = if unknown.is_empty() {
                Vec::new()
            } else {
                if prior_work_gone(&self.header, boot_identity, session_exists)? {
                    return Ok(());
                }
                inspect(&unknown, deadline)?
            };
            if deadline.has_passed() {
                return Err(io::Error::other("permission recovery deadline elapsed"));
            }
            if samples.observe(&current, &unknown, &idle)? {
                return Ok(());
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

    fn wait_owned_work(
        &mut self,
        scope: &WorkScope,
        deadline: Deadline,
        inspect: &mut IdleInspector<'_>,
    ) -> io::Result<()> {
        if self.header.boot != Some(boot_identity()?) {
            return Ok(());
        }
        let mut observer = WorkObserver::new(scope)?;
        let mut samples = IdleSamples::default();
        loop {
            if deadline.has_passed() {
                return Err(io::Error::other(
                    "private application cleanup deadline elapsed",
                ));
            }
            let account = work::snapshot(self.header.worker)?;
            let mut current = work::all_processes()?;
            for root in &observer.known().roots {
                if let Some(origin) = work::remaining_root(*root, self.header.worker)?
                    && !current.contains(&origin)
                {
                    current.push(origin);
                }
            }
            // The account read is mandatory even when unrelated host metadata is unavailable.
            for origin in &account {
                if !current.contains(origin) {
                    current.push(*origin);
                }
            }
            let mut observed = observer.observe(&current)?;
            self.observe_work(observer.known(), false)?;
            observed
                .unresolved
                .retain(|origin| account.contains(origin));
            let unknown: Vec<_> = observed
                .owned
                .iter()
                .chain(&observed.unresolved)
                .map(|origin| origin.process)
                .collect();
            let idle = if observed.unresolved.is_empty() {
                Vec::new()
            } else {
                inspect(
                    &observed
                        .unresolved
                        .iter()
                        .map(|origin| origin.process)
                        .collect::<Vec<_>>(),
                    deadline,
                )?
            };
            if idle.iter().any(|process| {
                !observed
                    .unresolved
                    .iter()
                    .any(|origin| &origin.process == process)
            }) {
                return Err(io::Error::other(
                    "idle-service proof included an owned or unrequested process",
                ));
            }
            let mut relevant: Vec<_> = account.iter().map(|origin| origin.process).collect();
            for owned in &observed.owned {
                if !relevant.contains(&owned.process) {
                    relevant.push(owned.process);
                }
            }
            relevant.sort_by_key(ProcessIdentity::pid);
            if deadline.has_passed() {
                return Err(io::Error::other(
                    "private application cleanup deadline elapsed",
                ));
            }
            if samples.observe(&relevant, &unknown, &idle)? {
                return Ok(());
            }
            if deadline
                .remaining()
                .is_some_and(|left| left <= Duration::from_millis(100))
            {
                return Err(io::Error::other(format!(
                    "private application cleanup deferred: {} owned processes and {} unresolved process origins remain",
                    observed.owned.len(),
                    observed.unresolved.len()
                )));
            }
            std::thread::sleep(
                deadline
                    .remaining()
                    .unwrap_or(Duration::from_millis(100))
                    .min(Duration::from_millis(100)),
            );
        }
    }

    pub(crate) fn revoke_access(&mut self, deadline: Deadline) -> io::Result<()> {
        if self.header.version == 4 {
            for scope in self.outputs.values() {
                scope.validate_accounts(self.header.worker)?;
            }
            for change in self.pending.values() {
                change.validate(self.header.worker)?;
            }
        }
        let ids: Vec<_> = self.pending.keys().copied().rev().collect();
        for id in ids {
            if deadline.has_passed() {
                return Err(io::Error::other("permission recovery deadline elapsed"));
            }
            if self.pending[&id].revoke_access(self.header.worker)? {
                self.settled(id)?;
            }
        }
        Ok(())
    }

    pub(crate) fn reconcile_outputs(
        &mut self,
        deadline: Deadline,
        inspect: &mut IdleInspector<'_>,
        repair: &mut OutputInspector<'_>,
    ) -> io::Result<()> {
        if self.outputs.is_empty() {
            return Ok(());
        }
        self.revoke_access(deadline)?;
        self.wait_idle_with(deadline, inspect)?;
        self.checkpoint()?;
        let scopes: Vec<_> = self.outputs.values().cloned().collect();
        for scope in &scopes {
            scope.validate_owner(self.header.worker)?;
        }
        let result = (|| {
            let mut first = true;
            loop {
                let mut added = 0;
                for scope in &scopes {
                    added += scope.grant_inspection(self.header.worker, self, deadline)?;
                }
                match repair(&scopes, deadline) {
                    Ok(()) => return Ok(()),
                    Err(error)
                        if error.kind() == io::ErrorKind::WouldBlock && (first || added > 0) =>
                    {
                        // A partial creator scan can expose a hidden normal-owned directory for
                        // the next narrow grant. Repeated attempts must make grant progress.
                        first = false;
                    }
                    Err(error) => return Err(error),
                }
            }
        })();
        let revoked = self.revoke_access(deadline);
        match (result, revoked) {
            (Ok(()), Ok(())) => {}
            (Err(error), Ok(())) | (Ok(()), Err(error)) => return Err(error),
            (Err(error), Err(revoke)) => {
                return Err(io::Error::other(format!(
                    "{error}; inspection revocation: {revoke}"
                )));
            }
        }
        if self.pending.values().any(PermissionChange::has_allow) {
            return Err(io::Error::other(
                "temporary output inspection access remains pending",
            ));
        }
        self.wait_idle_with(deadline, inspect)?;
        for id in self.outputs.keys().copied().collect::<Vec<_>>() {
            self.append(&Event::OutputReconciled { id })?;
            self.outputs.remove(&id);
        }
        Ok(())
    }

    fn recover_with_outputs(
        &mut self,
        deadline: Deadline,
        inspect: &mut IdleInspector<'_>,
        outputs: &mut OutputInspector<'_>,
    ) -> io::Result<()> {
        if let Err(revoke) = self.revoke_access(deadline) {
            if self.outputs.is_empty() || !PermissionChange::path_inaccessible(&revoke) {
                return Err(revoke);
            }
            // A creator-owned moved parent can hide an original normal-owned node. Add owner
            // access using only grants that still exist; never regrant worker access here.
            let scopes: Vec<_> = self.outputs.values().cloned().collect();
            for scope in &scopes {
                scope.validate_owner(self.header.worker)?;
            }
            match outputs(&scopes, deadline) {
                Ok(()) => {}
                Err(error) if error.kind() == io::ErrorKind::WouldBlock => {}
                Err(error) => {
                    return Err(io::Error::other(format!(
                        "{revoke}; exposing inaccessible output parents: {error}"
                    )));
                }
            }
            self.revoke_access(deadline)?;
        }
        self.wait_idle_with(deadline, inspect)?;
        self.reconcile_outputs(deadline, inspect, outputs)?;
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

    #[cfg(test)]
    pub(crate) fn finish(&mut self, deadline: Deadline) -> io::Result<()> {
        self.finish_with(deadline, &mut |_, _| Ok(Vec::new()))
    }

    pub(crate) fn finish_with(
        &mut self,
        deadline: Deadline,
        inspect: &mut IdleInspector<'_>,
    ) -> io::Result<()> {
        self.finish_with_outputs(deadline, inspect, &mut |_, _| {
            Err(io::Error::other("output recovery executor is unavailable"))
        })
    }

    pub(crate) fn finish_with_outputs(
        &mut self,
        deadline: Deadline,
        inspect: &mut IdleInspector<'_>,
        outputs: &mut OutputInspector<'_>,
    ) -> io::Result<()> {
        self.recover_with_outputs(deadline, inspect, outputs)?;
        self.append(&Event::Closed)?;
        self.closed = true;
        Ok(())
    }
}

impl RecordChanges for PermissionJournal {
    fn is_pending(&self, id: u64) -> bool {
        self.pending.contains_key(&id)
    }
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

    fn process(pid: u32, birth: u64) -> ProcessIdentity {
        serde_json::from_value(serde_json::json!({"pid":pid,"seconds":birth,"microseconds":0}))
            .unwrap()
    }

    #[test]
    fn a_new_detached_child_blocks_cleanup_even_alongside_verified_idle_services() {
        let service = process(100, 1);
        let child = process(101, 2);
        let mut samples = IdleSamples::default();
        assert!(
            !samples
                .observe(&[service, child], &[service, child], &[service])
                .unwrap()
        );
        assert!(
            !samples
                .observe(&[service, child], &[service, child], &[service])
                .unwrap()
        );
        assert!(!samples.observe(&[service], &[service], &[service]).unwrap());
        assert!(samples.observe(&[service], &[service], &[service]).unwrap());
    }

    #[test]
    fn a_busy_or_reused_service_requires_two_new_clean_observations() {
        let service = process(100, 1);
        let reused = process(100, 2);
        let mut samples = IdleSamples::default();
        assert!(!samples.observe(&[service], &[service], &[service]).unwrap());
        assert!(!samples.observe(&[service], &[service], &[]).unwrap());
        assert!(!samples.observe(&[service], &[service], &[service]).unwrap());
        assert!(!samples.observe(&[reused], &[reused], &[reused]).unwrap());
        assert!(samples.observe(&[reused], &[reused], &[reused]).unwrap());
    }

    #[test]
    fn unrequested_duplicate_and_stale_service_observations_are_refused() {
        let service = process(100, 1);
        let reused = process(100, 2);
        for supplied in [vec![service, service], vec![reused]] {
            assert!(
                IdleSamples::default()
                    .observe(&[service], &[service], &supplied)
                    .is_err()
            );
        }
    }

    fn test_root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        std::fs::set_permissions(root.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn journal(root: &Path) -> PermissionJournal {
        PermissionJournal::open(root, u32::MAX, "test-account".into(), None, None).unwrap()
    }

    #[test]
    fn setup_recovery_allows_delayed_activation_but_retains_a_timed_out_journal() {
        use std::process::{Command, Stdio};

        for (activation_delay, succeeds) in [(20, true), (31, false)] {
            let root = test_root();
            let mut journal = journal(root.path());
            // Model the private inventory with a test-owned process, without another account.
            let worker = rustix::process::geteuid().as_raw();
            journal.header.worker = worker;
            journal.header.baseline = account_processes(worker).unwrap();
            let mut child = Command::new("/bin/cat")
                .env_clear()
                .stdin(Stdio::piped())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            let before = std::fs::read(root.path().join("pending.jsonl")).unwrap();
            let mut inspected = false;
            let mut saw_child = false;
            let mut outputs_called = false;
            let result = crate::macos_worker::backend_transport::finish_setup_journal(
                &mut journal,
                &mut |unknown, deadline| {
                    inspected = true;
                    saw_child |= unknown
                        .iter()
                        .any(|process| process.matches_pid(child.id()));
                    if deadline.remaining().unwrap_or(Duration::ZERO)
                        < Duration::from_secs(activation_delay)
                    {
                        return Err(io::Error::new(
                            io::ErrorKind::TimedOut,
                            "activation delayed",
                        ));
                    }
                    Ok(unknown.to_vec())
                },
                &mut |_, _| {
                    outputs_called = true;
                    Err(io::Error::other("no output rights were recorded"))
                },
            );
            drop(child.stdin.take());
            let status = child.wait().unwrap();
            assert!(status.success());
            assert!(inspected);
            assert!(saw_child);
            assert!(!outputs_called);
            assert_eq!(result.is_ok(), succeeds);
            assert_eq!(journal.closed, succeeds);
            let bytes = std::fs::read(root.path().join("pending.jsonl")).unwrap();
            let mut expected = before;
            if succeeds {
                expected.extend(serde_json::to_vec(&Event::Closed).unwrap());
                expected.push(b'\n');
            } else {
                assert_eq!(result.unwrap_err().kind(), io::ErrorKind::TimedOut);
            }
            assert_eq!(bytes, expected);
        }
    }

    fn output_fixture() -> (tempfile::TempDir, PathBuf, PathBuf, OutputScope) {
        let root = tempfile::tempdir_in("/private/tmp").unwrap();
        let project = root.path().join("project");
        let store = root.path().join("journal");
        std::fs::create_dir(&project).unwrap();
        let scope = OutputScope::capture(&project, u32::MAX, &[]).unwrap();
        (root, project, store, scope)
    }

    fn acl(path: &Path) -> Vec<String> {
        let output = std::process::Command::new("/bin/ls")
            .arg("-lde")
            .arg(path)
            .output()
            .unwrap();
        assert!(output.status.success());
        String::from_utf8(output.stdout)
            .unwrap()
            .lines()
            .skip(1)
            .map(str::to_owned)
            .collect()
    }

    #[test]
    fn output_authority_survives_early_settlement_and_inspection_is_read_only() {
        let (_root, project, store, scope) = output_fixture();
        let before = acl(&project);
        let mut journal = journal(&store);
        journal.output_pending(scope.clone()).unwrap();
        scope
            .grant_inspection(u32::MAX, &mut journal, Deadline::UNBOUNDED)
            .unwrap();
        assert!(!journal.pending.is_empty());
        journal.revoke_access(Deadline::UNBOUNDED).unwrap();
        assert!(journal.pending.is_empty() && journal.outputs.len() == 1);
        assert_eq!(acl(&project), before);
        let mut calls = 0;
        journal
            .reconcile_outputs(
                Deadline::from_millis(3000),
                &mut |_, _| Ok(Vec::new()),
                &mut |scopes, _| {
                    calls += 1;
                    assert_eq!(scopes, std::slice::from_ref(&scope));
                    let events = std::fs::read_to_string(store.join("pending.jsonl"))?;
                    for event in events
                        .lines()
                        .map(|line| serde_json::from_str::<serde_json::Value>(line).unwrap())
                    {
                        if event["kind"] == "Prepared" {
                            for entry in event["change"]["additions"].as_array().unwrap() {
                                let permissions = entry["permissions"].as_u64().unwrap();
                                // Darwin READ + EXECUTE; no write/delete/ACL-edit/inheritance rights.
                                let read_search =
                                    (1 << 1) | (1 << 3) | (1 << 7) | (1 << 9) | (1 << 11);
                                assert_eq!(permissions & !read_search, 0);
                                assert_eq!(entry["flags"], 0);
                            }
                        }
                    }
                    Ok(())
                },
            )
            .unwrap();
        assert_eq!(calls, 1);
        assert!(journal.outputs.is_empty() && journal.pending.is_empty());
        assert_eq!(acl(&project), before);
        journal.finish(Deadline::from_millis(3000)).unwrap();
        assert!(journal.closed);
    }

    fn pad_with_empty_events(journal: &mut PermissionJournal, length: u64) {
        journal.file.seek(SeekFrom::End(0)).unwrap();
        let buffer = [b'\n'; 8192];
        while journal.file.metadata().unwrap().len() < length {
            let count =
                (length - journal.file.metadata().unwrap().len()).min(buffer.len() as u64) as usize;
            journal.file.write_all(&buffer[..count]).unwrap();
        }
        journal.file.sync_all().unwrap();
    }

    #[test]
    fn a_full_work_history_checkpoints_exact_guards_and_still_finishes_inspection() {
        use glass_macos::workspace_access::{Access, Recorder, Sharing};
        use std::sync::{Arc, Mutex};
        let (_root, project, store, _) = output_fixture();
        let protected = project.join("evidence");
        std::fs::create_dir(&protected).unwrap();
        let journal = Arc::new(Mutex::new(journal(&store)));
        let recorder: Recorder = journal.clone();
        let mut sharing = Sharing::recorded(u32::MAX, recorder).unwrap();
        sharing
            .path_excluding(
                &project,
                Access::ReadWrite,
                std::slice::from_ref(&protected),
            )
            .unwrap();
        let scope = OutputScope::capture_shared(
            &project,
            u32::MAX,
            std::slice::from_ref(&protected),
            &sharing,
        )
        .unwrap();
        {
            let mut journal = journal.lock().unwrap();
            journal.output_pending(scope).unwrap();
            journal.revoke_access(Deadline::UNBOUNDED).unwrap();
            let header = journal.header.clone();
            let next = journal.next;
            let next_output = journal.next_output;
            let guards: Vec<_> = journal.pending.keys().copied().collect();
            assert!(!guards.is_empty() && next > guards.len() as u64 + 1);
            pad_with_empty_events(&mut journal, LIMIT - 100);
            journal
                .reconcile_outputs(
                    Deadline::from_millis(5000),
                    &mut |_, _| Ok(Vec::new()),
                    &mut |_, _| Ok(()),
                )
                .unwrap();
            assert_eq!(journal.header, header);
            assert_eq!(journal.next_output, next_output);
            assert!(journal.next > next);
            assert_eq!(journal.pending.keys().copied().collect::<Vec<_>>(), guards);
            assert!(journal.file.metadata().unwrap().len() < LIMIT / 2);
        }
        sharing.restore().unwrap();
        journal
            .lock()
            .unwrap()
            .finish(Deadline::from_millis(3000))
            .unwrap();
        drop(sharing);
        drop(journal);
        let mut reopened =
            PermissionJournal::open(&store, u32::MAX, "test-account".into(), None, None).unwrap();
        reopened.finish(Deadline::from_millis(3000)).unwrap();
    }

    #[test]
    fn output_settlement_consumes_its_reserve_at_the_limit() {
        let (_root, _project, store, scope) = output_fixture();
        let mut journal = journal(&store);
        journal.output_pending(scope).unwrap();
        pad_with_empty_events(&mut journal, LIMIT - 100);
        journal.append(&Event::OutputReconciled { id: 1 }).unwrap();
        journal.outputs.remove(&1);
        journal.append(&Event::Closed).unwrap();
        assert!(journal.file.metadata().unwrap().len() <= LIMIT);
    }

    #[test]
    fn post_rename_sync_failure_keeps_the_named_inode_locked_and_poisoned() {
        let (_root, _project, store, scope) = output_fixture();
        let mut journal = journal(&store);
        journal.output_pending(scope).unwrap();
        let bytes = std::fs::read(store.join("pending.jsonl")).unwrap();
        assert!(
            journal
                .replace_file(&bytes, |_| Err(io::Error::other(
                    "injected directory sync failure"
                )))
                .is_err()
        );
        assert!(journal.poisoned);
        let named = File::open(store.join("pending.jsonl")).unwrap();
        assert_eq!(
            named.metadata().unwrap().ino(),
            journal.file.metadata().unwrap().ino()
        );
        assert!(fs4::FileExt::try_lock(&named).is_err());
        assert!(journal.append(&Event::WorkStarted).is_err());
        drop(journal);
        fs4::FileExt::try_lock(&named).unwrap();
        fs4::FileExt::unlock(&named).unwrap();
        let mut recovered = PermissionJournal::open_with_outputs(
            &store,
            u32::MAX,
            "test-account".into(),
            None,
            None,
            &mut |_, _| Ok(Vec::new()),
            &mut |_, _| Ok(()),
        )
        .unwrap();
        recovered.finish(Deadline::from_millis(3000)).unwrap();
    }

    #[test]
    fn checkpoint_replay_rejects_bad_position_counters_and_protected_boundaries() {
        for mode in 0..5 {
            let (_root, project, store, scope) = output_fixture();
            let before = acl(&project);
            let mut journal = journal(&store);
            let mut value = Checkpoint {
                next: 1,
                next_output: 2,
                pending: Vec::new(),
                outputs: vec![(1, scope.clone())],
            };
            match mode {
                0 => {
                    journal
                        .append(&Event::OutputPending { id: 1, scope })
                        .unwrap();
                }
                1 => value.outputs.push((1, scope.clone())),
                2 => value.next_output = 1,
                3 => {
                    let mut altered = serde_json::to_value(scope).unwrap();
                    altered["protected_paths"] =
                        serde_json::json!(["/private/changed-protected-boundary"]);
                    value
                        .outputs
                        .push((2, serde_json::from_value(altered).unwrap()));
                    value.next_output = 3;
                }
                4 => {
                    journal.header.version = 3;
                    journal.reset().unwrap();
                }
                _ => unreachable!(),
            }
            journal.append(&Event::Checkpoint { value }).unwrap();
            drop(journal);
            let mut repairs = 0;
            assert!(
                PermissionJournal::open_with_outputs(
                    &store,
                    u32::MAX,
                    "test-account".into(),
                    None,
                    None,
                    &mut |_, _| Ok(Vec::new()),
                    &mut |_, _| {
                        repairs += 1;
                        Ok(())
                    }
                )
                .is_err()
            );
            assert_eq!(repairs, 0);
            assert_eq!(acl(&project), before);
        }
    }

    #[test]
    fn interrupted_inspection_is_revoked_before_recovery_and_loss_keeps_output_pending() {
        let (_root, project, store, scope) = output_fixture();
        let before = acl(&project);
        let mut first = journal(&store);
        first.output_pending(scope.clone()).unwrap();
        scope
            .grant_inspection(u32::MAX, &mut first, Deadline::UNBOUNDED)
            .unwrap();
        assert_ne!(acl(&project), before);
        drop(first);
        let result = PermissionJournal::open_with_outputs(
            &store,
            u32::MAX,
            "test-account".into(),
            None,
            None,
            &mut |_, _| {
                assert_eq!(acl(&project), before);
                Ok(Vec::new())
            },
            &mut |_, _| Err(io::Error::other("injected broker loss")),
        );
        assert!(result.is_err());
        assert_eq!(acl(&project), before);
        let events: Vec<Event> = std::fs::read_to_string(store.join("pending.jsonl"))
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        assert!(
            events
                .iter()
                .any(|event| matches!(event, Event::OutputPending { .. }))
        );
        assert!(
            !events
                .iter()
                .any(|event| matches!(event, Event::OutputReconciled { .. } | Event::Closed))
        );
        let mut repairs = 0;
        let mut recovered = PermissionJournal::open_with_outputs(
            &store,
            u32::MAX,
            "test-account".into(),
            None,
            None,
            &mut |_, _| Ok(Vec::new()),
            &mut |_, _| {
                repairs += 1;
                Ok(())
            },
        )
        .unwrap();
        assert_eq!(repairs, 1);
        assert!(recovered.outputs.is_empty() && recovered.pending.is_empty());
        assert_eq!(acl(&project), before);
        recovered.finish(Deadline::from_millis(3000)).unwrap();
    }

    #[test]
    fn failed_repair_retries_only_with_bounded_progress_and_never_closes() {
        let (_root, project, store, scope) = output_fixture();
        let before = acl(&project);
        let mut journal = journal(&store);
        journal.output_pending(scope).unwrap();
        let mut calls = 0;
        assert!(
            journal
                .reconcile_outputs(
                    Deadline::from_millis(3000),
                    &mut |_, _| Ok(Vec::new()),
                    &mut |_, _| {
                        calls += 1;
                        Err(io::Error::new(io::ErrorKind::WouldBlock, "still blocked"))
                    }
                )
                .is_err()
        );
        assert_eq!(calls, 2);
        assert!(journal.outputs.len() == 1 && journal.pending.is_empty() && !journal.closed);
        assert_eq!(acl(&project), before);
        assert!(journal.finish(Deadline::from_millis(3000)).is_err());
    }

    #[test]
    fn known_live_work_prevents_inspection_grants_and_output_completion() {
        let (_root, project, store, scope) = output_fixture();
        let before = acl(&project);
        let mut journal = journal(&store);
        journal.output_pending(scope).unwrap();
        journal
            .header
            .work
            .as_mut()
            .unwrap()
            .work
            .roots
            .push(work::observe(std::process::id(), rustix::process::geteuid().as_raw()).unwrap());
        let mut repairs = 0;
        assert!(
            journal
                .reconcile_outputs(
                    Deadline::from_millis(200),
                    &mut |_, _| Ok(Vec::new()),
                    &mut |_, _| {
                        repairs += 1;
                        Ok(())
                    }
                )
                .is_err()
        );
        assert_eq!(repairs, 0);
        assert_eq!(acl(&project), before);
        assert!(journal.outputs.len() == 1 && journal.pending.is_empty());
    }

    #[test]
    fn invalid_permissions_do_not_trigger_additive_repair_or_revoke_a_valid_prefix() {
        let (_root, project, store, scope) = output_fixture();
        let mut journal = journal(&store);
        journal.output_pending(scope.clone()).unwrap();
        scope
            .grant_inspection(u32::MAX, &mut journal, Deadline::UNBOUNDED)
            .unwrap();
        let granted = acl(&project);
        let id = *journal.pending.keys().next().unwrap();
        let mut change = serde_json::to_value(&journal.pending[&id]).unwrap();
        let permissions = change["additions"][0]["permissions"].as_u64().unwrap();
        change["additions"][0]["permissions"] = (permissions | (1 << 12)).into();
        journal
            .pending
            .insert(id, serde_json::from_value(change).unwrap());
        let mut repairs = 0;
        let result = journal.recover_with_outputs(
            Deadline::from_millis(3000),
            &mut |_, _| Ok(Vec::new()),
            &mut |_, _| {
                repairs += 1;
                Ok(())
            },
        );
        assert!(result.is_err());
        assert_eq!(repairs, 0);
        assert_eq!(acl(&project), granted);
        assert!(journal.outputs.len() == 1 && !journal.closed);
    }

    #[test]
    fn old_headers_and_unrevoked_inspection_cannot_accept_output_completion() {
        for legacy in [true, false] {
            let (_root, _project, store, scope) = output_fixture();
            let mut journal = journal(&store);
            if legacy {
                journal.header.version = 3;
                journal.reset().unwrap();
            }
            journal
                .append(&Event::OutputPending {
                    id: 1,
                    scope: scope.clone(),
                })
                .unwrap();
            if !legacy {
                scope
                    .grant_inspection(u32::MAX, &mut journal, Deadline::UNBOUNDED)
                    .unwrap();
                journal.append(&Event::OutputReconciled { id: 1 }).unwrap();
                journal.revoke_access(Deadline::UNBOUNDED).unwrap();
            }
            drop(journal);
            assert!(
                PermissionJournal::open(&store, u32::MAX, "test-account".into(), None, None)
                    .is_err()
            );
        }
    }

    #[test]
    fn revoking_access_settles_allows_but_preserves_guards_and_the_open_lease() {
        use glass_macos::workspace_access::{Access, Recorder, Sharing};
        use std::sync::{Arc, Mutex};

        let root = test_root();
        let project = root.path().join("project");
        let evidence = project.join("evidence");
        std::fs::create_dir_all(&evidence).unwrap();
        let source = project.join("source");
        std::fs::write(&source, "source").unwrap();
        let acl = |path: &Path| {
            std::process::Command::new("/bin/ls")
                .args(["-lde"])
                .arg(path)
                .output()
                .unwrap()
                .stdout
        };
        let source_before = acl(&source);
        let store = root.path().join("journal");
        let worker = u32::MAX - 1;
        let journal = Arc::new(Mutex::new(
            PermissionJournal::open(&store, worker, "test-account".into(), None, None).unwrap(),
        ));
        let recorder: Recorder = journal.clone();
        let mut sharing = Sharing::recorded(worker, recorder).unwrap();
        sharing
            .path_excluding(&project, Access::ReadWrite, std::slice::from_ref(&evidence))
            .unwrap();
        assert_ne!(acl(&source), source_before);
        let owner = std::process::Command::new("/usr/bin/id")
            .arg("-un")
            .output()
            .unwrap()
            .stdout;
        let owner = String::from_utf8(owner).unwrap();
        let owner_rule = format!("user:{} allow ", owner.trim());
        let granted = String::from_utf8(acl(&project)).unwrap();
        let same_allow = granted
            .lines()
            .find(|line| line.contains(&owner_rule))
            .unwrap()
            .trim()
            .split_once(": ")
            .unwrap()
            .1
            .to_owned();

        {
            let mut journal = journal.lock().unwrap();
            let header = journal.header.clone();
            journal.revoke_access(Deadline::UNBOUNDED).unwrap();
            assert_eq!(acl(&source), source_before);
            assert!(!journal.closed && !journal.pending.is_empty());
            assert!(journal.pending.values().all(|change| {
                serde_json::to_value(change).unwrap()["additions"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|entry| entry["tag"] == "Deny")
            }));
            assert_eq!(journal.header, header);
            let history = std::fs::read_to_string(store.join("pending.jsonl")).unwrap();
            assert!(history.contains("Settled") && !history.contains("Closed"));
            journal.revoke_access(Deadline::UNBOUNDED).unwrap();
            assert_eq!(
                std::fs::read_to_string(store.join("pending.jsonl")).unwrap(),
                history
            );
        }
        assert!(
            std::process::Command::new("/bin/chmod")
                .args(["+a", &same_allow])
                .arg(&project)
                .status()
                .unwrap()
                .success()
        );
        let allows = |path: &Path| {
            String::from_utf8(acl(path))
                .unwrap()
                .lines()
                .filter(|line| line.contains(" allow "))
                .map(|line| line.trim().split_once(": ").unwrap().1.to_owned())
                .collect::<Vec<_>>()
        };
        let user_acl = allows(&project);
        drop(sharing);
        let path = store.join("pending.jsonl");
        let prefix = std::fs::read(&path).unwrap();
        let mut retained = File::open(&path).unwrap();
        drop(journal);
        let mut recovered =
            PermissionJournal::open(&store, worker, "test-account".into(), None, None).unwrap();
        assert_eq!(allows(&project), user_acl);
        let mut history = Vec::new();
        retained.read_to_end(&mut history).unwrap();
        assert!(history.starts_with(&prefix));
        let history = String::from_utf8(history).unwrap();
        assert_eq!(
            history.matches("Prepared").count(),
            history.matches("Settled").count()
        );
        recovered.finish(Deadline::from_millis(2000)).unwrap();
    }

    #[test]
    fn journal_release_does_not_wait_for_an_inherited_descriptor_to_close() {
        let root = test_root();
        let mut first = journal(root.path());
        first.finish(Deadline::UNBOUNDED).unwrap();
        let mut inherited = first.file.try_clone().unwrap();
        let before = std::fs::read(root.path().join("pending.jsonl")).unwrap();
        assert!(
            PermissionJournal::open(root.path(), u32::MAX, "test-account".into(), None, None)
                .is_err()
        );
        drop(first);
        let next = journal(root.path());
        inherited.seek(SeekFrom::Start(0)).unwrap();
        let mut retained = Vec::new();
        inherited.read_to_end(&mut retained).unwrap();
        assert_eq!(retained, before);
        drop(next);
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
        value.work = None;
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
        assert_eq!(next.header.version, 4);
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
            work: None,
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

    fn work_root() -> work::ProcessOrigin {
        serde_json::from_value(serde_json::json!({"process":{"pid":123,"seconds":1,"microseconds":0},"unique":123,"parent_unique":1})).unwrap()
    }

    #[test]
    fn work_roots_and_pending_launches_are_durable_before_settlement() {
        let root = test_root();
        let mut journal = journal(root.path());
        assert_eq!(journal.header.version, 4);
        journal
            .observe_work(
                WorkSnapshot {
                    roots: vec![work_root()],
                    uncertain: false,
                },
                false,
            )
            .unwrap();
        journal.work_started().unwrap();
        let expected = journal.header.clone();
        journal.read().unwrap();
        assert_eq!(journal.header, expected);
        assert!(journal.header.work.as_ref().unwrap().launch_pending);
        journal
            .observe_work(
                WorkSnapshot {
                    roots: vec![work_root()],
                    uncertain: false,
                },
                true,
            )
            .unwrap();
        let expected = journal.header.clone();
        journal.read().unwrap();
        assert_eq!(journal.header, expected);
        assert!(!journal.header.work.as_ref().unwrap().launch_pending);
    }

    #[test]
    fn learned_descendants_survive_journal_replay_without_their_parent() {
        let root = test_root();
        let mut journal = journal(root.path());
        journal
            .observe_work(
                WorkSnapshot {
                    roots: vec![work_root()],
                    uncertain: false,
                },
                false,
            )
            .unwrap();
        let child: work::ProcessOrigin = serde_json::from_value(serde_json::json!({
            "process":{"pid":124,"seconds":2,"microseconds":0},
            "unique":124,"parent_unique":123
        }))
        .unwrap();
        let mut observer = WorkObserver::new(journal.header.work.as_ref().unwrap()).unwrap();
        observer.observe(&[child]).unwrap();
        journal.observe_work(observer.known(), false).unwrap();
        journal.read().unwrap();
        let mut reopened = WorkObserver::new(journal.header.work.as_ref().unwrap()).unwrap();
        assert_eq!(reopened.observe(&[child]).unwrap().owned, vec![child]);
    }

    #[test]
    fn uncertainty_or_pending_launch_cannot_discard_a_known_cross_uid_root() {
        for pending in [false, true] {
            let root = test_root();
            let mut journal = PermissionJournal::open(
                root.path(),
                u32::MAX - 1,
                "test-account".into(),
                None,
                None,
            )
            .unwrap();
            let actual =
                work::observe(std::process::id(), rustix::process::geteuid().as_raw()).unwrap();
            journal
                .observe_work(
                    WorkSnapshot {
                        roots: vec![actual],
                        uncertain: !pending,
                    },
                    false,
                )
                .unwrap();
            if pending {
                journal.work_started().unwrap();
            }
            let failure = journal
                .wait_idle_with(Deadline::from_millis(1000), &mut |_, _| {
                    panic!("a known owned root cannot be offered for idle-service proof")
                })
                .unwrap_err();
            assert_eq!(failure.raw_os_error(), Some(1));
            assert!(!journal.closed);
            journal.read().unwrap();
            assert!(
                journal
                    .header
                    .work
                    .as_ref()
                    .unwrap()
                    .work
                    .roots
                    .contains(&actual)
            );
        }
    }

    #[test]
    fn a_retired_session_cannot_bypass_unresolved_private_work_on_the_same_boot() {
        use std::process::{Command, Stdio};

        let root = test_root();
        let mut journal = journal(root.path());
        let worker = rustix::process::geteuid().as_raw();
        let retired = i32::MAX as u32 - 2;
        assert!(!session_exists(retired, worker).unwrap());
        let mut gone = work_root();
        gone.unique = u64::MAX - 1;
        gone.process = serde_json::from_value(serde_json::json!({
            "pid":i32::MAX - 1,"seconds":1,"microseconds":0
        }))
        .unwrap();
        journal
            .observe_work(
                WorkSnapshot {
                    roots: vec![gone],
                    uncertain: true,
                },
                false,
            )
            .unwrap();
        // Model private-account inventory with test-owned processes; no account is created.
        journal.header.worker = worker;
        journal.header.session = Some(retired);
        journal.header.work.as_mut().unwrap().baseline = work::snapshot(worker).unwrap();
        let mut child = Command::new("/bin/cat")
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let child_origin = work::observe(child.id(), worker).unwrap();
        let mut inspections = 0;
        let result = journal.wait_idle_with(Deadline::from_millis(500), &mut |unknown, _| {
            inspections += 1;
            assert!(unknown.contains(&child_origin.process));
            Ok(Vec::new())
        });
        child.stdin.take();
        child.wait().unwrap();
        assert!(result.is_err());
        assert!(inspections > 0);
        assert!(!journal.closed);
    }

    #[test]
    fn an_unprepared_launch_result_cannot_change_owned_work_history() {
        let root = test_root();
        let mut journal = journal(root.path());
        let before = std::fs::read(root.path().join("pending.jsonl")).unwrap();
        assert!(
            journal
                .observe_work(
                    WorkSnapshot {
                        roots: vec![work_root()],
                        uncertain: false
                    },
                    true
                )
                .is_err()
        );
        assert_eq!(
            std::fs::read(root.path().join("pending.jsonl")).unwrap(),
            before
        );
    }

    #[test]
    fn an_invalid_work_snapshot_keeps_the_launch_pending_and_its_history_intact() {
        let root = test_root();
        let mut journal = journal(root.path());
        journal.work_started().unwrap();
        let before = std::fs::read(root.path().join("pending.jsonl")).unwrap();
        assert!(journal.observe_work(WorkSnapshot::default(), true).is_err());
        assert!(journal.header.work.as_ref().unwrap().launch_pending);
        assert_eq!(
            std::fs::read(root.path().join("pending.jsonl")).unwrap(),
            before
        );
    }
}
