//! The normal user's explicit, credential-free setup intent.

use anyhow::{Context, ensure};
use rustix::fs::{Mode, OFlags, openat};
use serde::{Deserialize, Serialize};
use std::fs::File;
use std::io::{Read, Write};
use std::os::unix::fs::MetadataExt;

pub(crate) const FILE: &str = "pending-setup.json";

#[derive(Debug)]
pub(crate) struct Superseded;

impl std::fmt::Display for Superseded {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str("saved setup was cancelled or replaced; this window did not restart it")
    }
}

impl std::error::Error for Superseded {}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Identity {
    pub uid: u32,
    pub uuid: String,
}

impl Identity {
    pub(crate) fn matches(&self, uid: u32, uuid: &str) -> bool {
        self.uid == uid && self.uuid.eq_ignore_ascii_case(uuid)
    }
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct Pending {
    version: u32,
    pub id: String,
    pub owner: u32,
    pub name: String,
    pub identity: Option<Identity>,
    pub create: bool,
    pub launchagent: bool,
    pub addr: String,
}

#[derive(Clone)]
pub(crate) struct WindowAttempt {
    displayed: Option<Pending>,
    owned: Option<String>,
}

impl WindowAttempt {
    pub(crate) fn new(displayed: Option<Pending>) -> Self {
        Self {
            displayed,
            owned: None,
        }
    }

    pub(crate) fn previous_id(&self) -> Option<&str> {
        self.owned_id().or_else(|| self.displayed_id())
    }

    pub(crate) fn owned_id(&self) -> Option<&str> {
        self.owned.as_deref()
    }

    pub(crate) fn displayed_id(&self) -> Option<&str> {
        self.displayed.as_ref().map(|pending| pending.id.as_str())
    }

    pub(crate) fn remember(&mut self, id: String) {
        self.owned = Some(id);
    }

    pub(crate) fn complete(&mut self) {
        self.owned = None;
        self.displayed = None;
    }

    pub(crate) fn creation_retry(
        &self,
        current: Option<&Pending>,
        name: &str,
        prompted: Option<&str>,
    ) -> anyhow::Result<Option<Pending>> {
        Pending::creation_retry(
            self.displayed.as_ref(),
            self.owned_id(),
            current,
            name,
            prompted,
        )
    }
}

impl Pending {
    pub(crate) fn creation_retry(
        displayed: Option<&Self>,
        owned: Option<&str>,
        current: Option<&Self>,
        name: &str,
        prompted: Option<&str>,
    ) -> anyhow::Result<Option<Self>> {
        if let Some(prompted) = prompted
            && !displayed
                .is_some_and(|saved| saved.create && saved.id == prompted && saved.name == name)
        {
            return Err(Superseded.into());
        }
        let Some(expected) = owned.or_else(|| displayed.map(|saved| saved.id.as_str())) else {
            return Ok(None);
        };
        let Some(current) = current.filter(|saved| saved.id == expected) else {
            return Err(Superseded.into());
        };
        Ok((current.create && current.name == name).then(|| current.clone()))
    }

    pub(crate) fn may_resume(
        &self,
        owner: u32,
        name: &str,
        identity: &Identity,
        enabled: bool,
        completed_creation: Option<&Identity>,
    ) -> bool {
        let expected = self.identity.as_ref().or({
            if self.create {
                completed_creation
            } else {
                None
            }
        });
        enabled
            && self.owner == owner
            && self.name == name
            && expected.is_some_and(|expected| expected.matches(identity.uid, &identity.uuid))
    }

    pub(crate) fn new(
        owner: u32,
        name: &str,
        identity: Option<Identity>,
        create: bool,
        launchagent: bool,
        addr: &str,
    ) -> anyhow::Result<Self> {
        let pending = Self {
            version: 1,
            id: format!("{:032x}", rand::random::<u128>()),
            owner,
            name: name.into(),
            identity,
            create,
            launchagent,
            addr: addr.into(),
        };
        pending.validate(owner)?;
        Ok(pending)
    }

    fn validate(&self, owner: u32) -> anyhow::Result<()> {
        ensure!(
            self.version == 1
                && self.owner == owner
                && self.id.len() == 32
                && self.id.bytes().all(|byte| byte.is_ascii_hexdigit())
                && !self.name.is_empty()
                && self.name.len() <= 64
                && self
                    .name
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_')
                && !self.addr.is_empty()
                && self.addr.len() <= 256
                && !self.addr.chars().any(char::is_control),
            "saved Glass setup is invalid or belongs to another user"
        );
        if let Some(identity) = &self.identity {
            ensure!(
                identity.uid >= 501
                    && identity.uid != owner
                    && identity.uuid.len() == 36
                    && identity.uuid.bytes().enumerate().all(|(index, byte)| {
                        if [8, 13, 18, 23].contains(&index) {
                            byte == b'-'
                        } else {
                            byte.is_ascii_hexdigit()
                        }
                    }),
                "saved Glass setup account identity is invalid"
            );
        }
        Ok(())
    }
}

fn private(file: &File, owner: u32, directory: bool) -> anyhow::Result<()> {
    let metadata = file.metadata()?;
    ensure!(
        metadata.uid() == owner
            && metadata.mode() & 0o077 == 0
            && if directory {
                metadata.is_dir()
            } else {
                metadata.is_file() && metadata.nlink() == 1
            },
        "saved setup storage is not private to this user"
    );
    #[cfg(target_os = "macos")]
    glass_macos::workspace_access::require_owner_private(file)?;
    Ok(())
}

struct Mutation(File);

impl Drop for Mutation {
    fn drop(&mut self) {
        if let Err(error) = fs4::FileExt::unlock(&self.0) {
            eprintln!("glass: saved setup lock release failed: {error}");
        }
    }
}

fn mutation(root: &File, owner: u32) -> anyhow::Result<Mutation> {
    private(root, owner, true)?;
    let file = File::from(openat(
        root,
        "pending.lock",
        OFlags::RDWR | OFlags::CREATE | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    private(&file, owner, false)?;
    fs4::FileExt::lock(&file).context("saved Glass setup is changing")?;
    Ok(Mutation(file))
}

fn bytes(root: &File, owner: u32) -> anyhow::Result<Option<Vec<u8>>> {
    private(root, owner, true)?;
    let file = match openat(
        root,
        FILE,
        OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
        Mode::empty(),
    ) {
        Ok(file) => File::from(file),
        Err(rustix::io::Errno::NOENT) => return Ok(None),
        Err(error) => return Err(error.into()),
    };
    private(&file, owner, false)?;
    let mut bytes = Vec::new();
    file.take(8193).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() <= 8192,
        "saved Glass setup exceeded its size limit"
    );
    Ok(Some(bytes))
}

pub(crate) fn read(root: &File, owner: u32) -> anyhow::Result<Option<Pending>> {
    let Some(bytes) = bytes(root, owner)? else {
        return Ok(None);
    };
    let pending: Pending =
        serde_json::from_slice(&bytes).context("saved Glass setup is unreadable")?;
    pending.validate(owner)?;
    Ok(Some(pending))
}

pub(crate) fn remember(
    root: &File,
    pending: &Pending,
    previous: Option<&str>,
) -> anyhow::Result<()> {
    pending.validate(pending.owner)?;
    let _mutation = mutation(root, pending.owner)?;
    if let Some(previous) = previous {
        if !read(root, pending.owner)?.is_some_and(|current| current.id == previous) {
            return Err(Superseded.into());
        }
    } else {
        bytes(root, pending.owner)?;
    }
    let temporary = format!(".pending-{}", pending.id);
    let mut file = File::from(openat(
        root,
        &temporary,
        OFlags::WRONLY | OFlags::CREATE | OFlags::EXCL | OFlags::NOFOLLOW | OFlags::CLOEXEC,
        Mode::from_raw_mode(0o600),
    )?);
    let result = (|| {
        private(&file, pending.owner, false)?;
        file.write_all(&serde_json::to_vec(pending)?)?;
        file.sync_all()?;
        rustix::fs::renameat(root, &temporary, root, FILE)?;
        root.sync_all()?;
        Ok(())
    })();
    if result.is_err() {
        let _ = rustix::fs::unlinkat(root, &temporary, rustix::fs::AtFlags::empty());
    }
    result
}

pub(crate) fn forget(root: &File, owner: u32, id: &str) -> anyhow::Result<()> {
    let _mutation = mutation(root, owner)?;
    if read(root, owner)?.is_some_and(|pending| pending.id == id) {
        rustix::fs::unlinkat(root, FILE, rustix::fs::AtFlags::empty())?;
        root.sync_all()?;
    }
    Ok(())
}

/// Explicit disable holds the exclusive setup lease, excluding all intent writers.
pub(crate) fn discard(root: &File, owner: u32) -> anyhow::Result<()> {
    private(root, owner, true)?;
    let metadata = match rustix::fs::statat(root, FILE, rustix::fs::AtFlags::SYMLINK_NOFOLLOW) {
        Ok(metadata) => metadata,
        Err(rustix::io::Errno::NOENT) => return Ok(()),
        Err(error) => return Err(error.into()),
    };
    ensure!(
        metadata.st_uid == owner,
        "saved setup entry belongs to another user; it was retained"
    );
    match rustix::fs::unlinkat(root, FILE, rustix::fs::AtFlags::empty()) {
        Ok(()) => root.sync_all()?,
        Err(rustix::io::Errno::NOENT) => {}
        Err(error) => return Err(error.into()),
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::{PermissionsExt, symlink};

    fn storage() -> (tempfile::TempDir, File, Pending) {
        let directory = tempfile::tempdir().unwrap();
        std::fs::set_permissions(directory.path(), std::fs::Permissions::from_mode(0o700)).unwrap();
        let root = File::open(directory.path()).unwrap();
        let owner = rustix::process::getuid().as_raw();
        let pending = Pending::new(
            owner,
            "development",
            Some(Identity {
                uid: owner.max(501) + 1,
                uuid: "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee".into(),
            }),
            false,
            false,
            "127.0.0.1:7300",
        )
        .unwrap();
        (directory, root, pending)
    }

    #[test]
    fn a_completed_window_can_begin_another_explicit_setup() {
        let (_directory, root, first) = storage();
        remember(&root, &first, None).unwrap();
        let mut window = WindowAttempt::new(Some(first.clone()));
        window.remember(first.id.clone());
        forget(&root, first.owner, &first.id).unwrap();
        window.complete();
        let mut next = first.clone();
        next.id = "b".repeat(32);
        remember(&root, &next, window.previous_id()).unwrap();
        assert_eq!(read(&root, first.owner).unwrap(), Some(next));
    }

    #[test]
    fn completion_of_an_attempt_created_in_this_window_retires_its_token() {
        let (_directory, root, first) = storage();
        let mut window = WindowAttempt::new(None);
        remember(&root, &first, window.previous_id()).unwrap();
        window.remember(first.id.clone());
        forget(&root, first.owner, &first.id).unwrap();
        window.complete();
        let mut next = first.clone();
        next.id = "b".repeat(32);
        remember(&root, &next, window.previous_id()).unwrap();
        assert_eq!(read(&root, first.owner).unwrap(), Some(next));
    }

    #[test]
    fn a_cancelled_window_without_completion_cannot_restart_its_attempt() {
        let (_directory, root, first) = storage();
        remember(&root, &first, None).unwrap();
        let mut window = WindowAttempt::new(Some(first.clone()));
        window.remember(first.id.clone());
        forget(&root, first.owner, &first.id).unwrap();
        assert!(
            remember(&root, &first, window.previous_id())
                .unwrap_err()
                .is::<Superseded>()
        );
        assert!(read(&root, first.owner).unwrap().is_none());
    }

    #[test]
    fn an_old_window_cannot_replace_another_windows_current_attempt() {
        let (_directory, root, first) = storage();
        remember(&root, &first, None).unwrap();
        let mut window = WindowAttempt::new(Some(first.clone()));
        window.remember(first.id.clone());
        let mut newer = first.clone();
        newer.id = "b".repeat(32);
        remember(&root, &newer, Some(&first.id)).unwrap();
        assert!(
            remember(&root, &first, window.previous_id())
                .unwrap_err()
                .is::<Superseded>()
        );
        assert_eq!(read(&root, first.owner).unwrap(), Some(newer));
    }

    #[test]
    fn completed_creation_is_not_offered_as_a_password_resume() {
        let (_directory, _root, mut first) = storage();
        first.create = true;
        first.identity = None;
        let mut window = WindowAttempt::new(Some(first.clone()));
        window.remember(first.id.clone());
        window.complete();
        assert!(
            window
                .creation_retry(None, "newaccount", None)
                .unwrap()
                .is_none()
        );
        assert!(
            window
                .creation_retry(None, &first.name, Some(&first.id))
                .unwrap_err()
                .is::<Superseded>()
        );
    }

    #[test]
    fn a_reopened_store_retains_only_explicit_setup_metadata() {
        let (directory, root, pending) = storage();
        remember(&root, &pending, None).unwrap();
        drop(root);
        assert_eq!(
            read(&File::open(directory.path()).unwrap(), pending.owner).unwrap(),
            Some(pending)
        );
    }

    #[test]
    fn automatic_resume_requires_the_same_enabled_account_and_owner() {
        let (_directory, _root, pending) = storage();
        let identity = pending.identity.as_ref().unwrap();
        assert!(pending.may_resume(pending.owner, &pending.name, identity, true, None));
        assert!(!pending.may_resume(pending.owner, &pending.name, identity, false, None));
        assert!(!pending.may_resume(pending.owner + 1, &pending.name, identity, true, None));
        assert!(!pending.may_resume(pending.owner, "another", identity, true, None));
        let mut replacement = identity.clone();
        replacement.uuid = "bbbbbbbb-bbbb-4ccc-8ddd-eeeeeeeeeeee".into();
        assert!(!pending.may_resume(pending.owner, &pending.name, &replacement, true, None));
    }

    #[test]
    fn an_interrupted_create_request_needs_its_completed_privileged_receipt_to_resume() {
        let (_directory, _root, mut pending) = storage();
        let identity = pending.identity.take().unwrap();
        pending.create = true;
        assert!(!pending.may_resume(pending.owner, &pending.name, &identity, true, None));
        assert!(pending.may_resume(
            pending.owner,
            &pending.name,
            &identity,
            true,
            Some(&identity)
        ));
        pending.create = false;
        assert!(!pending.may_resume(
            pending.owner,
            &pending.name,
            &identity,
            true,
            Some(&identity)
        ));
    }

    #[test]
    fn cancellation_of_an_old_window_cannot_remove_a_newer_setup() {
        let (_directory, root, first) = storage();
        remember(&root, &first, None).unwrap();
        let mut next = first.clone();
        next.id = "bb".repeat(16);
        remember(&root, &next, None).unwrap();
        forget(&root, first.owner, &first.id).unwrap();
        assert_eq!(read(&root, first.owner).unwrap(), Some(next.clone()));
        forget(&root, next.owner, &next.id).unwrap();
        assert!(read(&root, next.owner).unwrap().is_none());
    }

    #[test]
    fn a_cancelled_or_replaced_intent_cannot_be_republished_by_a_stale_resume() {
        let (_directory, root, first) = storage();
        remember(&root, &first, None).unwrap();
        let mut next = first.clone();
        next.id = "bb".repeat(16);
        forget(&root, first.owner, &first.id).unwrap();
        assert!(remember(&root, &next, Some(&first.id)).is_err());
        assert!(read(&root, first.owner).unwrap().is_none());
        remember(&root, &first, None).unwrap();
        remember(&root, &next, Some(&first.id)).unwrap();
        assert!(remember(&root, &first, Some(&first.id)).is_err());
        assert_eq!(read(&root, first.owner).unwrap(), Some(next));
    }

    #[test]
    fn a_creation_retry_claims_its_latest_checkpoint_and_cannot_revive_cancelled_work() {
        let (_directory, root, mut displayed) = storage();
        displayed.create = true;
        displayed.identity = None;
        remember(&root, &displayed, None).unwrap();
        let mut checkpoint = displayed.clone();
        checkpoint.id = "bb".repeat(16);
        remember(&root, &checkpoint, Some(&displayed.id)).unwrap();
        let retry = |current: Option<&Pending>, name: &str, prompted: Option<&str>| {
            Pending::creation_retry(
                Some(&displayed),
                Some(&checkpoint.id),
                current,
                name,
                prompted,
            )
        };
        let current = read(&root, displayed.owner).unwrap();
        assert_eq!(
            retry(current.as_ref(), &displayed.name, Some(&displayed.id)).unwrap(),
            Some(checkpoint.clone())
        );
        assert_eq!(
            retry(current.as_ref(), &displayed.name, None).unwrap(),
            Some(checkpoint.clone())
        );
        assert_eq!(retry(current.as_ref(), "newname", None).unwrap(), None);
        assert!(retry(current.as_ref(), "newname", Some(&displayed.id)).is_err());
        forget(&root, displayed.owner, &checkpoint.id).unwrap();
        assert!(
            retry(
                read(&root, displayed.owner).unwrap().as_ref(),
                &displayed.name,
                Some(&displayed.id)
            )
            .is_err()
        );
        remember(&root, &displayed, None).unwrap();
        assert!(
            retry(
                read(&root, displayed.owner).unwrap().as_ref(),
                &displayed.name,
                None
            )
            .is_err()
        );
        assert!(remember(&root, &checkpoint, Some(&checkpoint.id)).is_err());
    }

    #[test]
    fn foreign_permissions_links_and_credentials_are_refused() {
        let (directory, root, pending) = storage();
        let foreign = directory.path().join("unrelated");
        std::fs::write(&foreign, b"retained").unwrap();
        symlink(&foreign, directory.path().join(FILE)).unwrap();
        assert!(remember(&root, &pending, None).is_err());
        assert_eq!(std::fs::read(&foreign).unwrap(), b"retained");
        std::fs::remove_file(directory.path().join(FILE)).unwrap();
        remember(&root, &pending, None).unwrap();
        std::fs::set_permissions(
            directory.path().join(FILE),
            std::fs::Permissions::from_mode(0o644),
        )
        .unwrap();
        assert!(read(&root, pending.owner).is_err());
        let mut value = serde_json::to_value(&pending).unwrap();
        value["password"] = "must never be accepted".into();
        assert!(serde_json::from_value::<Pending>(value).is_err());
        assert!(read(&root, pending.owner + 1).is_err());
    }

    #[test]
    fn explicit_disable_can_discard_invalid_intent_without_following_a_link() {
        let (directory, root, pending) = storage();
        std::fs::write(directory.path().join(FILE), b"incomplete metadata").unwrap();
        assert!(read(&root, pending.owner).is_err());
        discard(&root, pending.owner).unwrap();
        let unrelated = directory.path().join("unrelated");
        std::fs::write(&unrelated, b"retained").unwrap();
        symlink(&unrelated, directory.path().join(FILE)).unwrap();
        discard(&root, pending.owner).unwrap();
        assert_eq!(std::fs::read(unrelated).unwrap(), b"retained");
        assert!(read(&root, pending.owner).unwrap().is_none());
    }
}
