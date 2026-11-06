use anyhow::{Context, Result, bail, ensure};
use serde::{Serialize, de::DeserializeOwned};
use std::ffi::OsStr;
use std::fs::File;
use std::io::{Read, Write};
use std::path::{Component, Path, PathBuf, Prefix};

const MAX_STATE: u64 = 1024 * 1024;

pub(super) struct Root {
    pub path: PathBuf,
    pub directory: File,
}

impl Root {
    pub fn open(path: &Path, create: bool) -> Result<Option<Self>> {
        ensure!(
            path.is_absolute(),
            "Windows desktop state requires an absolute local path"
        );
        let mut components = path.components();
        ensure!(
            matches!(components.next(), Some(Component::Prefix(prefix)) if matches!(prefix.kind(), Prefix::Disk(_) | Prefix::VerbatimDisk(_))),
            "Windows desktop state requires a local disk"
        );
        ensure!(
            matches!(components.next(), Some(Component::RootDir)),
            "Windows desktop state requires a disk root"
        );
        let mut current = path
            .ancestors()
            .last()
            .context("state disk root")?
            .to_path_buf();
        let mut parent = glass_windows::open_directory_no_reparse(&current)
            .map_err(|_| anyhow::anyhow!("cannot retain desktop state disk root"))?;
        for component in components {
            let Component::Normal(name) = component else {
                bail!("invalid desktop state path component");
            };
            current.push(name);
            parent = match glass_windows::open_directory_child_readonly(&parent, name) {
                Ok(directory) => directory,
                Err(glass_windows::HostFsError::Integrity) => {
                    bail!("desktop state path contains a reparse point or changed identity")
                }
                Err(_) if !create => match std::fs::symlink_metadata(&current) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    _ => bail!("desktop state ancestor is inaccessible"),
                },
                Err(_) => glass_windows::create_private_child(&parent, name, true)
                    .context("create private desktop state directory")?,
            };
        }
        ensure!(
            glass_windows::file_is_private_to_current_user(&parent)?,
            "Windows desktop state directory must be private to its owner"
        );
        let result = Self {
            path: path.to_path_buf(),
            directory: parent,
        };
        result.validate()?;
        Ok(Some(result))
    }

    pub fn validate(&self) -> Result<()> {
        ensure!(
            glass_windows::directory_matches_path_no_reparse(&self.directory, &self.path)
                .map_err(|_| anyhow::anyhow!("desktop directory path is unavailable"))?,
            "desktop state directory changed identity"
        );
        ensure!(
            glass_windows::file_is_private_to_current_user(&self.directory)?,
            "desktop state directory permissions changed"
        );
        Ok(())
    }

    pub fn child(&self, name: &str, create: bool) -> Result<Option<Self>> {
        self.validate()?;
        let directory =
            match glass_windows::open_directory_child_readonly(&self.directory, OsStr::new(name)) {
                Ok(directory) => directory,
                Err(glass_windows::HostFsError::Integrity) => {
                    bail!("desktop child directory changed identity")
                }
                Err(_) if !create => match std::fs::symlink_metadata(self.path.join(name)) {
                    Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(None),
                    _ => bail!("desktop child directory is inaccessible"),
                },
                Err(_) => {
                    glass_windows::create_private_child(&self.directory, OsStr::new(name), true)?
                }
            };
        ensure!(
            glass_windows::file_is_private_to_current_user(&directory)?,
            "desktop child directory is not private"
        );
        Ok(Some(Self {
            path: self.path.join(name),
            directory,
        }))
    }

    pub fn names(&self) -> Result<Vec<String>> {
        self.validate()?;
        glass_windows::directory_entry_names(&self.directory)
            .map_err(|_| anyhow::anyhow!("read desktop state entries"))?
            .into_iter()
            .map(|name| {
                name.into_string()
                    .map_err(|_| anyhow::anyhow!("invalid desktop state filename"))
            })
            .collect()
    }

    pub fn bytes(&self, name: &str, limit: u64) -> Result<Option<Vec<u8>>> {
        self.validate()?;
        if !self
            .names()?
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(name))
        {
            return Ok(None);
        }
        for _ in 0..4 {
            let mut file =
                glass_windows::open_file_child_readonly(&self.directory, OsStr::new(name))
                    .map_err(|_| {
                        anyhow::anyhow!("desktop state file is unavailable or has changed")
                    })?;
            let same_path = || {
                glass_windows::file_matches_path_no_reparse(&file, &self.path.join(name))
                    .map_err(|_| anyhow::anyhow!("desktop state file path is unavailable"))
            };
            if !same_path()? {
                continue;
            }
            ensure!(
                glass_windows::file_is_private_to_current_user(&file)?,
                "desktop state file permissions changed"
            );
            if !glass_windows::file_has_single_link(&file)
                .map_err(|_| anyhow::anyhow!("read desktop state file links"))?
            {
                if !same_path()? {
                    continue;
                }
                bail!("desktop state file has an alias");
            }
            ensure!(
                file.metadata()?.len() <= limit,
                "desktop state file exceeds its size limit"
            );
            let mut bytes = Vec::new();
            (&mut file).take(limit + 1).read_to_end(&mut bytes)?;
            ensure!(
                bytes.len() as u64 <= limit,
                "desktop state file grew beyond its limit"
            );
            if !glass_windows::file_matches_path_no_reparse(&file, &self.path.join(name))
                .map_err(|_| anyhow::anyhow!("desktop state file path is unavailable"))?
            {
                continue;
            }
            self.validate()?;
            return Ok(Some(bytes));
        }
        bail!("desktop state changed repeatedly while being read")
    }

    pub fn read<T: DeserializeOwned>(&self, name: &str) -> Result<Option<T>> {
        self.bytes(name, MAX_STATE)?
            .map(|bytes| serde_json::from_slice(&bytes).context("parse Windows desktop state"))
            .transpose()
    }

    pub fn write<T: Serialize>(&self, name: &str, value: &T) -> Result<()> {
        let bytes = serde_json::to_vec(value)?;
        ensure!(
            bytes.len() as u64 <= MAX_STATE,
            "desktop state exceeds its limit"
        );
        self.write_bytes(name, &bytes)
    }

    pub fn write_bytes(&self, name: &str, bytes: &[u8]) -> Result<()> {
        self.validate()?;
        ensure!(
            matches!(
                Path::new(name).components().next(),
                Some(Component::Normal(_))
            ) && Path::new(name).components().count() == 1
                && !name.contains(':'),
            "invalid desktop state filename"
        );
        if self
            .names()?
            .iter()
            .any(|entry| entry.eq_ignore_ascii_case(name))
        {
            self.bytes(name, bytes.len().max(MAX_STATE as usize) as u64)?
                .context("existing desktop state disappeared")?;
        }
        let temporary = format!("tmp-{:032x}", rand::random::<u128>());
        let mut file =
            glass_windows::create_private_child(&self.directory, OsStr::new(&temporary), false)?;
        let result = (|| {
            file.write_all(bytes)?;
            file.sync_all()?;
            self.validate()?;
            std::fs::rename(self.path.join(&temporary), self.path.join(name))?;
            Ok(())
        })();
        if result.is_err() {
            let _ = glass_windows::remove_by_handle(&file);
        }
        result
    }
}

pub(super) fn base_path() -> Result<PathBuf> {
    if let Some(path) = std::env::var_os("GLASS_WINDOWS_DESKTOP_STATE_DIR") {
        return Ok(PathBuf::from(path));
    }
    Ok(
        directories::ProjectDirs::from("tech", "fixedwidth", "glass")
            .context("Windows local application-data directory is unavailable")?
            .data_local_dir()
            .join("windows-desktop"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn private_directory_and_atomic_state_roundtrip_with_read_control() {
        let scratch = tempfile::tempdir().expect("scratch");
        let path = scratch.path().join("state");
        let root = Root::open(&path, true)
            .expect("create private directory")
            .expect("root");
        root.write("configuration.json", &serde_json::json!({"revision":1}))
            .expect("first write");
        root.write("configuration.json", &serde_json::json!({"revision":2}))
            .expect("replacement");
        let read_only = Root::open(&path, false)
            .expect("reopen readonly")
            .expect("root");
        assert_eq!(
            read_only
                .read::<serde_json::Value>("CONFIGURATION.JSON")
                .expect("private state read"),
            Some(serde_json::json!({"revision":2}))
        );
    }

    #[test]
    fn status_lookup_does_not_create_a_missing_state_directory() {
        let scratch = tempfile::tempdir().expect("scratch");
        let path = scratch.path().join("missing");
        assert!(
            Root::open(&path, false)
                .expect("read only lookup")
                .is_none()
        );
        assert!(!path.exists());
    }

    #[test]
    fn a_state_file_alias_is_refused_without_modifying_either_link() {
        let scratch = tempfile::tempdir().expect("scratch");
        let path = scratch.path().join("state");
        let root = Root::open(&path, true).expect("create").expect("root");
        root.write("configuration.json", &serde_json::json!({"original":true}))
            .expect("state");
        let alias = scratch.path().join("alias");
        std::fs::hard_link(path.join("configuration.json"), &alias).expect("hard link");
        assert!(
            root.read::<serde_json::Value>("configuration.json")
                .is_err()
        );
        assert!(
            root.write("configuration.json", &serde_json::json!({"changed":true}))
                .is_err()
        );
        assert_eq!(
            std::fs::read(alias).expect("alias unchanged"),
            br#"{"original":true}"#
        );
    }
}
