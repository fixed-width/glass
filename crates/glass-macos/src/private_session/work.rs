//! Process lifetimes and ancestry for private application work.

use std::collections::{BTreeMap, BTreeSet};
use std::io;
use std::sync::{Mutex, OnceLock};

use serde::{Deserialize, Serialize};

use super::ProcessIdentity;

const ROOT_LIMIT: usize = 1024;

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct ProcessOrigin {
    pub process: ProcessIdentity,
    pub unique: u64,
    pub parent_unique: u64,
}

impl ProcessOrigin {
    pub fn validate(&self) -> io::Result<()> {
        if self.process.pid() == 0
            || self.process.pid() > i32::MAX as u32
            || self.process.seconds == 0
            || self.process.microseconds >= 1_000_000
            || self.unique == 0
            || self.unique == self.parent_unique
        {
            return Err(io::Error::other("invalid process lifetime identity"));
        }
        Ok(())
    }
}

/// Read one lifetime, including an unreaped child that has already exited.
pub fn observe(pid: u32, uid: u32) -> io::Result<ProcessOrigin> {
    if uid == u32::MAX {
        return Err(io::Error::other("invalid ownership account"));
    }
    observe_for(pid, uid)
}

fn observe_for(pid: u32, uid: u32) -> io::Result<ProcessOrigin> {
    #[repr(C)]
    #[derive(Default)]
    struct Native {
        process: ProcessIdentity,
        unique: u64,
        parent_unique: u64,
    }
    unsafe extern "C" {
        fn glass_private_process_origin(pid: u32, uid: u32, out: *mut Native) -> i32;
    }
    let mut native = Native::default();
    // SAFETY: the bridge initializes one repr(C) record without retaining its pointer.
    let code = unsafe { glass_private_process_origin(pid, uid, &mut native) };
    if code != 0 {
        return Err(io::Error::from_raw_os_error(code));
    }
    let value = ProcessOrigin {
        process: native.process,
        unique: native.unique,
        parent_unique: native.parent_unique,
    };
    value.validate()?;
    Ok(value)
}

/// Recheck a known root independently of the account list, including a changed effective UID.
pub fn remaining_root(root: ProcessOrigin, uid: u32) -> io::Result<Option<ProcessOrigin>> {
    match observe_for(root.process.pid(), u32::MAX) {
        Ok(current) if current != root => Ok(None),
        Ok(_) => match observe(root.process.pid(), uid) {
            Ok(current) => Ok((current == root).then_some(current)),
            Err(error) if matches!(error.raw_os_error(), Some(2 | 3)) => Ok(None),
            Err(error) => Err(error),
        },
        Err(error) if matches!(error.raw_os_error(), Some(2 | 3)) => Ok(None),
        Err(error) => Err(error),
    }
}

pub fn snapshot(uid: u32) -> io::Result<Vec<ProcessOrigin>> {
    snapshot_for(uid, super::account_processes(uid)?)
}

/// Observe ancestry across account boundaries; this does not grant ownership of other work.
pub fn all_processes() -> io::Result<Vec<ProcessOrigin>> {
    unsafe extern "C" {
        fn glass_private_all_processes(out: *mut ProcessIdentity, capacity: i32) -> i32;
    }
    let mut processes = vec![ProcessIdentity::default(); 8192];
    // SAFETY: the bridge writes at most the supplied number of repr(C) process identities.
    let count = unsafe { glass_private_all_processes(processes.as_mut_ptr(), 8192) };
    if count < 0 {
        return Err(io::Error::from_raw_os_error(-count));
    }
    processes.truncate(count as usize);
    snapshot_for(u32::MAX, processes)
}

fn snapshot_for(uid: u32, processes: Vec<ProcessIdentity>) -> io::Result<Vec<ProcessOrigin>> {
    let mut result = Vec::new();
    for process in processes {
        match observe_for(process.pid(), uid) {
            Ok(origin) if origin.process == process => result.push(origin),
            Ok(_) => {
                return Err(io::Error::other(
                    "process changed during ownership observation",
                ));
            }
            Err(error) if matches!(error.raw_os_error(), Some(2 | 3)) => {}
            Err(error) if uid == u32::MAX && matches!(error.raw_os_error(), Some(1 | 13)) => {}
            Err(error) => return Err(error),
        }
    }
    Ok(result)
}

#[derive(Clone, Debug, Default, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkSnapshot {
    pub roots: Vec<ProcessOrigin>,
    pub uncertain: bool,
}

static TRACKER: OnceLock<Mutex<WorkSnapshot>> = OnceLock::new();

pub fn begin() -> io::Result<()> {
    super::require_private().map_err(io::Error::other)?;
    let root = observe(std::process::id(), rustix::process::geteuid().as_raw())?;
    TRACKER
        .set(Mutex::new(WorkSnapshot {
            roots: vec![root],
            uncertain: false,
        }))
        .map_err(|_| io::Error::other("private work tracking was already initialized"))
}

pub(crate) fn record(pid: u32) -> io::Result<Option<ProcessOrigin>> {
    let Some(tracker) = TRACKER.get() else {
        return Ok(None);
    };
    let mut tracker = tracker
        .lock()
        .map_err(|_| io::Error::other("private work tracker is poisoned"))?;
    let result = (|| {
        let origin = observe(pid, rustix::process::geteuid().as_raw())?;
        if !tracker.roots.contains(&origin) {
            if tracker.roots.len() >= ROOT_LIMIT {
                return Err(io::Error::other("too many private work roots"));
            }
            tracker.roots.push(origin);
        }
        Ok(Some(origin))
    })();
    if result.is_err() {
        tracker.uncertain = true;
    }
    result
}

pub(crate) fn mark_uncertain() {
    if let Some(tracker) = TRACKER.get() {
        let mut tracker = tracker
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        tracker.uncertain = true;
    }
}

pub(crate) fn matches_adopted(origin: Option<ProcessOrigin>) -> io::Result<bool> {
    let Some(root) = origin else {
        return Ok(true);
    };
    match remaining_root(root, rustix::process::geteuid().as_raw()) {
        Ok(current) => Ok(current.is_some()),
        Err(error) => {
            mark_uncertain();
            Err(error)
        }
    }
}

pub fn recorded() -> io::Result<WorkSnapshot> {
    TRACKER
        .get()
        .ok_or_else(|| io::Error::other("private work tracking is not initialized"))?
        .lock()
        .map(|tracker| tracker.clone())
        .map_err(|_| io::Error::other("private work tracker is poisoned"))
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct WorkScope {
    pub baseline: Vec<ProcessOrigin>,
    pub work: WorkSnapshot,
    pub launch_pending: bool,
}

impl WorkScope {
    pub fn validate(&self) -> io::Result<()> {
        if self.baseline.len() > 8192 || self.work.roots.len() > ROOT_LIMIT {
            return Err(io::Error::other("invalid private work scope"));
        }
        let mut seen = BTreeSet::new();
        for origin in &self.baseline {
            origin.validate()?;
            if !seen.insert(origin.unique) {
                return Err(io::Error::other("duplicate process baseline identity"));
            }
        }
        seen.clear();
        for origin in &self.work.roots {
            origin.validate()?;
            if !seen.insert(origin.unique) {
                return Err(io::Error::other("duplicate private work root"));
            }
        }
        Ok(())
    }

    pub fn merge(&mut self, observed: WorkSnapshot, start_finished: bool) -> io::Result<bool> {
        let before = self.clone();
        if observed.roots.is_empty() || observed.roots.len() > ROOT_LIMIT {
            return Err(io::Error::other("invalid private work-root observation"));
        }
        for origin in observed.roots {
            origin.validate()?;
            if let Some(prior) = self
                .work
                .roots
                .iter()
                .find(|prior| prior.unique == origin.unique)
            {
                if prior != &origin {
                    return Err(io::Error::other("private work root identity changed"));
                }
            } else {
                self.work.roots.push(origin);
            }
        }
        self.work.uncertain |= observed.uncertain;
        if start_finished {
            self.launch_pending = false;
        }
        self.validate()?;
        Ok(self != &before)
    }
}

pub struct WorkObserver {
    scope: WorkScope,
    owned: BTreeMap<u64, ProcessOrigin>,
}

pub struct WorkObservation {
    pub owned: Vec<ProcessOrigin>,
    pub unresolved: Vec<ProcessOrigin>,
}

impl WorkObserver {
    pub fn new(scope: &WorkScope) -> io::Result<Self> {
        scope.validate()?;
        let owned = scope
            .work
            .roots
            .iter()
            .map(|origin| (origin.unique, *origin))
            .collect();
        Ok(Self {
            scope: scope.clone(),
            owned,
        })
    }

    pub fn known(&self) -> WorkSnapshot {
        WorkSnapshot {
            roots: self.owned.values().copied().collect(),
            uncertain: self.scope.work.uncertain,
        }
    }

    pub fn observe(&mut self, current: &[ProcessOrigin]) -> io::Result<WorkObservation> {
        let scope = &self.scope;
        for origin in current {
            origin.validate()?;
            if let Some(prior) = self
                .owned
                .values()
                .chain(&scope.baseline)
                .find(|prior| prior.unique == origin.unique)
                && prior != origin
            {
                return Err(io::Error::other("recorded process lifetime changed"));
            }
        }
        loop {
            let mut changed = false;
            for origin in current {
                if self.owned.contains_key(&origin.unique) {
                    continue;
                }
                if self.owned.contains_key(&origin.parent_unique) {
                    if self.owned.len() >= ROOT_LIMIT {
                        return Err(io::Error::other("too many private work lifetimes"));
                    }
                    changed |= self.owned.insert(origin.unique, *origin).is_none();
                }
            }
            if !changed {
                break;
            }
        }
        Ok(WorkObservation {
            owned: current
                .iter()
                .filter(|origin| self.owned.contains_key(&origin.unique))
                .copied()
                .collect(),
            unresolved: current
                .iter()
                .filter(|origin| {
                    !self.owned.contains_key(&origin.unique) && !scope.baseline.contains(origin)
                })
                .copied()
                .collect(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::{BufRead, BufReader, Write};
    use std::process::{Child, Command, Stdio};
    use std::time::{Duration, Instant};

    fn origin(pid: u32, unique: u64, parent: u64) -> ProcessOrigin {
        ProcessOrigin {
            process: ProcessIdentity {
                pid,
                seconds: unique,
                microseconds: 0,
            },
            unique,
            parent_unique: parent,
        }
    }

    fn scope(roots: Vec<ProcessOrigin>) -> WorkScope {
        WorkScope {
            baseline: vec![origin(20, 20, 1)],
            work: WorkSnapshot {
                roots,
                uncertain: false,
            },
            launch_pending: false,
        }
    }

    #[test]
    fn launchd_parentage_does_not_prove_independence() {
        let mut observer = WorkObserver::new(&scope(vec![origin(10, 10, 1)])).unwrap();
        let observed = observer
            .observe(&[origin(30, 30, 1), origin(31, 31, 30)])
            .unwrap();
        assert!(observed.owned.is_empty());
        assert_eq!(
            observed.unresolved,
            vec![origin(30, 30, 1), origin(31, 31, 30)]
        );
    }

    #[test]
    fn a_new_baseline_descendant_is_not_proven_independent() {
        let child = origin(30, 30, 20);
        let mut observer = WorkObserver::new(&scope(vec![origin(10, 10, 1)])).unwrap();
        let observed = observer.observe(&[origin(20, 20, 1), child]).unwrap();
        assert!(observed.owned.is_empty());
        assert_eq!(observed.unresolved, vec![child]);
    }

    #[test]
    fn explicit_launchservices_roots_override_their_launchd_parent() {
        let root = origin(30, 30, 1);
        let mut observer = WorkObserver::new(&scope(vec![root])).unwrap();
        assert_eq!(observer.observe(&[root]).unwrap().owned, vec![root]);
    }

    #[test]
    fn remembered_children_stay_owned_after_their_parent_exits() {
        let child = origin(31, 31, 10);
        let leaf = origin(32, 32, 31);
        let mut observer = WorkObserver::new(&scope(vec![origin(10, 10, 1)])).unwrap();
        assert_eq!(observer.observe(&[child, leaf]).unwrap().owned.len(), 2);
        assert_eq!(observer.observe(&[leaf]).unwrap().owned, vec![leaf]);
    }

    #[test]
    fn learned_lifetimes_remain_owned_when_an_observer_is_reopened() {
        let child = origin(31, 31, 10);
        let mut scope = scope(vec![origin(10, 10, 1)]);
        let mut observer = WorkObserver::new(&scope).unwrap();
        observer.observe(&[child]).unwrap();
        scope.merge(observer.known(), false).unwrap();
        let mut reopened = WorkObserver::new(&scope).unwrap();
        assert_eq!(reopened.observe(&[child]).unwrap().owned, vec![child]);
    }

    #[test]
    fn a_missing_intermediate_parent_is_unresolved() {
        let orphan = origin(32, 32, 31);
        let mut observer = WorkObserver::new(&scope(vec![origin(10, 10, 1)])).unwrap();
        assert_eq!(
            observer.observe(&[orphan]).unwrap().unresolved,
            vec![orphan]
        );
    }

    #[test]
    fn a_reused_numeric_pid_is_not_the_old_owned_process() {
        let mut observer = WorkObserver::new(&scope(vec![origin(10, 10, 1)])).unwrap();
        let observed = observer.observe(&[origin(10, 100, 1)]).unwrap();
        assert!(observed.owned.is_empty());
        assert_eq!(observed.unresolved, vec![origin(10, 100, 1)]);
    }

    #[test]
    fn a_conflicting_lifetime_cannot_reuse_a_recorded_unique_id() {
        let root = origin(10, 10, 1);
        let mut observer = WorkObserver::new(&scope(vec![root])).unwrap();
        assert!(
            observer
                .observe(&[ProcessOrigin {
                    process: origin(11, 11, 1).process,
                    ..root
                }])
                .is_err()
        );
    }

    struct Fixture {
        child: Child,
        root: Option<ProcessOrigin>,
        _directory: tempfile::TempDir,
    }
    impl Drop for Fixture {
        fn drop(&mut self) {
            self.child.stdin.take();
            let _ = self.child.wait();
        }
    }

    fn fixture(mode: &str) -> Fixture {
        let directory = tempfile::tempdir().unwrap();
        let binary = directory.path().join("work-process");
        let built = Command::new("/usr/bin/clang")
            .args(["-Wall", "-Wextra", "-Werror"])
            .arg(std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("fixture/work_process.c"))
            .arg("-o")
            .arg(&binary)
            .output()
            .unwrap();
        assert!(
            built.status.success(),
            "{}",
            String::from_utf8_lossy(&built.stderr)
        );
        let child = Command::new(binary)
            .arg(mode)
            .env_clear()
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
            .unwrap();
        let mut fixture = Fixture {
            child,
            root: None,
            _directory: directory,
        };
        fixture.root =
            Some(observe(fixture.child.id(), rustix::process::geteuid().as_raw()).unwrap());
        fixture
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"g")
            .unwrap();
        fixture
    }

    fn wait_gone(process: ProcessOrigin, uid: u32) {
        let deadline = Instant::now() + Duration::from_secs(2);
        while remaining_root(process, uid).unwrap().is_some() {
            assert!(Instant::now() < deadline, "fixture did not exit");
            std::thread::sleep(Duration::from_millis(10));
        }
    }

    #[test]
    fn full_host_inventory_can_find_a_known_owned_child() {
        let mut fixture = fixture("exec");
        let uid = rustix::process::geteuid().as_raw();
        let mut line = String::new();
        BufReader::new(fixture.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        let child = observe(fixture.child.id(), uid).unwrap();
        assert!(all_processes().unwrap().contains(&child));
        fixture.child.stdin.take();
        fixture.child.wait().unwrap();
    }

    #[test]
    fn kernel_parent_identity_survives_setsid_and_reparenting() {
        let mut fixture = fixture("single");
        let uid = rustix::process::geteuid().as_raw();
        let root = fixture.root.unwrap();
        let mut line = String::new();
        BufReader::new(fixture.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        fixture.child.wait().unwrap();
        let child = observe(line.trim().parse().unwrap(), uid).unwrap();
        assert_eq!(child.parent_unique, root.unique);
        let mut observer = WorkObserver::new(&scope(vec![root])).unwrap();
        assert_eq!(observer.observe(&[child]).unwrap().owned, vec![child]);
        fixture.child.stdin.take();
        wait_gone(child, uid);
    }

    #[test]
    fn a_double_fork_with_a_lost_parent_cannot_be_reported_clean() {
        let mut fixture = fixture("double");
        let uid = rustix::process::geteuid().as_raw();
        let root = fixture.root.unwrap();
        let mut line = String::new();
        BufReader::new(fixture.child.stdout.take().unwrap())
            .read_line(&mut line)
            .unwrap();
        fixture.child.wait().unwrap();
        let child = observe(line.trim().parse().unwrap(), uid).unwrap();
        assert_ne!(child.parent_unique, root.unique);
        let mut observer = WorkObserver::new(&scope(vec![root])).unwrap();
        assert_eq!(observer.observe(&[child]).unwrap().unresolved, vec![child]);
        fixture.child.stdin.take();
        wait_gone(child, uid);
    }

    #[test]
    fn exec_preserves_the_owned_lifetime_without_freezing_its_executable() {
        let mut fixture = fixture("exec");
        let uid = rustix::process::geteuid().as_raw();
        let mut stdout = BufReader::new(fixture.child.stdout.take().unwrap());
        let mut line = String::new();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "ready");
        let before = observe(fixture.child.id(), uid).unwrap();
        fixture
            .child
            .stdin
            .as_mut()
            .unwrap()
            .write_all(b"x")
            .unwrap();
        line.clear();
        stdout.read_line(&mut line).unwrap();
        assert_eq!(line.trim(), "executed");
        assert_eq!(observe(fixture.child.id(), uid).unwrap(), before);
        fixture.child.stdin.take();
        fixture.child.wait().unwrap();
        assert!(remaining_root(before, uid).unwrap().is_none());
    }
}
