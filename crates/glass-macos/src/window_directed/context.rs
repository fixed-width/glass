//! Effective command startup facts; environment contents stay private and unqualified.

use std::collections::BTreeMap;
use std::ffi::{OsStr, OsString};
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};
use std::process::Command;

use glass_core::{AppSpec, Result};

use super::{refusal, validate_environment};

pub(super) const MAX_ARGUMENT_BYTES: usize = 128 * 1024;
const MAX_ARGUMENT_COUNT: usize = 4096;
pub(super) const VNODE_RECORD_BYTES: usize = 2352;

#[derive(Debug, PartialEq, Eq)]
pub(super) struct DirectoryIdentity {
    path: PathBuf,
    device: u64,
    inode: u64,
}

impl DirectoryIdentity {
    fn read(path: &Path) -> Result<Self> {
        let path =
            std::fs::canonicalize(path).map_err(|_| refusal("startup directory is unavailable"))?;
        let metadata = std::fs::metadata(&path)
            .map_err(|_| refusal("startup directory metadata is unavailable"))?;
        if !metadata.is_dir() || metadata.dev() == 0 || metadata.ino() == 0 {
            return Err(refusal("startup directory identity is incomplete"));
        }
        Ok(Self {
            path,
            device: metadata.dev(),
            inode: metadata.ino(),
        })
    }
}

// No Debug: an inherited environment can contain credentials.
pub(crate) struct LaunchContext {
    arguments: Vec<OsString>,
    environment: BTreeMap<OsString, OsString>,
    directory: DirectoryIdentity,
}

impl LaunchContext {
    pub(super) fn capture(spec: &AppSpec) -> Result<Self> {
        let cwd = spec.cwd.clone().map_or_else(
            || std::env::current_dir().map_err(|_| refusal("startup directory is unavailable")),
            Ok,
        )?;
        Self::from_environment(spec, std::env::vars_os(), &cwd)
    }

    fn from_environment(
        spec: &AppSpec,
        inherited: impl IntoIterator<Item = (OsString, OsString)>,
        cwd: &Path,
    ) -> Result<Self> {
        let mut environment: BTreeMap<_, _> = inherited.into_iter().collect();
        for (key, value) in &spec.env {
            environment.insert(key.into(), value.into());
        }
        let names: Vec<_> = environment
            .keys()
            .map(|key| key.to_string_lossy())
            .collect();
        validate_environment(names.iter().map(|key| key.as_ref()))?;
        let arguments: Vec<OsString> = spec.run.iter().map(OsString::from).collect();
        let mut bytes = 0_usize;
        for value in arguments
            .iter()
            .chain(environment.keys())
            .chain(environment.values())
        {
            if value.as_bytes().contains(&0) {
                return Err(refusal("startup context contains an invalid string"));
            }
            bytes = bytes.saturating_add(value.as_bytes().len() + 1);
        }
        if arguments.is_empty()
            || arguments.len() > MAX_ARGUMENT_COUNT
            || bytes > MAX_ARGUMENT_BYTES / 2
            || environment
                .keys()
                .any(|key| key.is_empty() || key.as_bytes().contains(&b'='))
        {
            return Err(refusal(
                "startup context is invalid or exceeds inspection bounds",
            ));
        }
        Ok(Self {
            arguments,
            environment,
            directory: DirectoryIdentity::read(cwd)?,
        })
    }

    pub(crate) fn apply(&self, command: &mut Command) {
        command
            .env_clear()
            .envs(&self.environment)
            .current_dir(&self.directory.path);
    }

    pub(crate) fn cwd(&self) -> &Path {
        &self.directory.path
    }

    pub(super) fn require_current(
        &self,
        arguments: &[OsString],
        directory: &DirectoryIdentity,
    ) -> Result<()> {
        if arguments != self.arguments || directory != &self.directory {
            return Err(refusal("bound startup arguments or directory changed"));
        }
        if DirectoryIdentity::read(&self.directory.path)? != self.directory {
            return Err(refusal("bound startup directory was replaced"));
        }
        Ok(())
    }
}

pub(super) fn parse_arguments(bytes: &[u8], length: usize) -> Result<Vec<OsString>> {
    let bytes = bytes
        .get(..length)
        .filter(|bytes| bytes.len() >= 4 && bytes.len() <= MAX_ARGUMENT_BYTES)
        .ok_or_else(|| refusal("kernel argument response is incomplete"))?;
    let count = i32::from_ne_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| refusal("kernel argument count is incomplete"))?,
    );
    let count = usize::try_from(count)
        .ok()
        .filter(|count| *count > 0 && *count <= MAX_ARGUMENT_COUNT)
        .ok_or_else(|| refusal("kernel argument count is invalid"))?;
    let mut remaining = &bytes[4..];
    let executable = take_string(&mut remaining)?;
    if !executable.starts_with(b"/") {
        return Err(refusal("kernel argument executable path is incomplete"));
    }
    while remaining.first() == Some(&0) {
        remaining = &remaining[1..];
    }
    let mut arguments = Vec::with_capacity(count);
    for _ in 0..count {
        arguments.push(OsStr::from_bytes(take_string(&mut remaining)?).to_owned());
    }
    if arguments[0].is_empty() {
        return Err(refusal("kernel argument vector has no executable"));
    }
    Ok(arguments)
}

fn take_string<'a>(remaining: &mut &'a [u8]) -> Result<&'a [u8]> {
    let end = remaining
        .iter()
        .position(|byte| *byte == 0)
        .ok_or_else(|| refusal("kernel argument string is unterminated"))?;
    let value = &remaining[..end];
    *remaining = &remaining[end + 1..];
    Ok(value)
}

pub(super) fn parse_directory(bytes: &[u8], count: i32) -> Result<DirectoryIdentity> {
    // Installed proc_vnodepathinfo: two 1176-byte records, each with a 152-byte vnode_info.
    if count != VNODE_RECORD_BYTES as i32 || bytes.len() != VNODE_RECORD_BYTES {
        return Err(refusal("kernel startup directory response is incomplete"));
    }
    let device = u32::from_ne_bytes(
        bytes[..4]
            .try_into()
            .map_err(|_| refusal("kernel directory device is incomplete"))?,
    ) as u64;
    let inode = u64::from_ne_bytes(
        bytes[8..16]
            .try_into()
            .map_err(|_| refusal("kernel directory inode is incomplete"))?,
    );
    let kind = i32::from_ne_bytes(
        bytes[136..140]
            .try_into()
            .map_err(|_| refusal("kernel directory type is incomplete"))?,
    );
    let path = &bytes[152..1176];
    let end = path
        .iter()
        .position(|byte| *byte == 0)
        .filter(|end| *end > 0)
        .ok_or_else(|| refusal("kernel directory path is incomplete"))?;
    if device == 0 || inode == 0 || kind != 2 || path[0] != b'/' {
        return Err(refusal("kernel startup directory identity is unavailable"));
    }
    Ok(DirectoryIdentity {
        path: PathBuf::from(OsStr::from_bytes(&path[..end])),
        device,
        inode,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{BoundDispatch, SandboxLevel};
    use std::os::unix::ffi::OsStringExt;

    fn spec() -> AppSpec {
        AppSpec {
            build: None,
            run: vec!["/tmp/fixture".into(), "stable".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 3_000,
            sandbox: SandboxLevel::Default,
            a11y: true,
        }
    }

    fn argument_record() -> Vec<u8> {
        let mut bytes = 2_i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/tmp/fixture\0\0\0/tmp/fixture\0stable\0");
        bytes
    }

    fn directory_record(directory: &DirectoryIdentity) -> Vec<u8> {
        let mut bytes = vec![0; VNODE_RECORD_BYTES];
        bytes[..4].copy_from_slice(&(directory.device as u32).to_ne_bytes());
        bytes[8..16].copy_from_slice(&directory.inode.to_ne_bytes());
        bytes[136..140].copy_from_slice(&2_i32.to_ne_bytes());
        let path = directory.path.as_os_str().as_bytes();
        bytes[152..152 + path.len()].copy_from_slice(path);
        bytes
    }

    #[test]
    fn explicit_snapshot_applies_overrides_and_preserves_non_utf8_environment() {
        let dir = tempfile::tempdir().unwrap();
        let mut launch = spec();
        launch.env = vec![("CONTEXT".into(), "override".into())];
        let opaque = OsString::from_vec(vec![0xff, b'x']);
        let context = LaunchContext::from_environment(
            &launch,
            [
                ("CONTEXT".into(), "inherited".into()),
                ("OPAQUE".into(), opaque.clone()),
            ],
            dir.path(),
        )
        .unwrap();
        let mut command = Command::new("/tmp/fixture");
        command.env("UNRETAINED", "later-parent-value");
        context.apply(&mut command);
        let environment: BTreeMap<_, _> = command
            .get_envs()
            .map(|(key, value)| (key.to_owned(), value.unwrap().to_owned()))
            .collect();
        assert_eq!(environment.len(), 2);
        assert_eq!(environment[OsStr::new("CONTEXT")], "override");
        assert_eq!(environment[OsStr::new("OPAQUE")], opaque);
        assert_eq!(command.get_current_dir(), Some(context.cwd()));
    }

    #[test]
    fn capture_refuses_prohibited_injection_without_disclosing_values() {
        let mut launch = spec();
        launch.env = vec![("DYLD_INSERT_LIBRARIES".into(), "synthetic-secret".into())];
        let error = LaunchContext::capture(&launch).err().unwrap();
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(!error.to_string().contains("synthetic-secret"));
        let dir = tempfile::tempdir().unwrap();
        let error = LaunchContext::from_environment(
            &spec(),
            [("GLASS_CLIP_PRIVATE".into(), "synthetic-secret".into())],
            dir.path(),
        )
        .err()
        .unwrap();
        assert!(!error.to_string().contains("synthetic-secret"));
    }

    #[test]
    fn malformed_or_excessive_startup_context_refuses() {
        let dir = tempfile::tempdir().unwrap();
        for (key, value) in [("", "value"), ("BAD=KEY", "value"), ("KEY", "NUL\0value")] {
            assert!(
                LaunchContext::from_environment(&spec(), [(key.into(), value.into())], dir.path())
                    .is_err()
            );
        }
        assert!(
            LaunchContext::from_environment(
                &spec(),
                [("BIG".into(), "x".repeat(MAX_ARGUMENT_BYTES).into())],
                dir.path()
            )
            .is_err()
        );
        let mut launch = spec();
        launch.run.clear();
        assert!(LaunchContext::from_environment(&launch, [], dir.path()).is_err());
    }

    #[test]
    fn arguments_are_counted_separately_from_environment_and_loader_data() {
        let mut bytes = argument_record();
        bytes.extend_from_slice(b"SECRET=synthetic\0ptr_munge=\0\xff\0");
        assert_eq!(
            parse_arguments(&bytes, bytes.len()).unwrap(),
            ["/tmp/fixture", "stable"]
        );
        let mut bytes = 3_i32.to_ne_bytes().to_vec();
        bytes.extend_from_slice(b"/tmp/fixture\0/tmp/fixture\0\xff\0\0loader=data\0");
        let arguments = parse_arguments(&bytes, bytes.len()).unwrap();
        assert_eq!(arguments[1].as_bytes(), b"\xff");
        assert!(arguments[2].is_empty());
    }

    #[test]
    fn truncated_invalid_or_oversized_argument_responses_refuse() {
        let valid = argument_record();
        for length in 0..valid.len() {
            assert!(parse_arguments(&valid, length).is_err(), "length={length}");
        }
        assert!(parse_arguments(&valid, valid.len() + 1).is_err());
        for count in [-1_i32, 0, 4097] {
            let mut bytes = valid.clone();
            bytes[..4].copy_from_slice(&count.to_ne_bytes());
            assert!(parse_arguments(&bytes, bytes.len()).is_err());
        }
        let bytes = vec![0; MAX_ARGUMENT_BYTES + 1];
        assert!(parse_arguments(&bytes, bytes.len()).is_err());
        let mut bytes = valid;
        bytes[4] = b'r';
        assert!(parse_arguments(&bytes, bytes.len()).is_err());
    }

    #[test]
    fn directory_record_requires_complete_path_type_and_identity() {
        let directory = DirectoryIdentity {
            path: "/private/tmp/context".into(),
            device: 1,
            inode: 2,
        };
        let valid = directory_record(&directory);
        assert_eq!(
            parse_directory(&valid, VNODE_RECORD_BYTES as i32).unwrap(),
            directory
        );
        for offset in [0, 8, 136, 152] {
            let mut bytes = valid.clone();
            bytes[offset..offset + 8].fill(0);
            assert!(parse_directory(&bytes, VNODE_RECORD_BYTES as i32).is_err());
        }
        let mut bytes = valid.clone();
        bytes[152..1176].fill(b'x');
        assert!(parse_directory(&bytes, VNODE_RECORD_BYTES as i32).is_err());
        assert!(parse_directory(&valid, 1).is_err());
        assert!(parse_directory(&valid[..1176], VNODE_RECORD_BYTES as i32).is_err());
    }

    #[test]
    fn argument_path_device_and_inode_changes_refuse_independently() {
        let dir = tempfile::tempdir().unwrap();
        let context = LaunchContext::from_environment(&spec(), [], dir.path()).unwrap();
        let arguments: Vec<_> = spec().run.into_iter().map(OsString::from).collect();
        context
            .require_current(&arguments, &context.directory)
            .unwrap();
        let mut changed = arguments.clone();
        changed[1] = "changed".into();
        assert!(
            context
                .require_current(&changed, &context.directory)
                .is_err()
        );
        for directory in [
            DirectoryIdentity {
                path: "/other".into(),
                device: context.directory.device,
                inode: context.directory.inode,
            },
            DirectoryIdentity {
                path: context.directory.path.clone(),
                device: context.directory.device + 1,
                inode: context.directory.inode,
            },
            DirectoryIdentity {
                path: context.directory.path.clone(),
                device: context.directory.device,
                inode: context.directory.inode + 1,
            },
        ] {
            assert!(context.require_current(&arguments, &directory).is_err());
        }
    }

    #[test]
    fn named_directory_replacement_cannot_match_the_old_live_directory() {
        let root = tempfile::tempdir().unwrap();
        let path = root.path().join("cwd");
        std::fs::create_dir(&path).unwrap();
        let context = LaunchContext::from_environment(&spec(), [], &path).unwrap();
        std::fs::rename(&path, root.path().join("old-cwd")).unwrap();
        std::fs::create_dir(&path).unwrap();
        assert!(
            context
                .require_current(&context.arguments, &context.directory)
                .is_err()
        );
        std::fs::remove_dir(&path).unwrap();
        assert!(
            context
                .require_current(&context.arguments, &context.directory)
                .is_err()
        );
    }
}
