//! Authenticate the active worker, independently of who registered the activation job.

use std::fs::File;
use std::io::Read;
use std::os::unix::fs::{DirBuilderExt, FileTypeExt, MetadataExt, PermissionsExt};
use std::os::unix::net::{UnixListener, UnixStream};
use std::path::{Path, PathBuf};

use glass_core::{Deadline, GlassError, Result};
use rustix::fs::{AtFlags, Mode, OFlags, open, unlinkat};

fn failure(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(error.to_string())
}

pub(super) struct Callback {
    directory: PathBuf,
    retained: File,
    listener: UnixListener,
}

impl Callback {
    pub(super) fn new(owner: u32) -> Result<Self> {
        let parent = Path::new("/private/tmp");
        let meta = std::fs::symlink_metadata(parent).map_err(failure)?;
        if !meta.is_dir() || meta.uid() != 0 || meta.mode() & 0o1000 == 0 {
            return Err(failure(
                "private callback requires root-owned sticky temporary storage",
            ));
        }
        let directory = parent.join(format!("glass-connection-{:032x}", rand::random::<u128>()));
        std::fs::DirBuilder::new()
            .mode(0o700)
            .create(&directory)
            .map_err(failure)?;
        let retained = File::from(
            open(
                &directory,
                OFlags::RDONLY | OFlags::DIRECTORY | OFlags::NOFOLLOW | OFlags::CLOEXEC,
                Mode::empty(),
            )
            .map_err(failure)?,
        );
        let metadata = retained.metadata().map_err(failure)?;
        if metadata.uid() != owner {
            return Err(failure("callback directory ownership changed"));
        }
        glass_macos::private_session::require_write_protected(&retained, owner).map_err(failure)?;
        let path = directory.join("worker.sock");
        let listener = match UnixListener::bind(&path) {
            Ok(listener) => listener,
            Err(error) => {
                let _ = std::fs::remove_dir(&directory);
                return Err(failure(error));
            }
        };
        let result = Self {
            directory,
            retained,
            listener,
        };
        std::fs::set_permissions(result.path(), std::fs::Permissions::from_mode(0o666))
            .map_err(failure)?;
        std::fs::set_permissions(&result.directory, std::fs::Permissions::from_mode(0o711))
            .map_err(failure)?;
        result.listener.set_nonblocking(true).map_err(failure)?;
        Ok(result)
    }

    pub(super) fn path(&self) -> PathBuf {
        self.directory.join("worker.sock")
    }

    pub(super) fn accept(
        &self,
        worker: u32,
        requirement: &[u8],
        deadline: Deadline,
    ) -> Result<(UnixStream, u32, u32)> {
        loop {
            let remaining = deadline
                .remaining()
                .map(rustix::event::Timespec::try_from)
                .transpose()
                .map_err(failure)?;
            if deadline.has_passed() {
                return Err(GlassError::deadline_not_started(
                    "private worker authentication",
                ));
            }
            let mut poll = [rustix::event::PollFd::new(
                &self.listener,
                rustix::event::PollFlags::IN,
            )];
            match rustix::event::poll(&mut poll, remaining.as_ref()) {
                Err(rustix::io::Errno::INTR) | Ok(0) => continue,
                Err(error) => return Err(failure(error)),
                Ok(_) => {}
            }
            let (stream, _) = match self.listener.accept() {
                Err(error) if error.kind() == std::io::ErrorKind::WouldBlock => continue,
                result => result.map_err(failure)?,
            };
            if glass_macos::worker_socket::peer_uid(&stream).ok() != Some(worker) {
                continue;
            }
            // Authenticate the kernel token before consuming any self-reported greeting.
            let Ok((pid, session)) =
                glass_macos::private_session::authenticate_peer(&stream, requirement, worker)
            else {
                continue;
            };
            stream.set_nonblocking(true).map_err(failure)?;
            return Ok((stream, pid, session));
        }
    }
}

impl Drop for Callback {
    fn drop(&mut self) {
        let _ = unlinkat(&self.retained, "worker.sock", AtFlags::empty());
        if let Ok(current) = std::fs::symlink_metadata(&self.directory)
            && let Ok(retained) = self.retained.metadata()
            && current.dev() == retained.dev()
            && current.ino() == retained.ino()
        {
            let _ = std::fs::remove_dir(&self.directory);
        }
    }
}

pub(crate) fn broker_requirement() -> Result<Vec<u8>> {
    let file = File::from(
        open(
            glass_macos::private_session::root_for(rustix::process::geteuid().as_raw())
                .join("worker.requirement"),
            OFlags::RDONLY | OFlags::NOFOLLOW | OFlags::NONBLOCK | OFlags::CLOEXEC,
            Mode::empty(),
        )
        .map_err(failure)?,
    );
    let meta = file.metadata().map_err(failure)?;
    if !meta.is_file()
        || meta.uid() != 0
        || meta.nlink() != 1
        || meta.mode() & 0o022 != 0
        || meta.len() == 0
        || meta.len() > 65536
    {
        return Err(failure(
            "worker signing requirement is not protected; run glass-mcp setup",
        ));
    }
    glass_macos::private_session::require_write_protected(&file, 0).map_err(failure)?;
    let mut bytes = Vec::new();
    file.take(65537).read_to_end(&mut bytes).map_err(failure)?;
    if bytes.len() > 65536 {
        return Err(failure("worker requirement exceeds its bound"));
    }
    Ok(bytes)
}

pub(super) fn connect(path: &Path, owner: u32) -> anyhow::Result<tokio::net::UnixStream> {
    use anyhow::ensure;
    let parent = path
        .parent()
        .ok_or_else(|| anyhow::anyhow!("callback directory missing"))?;
    ensure!(
        parent.parent() == Some(Path::new("/private/tmp"))
            && parent
                .file_name()
                .is_some_and(|name| name.to_string_lossy().starts_with("glass-connection-"))
            && path.file_name().is_some_and(|name| name == "worker.sock"),
        "invalid callback endpoint"
    );
    let metadata = std::fs::symlink_metadata(parent)?;
    ensure!(
        metadata.is_dir() && metadata.uid() == owner && metadata.mode() & 0o777 == 0o711,
        "callback directory is not owner-controlled"
    );
    let metadata = std::fs::symlink_metadata(path)?;
    ensure!(
        metadata.file_type().is_socket() && metadata.uid() == owner,
        "callback endpoint was replaced"
    );
    let stream = UnixStream::connect(path)?;
    ensure!(
        glass_macos::worker_socket::peer_uid(&stream)? == owner,
        "callback listener is not the enrolled owner"
    );
    stream.set_nonblocking(true)?;
    Ok(tokio::net::UnixStream::from_std(stream)?)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn callback_drop_removes_its_own_endpoint() {
        let callback = Callback::new(rustix::process::getuid().as_raw()).unwrap();
        let directory = callback.directory.clone();
        let metadata = std::fs::symlink_metadata(callback.path()).unwrap();
        assert!(metadata.file_type().is_socket());
        assert_eq!(metadata.mode() & 0o777, 0o666);
        assert_eq!(
            std::fs::symlink_metadata(&directory).unwrap().mode() & 0o777,
            0o711
        );
        drop(callback);
        assert!(!directory.exists());
    }

    #[test]
    fn wrong_callback_uids_are_discarded_without_ending_the_accept_window() {
        let owner = rustix::process::getuid().as_raw();
        let callback = Callback::new(owner).unwrap();
        let path = callback.path();
        let sender = std::thread::spawn(move || {
            let first = UnixStream::connect(&path).unwrap();
            let second = UnixStream::connect(path).unwrap();
            (first, second)
        });
        let deadline = Deadline::from_millis(100);
        let error = callback.accept(owner + 1, &[], deadline).err().unwrap();
        assert!(deadline.has_passed());
        assert!(error.to_string().contains("private worker authentication"));
        drop(sender.join().unwrap());
    }
}
