use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path};

use anyhow::{Context, ensure};
use cap_fs_ext::{DirExt, FollowSymlinks, OpenOptionsFollowExt};
use cap_std::fs::{Dir, DirBuilder, OpenOptions};

pub(super) fn open_directory(path: &Path) -> anyhow::Result<Dir> {
    ensure!(path.is_absolute(), "expected an absolute directory");
    let mut base = std::path::PathBuf::new();
    let mut directory = None;
    for component in path.components() {
        match component {
            Component::Prefix(_) | Component::RootDir => base.push(component),
            Component::Normal(name) => {
                let parent = match directory.take() {
                    Some(parent) => parent,
                    None => Dir::open_ambient_dir(&base, cap_std::ambient_authority())?,
                };
                directory = Some(parent.open_dir_nofollow(name)?);
            }
            _ => anyhow::bail!("directory must not contain parent components"),
        }
    }
    directory.context("a filesystem root is not an owned directory")
}

pub(super) fn check_owner(dir: &Dir) -> anyhow::Result<()> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let metadata = dir.dir_metadata()?;
        ensure!(
            metadata.uid() == rustix::process::geteuid().as_raw(),
            "trace directory is not owned by the current user"
        );
        ensure!(
            metadata.mode() & 0o022 == 0,
            "trace directory must not be writable by other users"
        );
    }
    #[cfg(windows)]
    {
        ensure!(
            glass_windows::file_is_private_to_current_user(&dir.try_clone()?.into_std_file())?,
            "trace directory must be owned by the current user with a protected owner-and-SYSTEM DACL"
        );
    }
    Ok(())
}

pub(super) fn same_directory(first: &Dir, second: &Dir) -> anyhow::Result<bool> {
    #[cfg(unix)]
    {
        use cap_std::fs::MetadataExt;
        let first = first.dir_metadata()?;
        let second = second.dir_metadata()?;
        Ok(first.dev() == second.dev() && first.ino() == second.ino())
    }
    #[cfg(windows)]
    glass_windows::same_file_object(
        &first.try_clone()?.into_std_file(),
        &second.try_clone()?.into_std_file(),
    )
    .map_err(|_| anyhow::anyhow!("cannot compare directory identities"))
}

pub(super) fn create_directory(parent: &Dir, name: &str) -> anyhow::Result<Dir> {
    valid_name(name)?;
    let builder = DirBuilder::new();
    #[cfg(unix)]
    let builder = {
        use cap_std::fs::DirBuilderExt;
        let mut builder = builder;
        builder.mode(0o700);
        builder
    };
    parent.create_dir_with(name, &builder)?;
    let child = parent.open_dir_nofollow(name)?;
    #[cfg(windows)]
    glass_windows::restrict_directory_child_to_current_user(
        &parent.try_clone()?.into_std_file(),
        std::ffi::OsStr::new(name),
    )?;
    Ok(child)
}

pub(super) fn valid_name(name: &str) -> anyhow::Result<()> {
    ensure!(
        !name.is_empty()
            && name.len() <= 128
            && name
                .bytes()
                .all(|b| b.is_ascii_alphanumeric() || b == b'-' || b == b'.' || b == b'_'),
        "invalid evidence filename"
    );
    ensure!(name != "." && name != "..", "invalid evidence filename");
    Ok(())
}

pub(super) fn create_file(dir: &Dir, name: &str) -> anyhow::Result<File> {
    valid_name(name)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(true)
        .create_new(true)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.mode(0o600);
    }
    #[cfg(windows)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.access_mode(windows::Win32::Storage::FileSystem::FILE_ALL_ACCESS.0);
    }
    let file = dir.open_with(name, &options)?.into_std();
    #[cfg(windows)]
    glass_windows::restrict_file_to_current_user(&file)?;
    Ok(file)
}

pub(super) fn open_file(dir: &Dir, name: &str, writable: bool) -> anyhow::Result<File> {
    valid_name(name)?;
    let mut options = OpenOptions::new();
    options
        .read(true)
        .write(writable)
        .follow(FollowSymlinks::No);
    #[cfg(unix)]
    {
        use cap_std::fs::OpenOptionsExt;
        options.custom_flags(rustix::fs::OFlags::NONBLOCK.bits() as i32);
    }
    let file = dir.open_with(name, &options)?.into_std();
    let metadata = file.metadata()?;
    ensure!(metadata.is_file(), "evidence is not a regular file");
    #[cfg(unix)]
    {
        use std::os::unix::fs::MetadataExt;
        ensure!(metadata.nlink() == 1, "hard-linked evidence is refused");
    }
    #[cfg(windows)]
    ensure!(
        glass_windows::file_has_single_link(&file)
            .map_err(|_| anyhow::anyhow!("cannot inspect evidence links"))?,
        "hard-linked evidence is refused"
    );
    Ok(file)
}

pub(super) fn read_bounded(dir: &Dir, name: &str, limit: u64) -> anyhow::Result<Vec<u8>> {
    let file = open_file(dir, name, false)?;
    ensure!(
        file.metadata()?.len() <= limit,
        "evidence exceeds its size limit"
    );
    let mut bytes = Vec::new();
    file.take(limit + 1).read_to_end(&mut bytes)?;
    ensure!(
        bytes.len() as u64 <= limit,
        "evidence grew beyond its size limit"
    );
    Ok(bytes)
}

pub(super) fn write_atomic(dir: &Dir, name: &str, bytes: &[u8]) -> anyhow::Result<()> {
    let temporary = format!("pending-{}", crate::artifacts::new_server_id());
    let result = (|| {
        let mut file = create_file(dir, &temporary)?;
        file.write_all(bytes)?;
        file.flush()?;
        drop(file);
        dir.rename(&temporary, dir, name)?;
        Ok(())
    })();
    if result.is_err() {
        let _ = dir.remove_file(&temporary);
    }
    result
}
