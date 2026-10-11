//! Bounded executable-file identity checks; matching bytes do not establish code trust.

use std::fs::{self, File, Metadata};
use std::io::Read;
use std::os::unix::ffi::OsStrExt;
use std::os::unix::fs::MetadataExt;
use std::path::{Path, PathBuf};

use glass_core::{BoundDispatch, Deadline, GlassError, Result};
use rustix::fs::{Mode, OFlags};
use sha2::{Digest, Sha256};

use super::refusal;

const MAX_BYTES: u64 = 64 * 1024 * 1024;

/// The original executable file, not a qualified application or loaded-library inventory.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ExecutableIdentity {
    pub path: PathBuf,
    pub sha256: [u8; 32],
    pub image_uuid: [u8; 16],
    file: FileStamp,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct FileStamp {
    device: u64,
    inode: u64,
    length: u64,
    modified: (i64, i64),
    changed: (i64, i64),
}

impl FileStamp {
    fn read(metadata: Metadata) -> Result<Self> {
        if !metadata.is_file() || metadata.len() > MAX_BYTES {
            return Err(refusal("executable is not a bounded regular file"));
        }
        Ok(Self {
            device: metadata.dev(),
            inode: metadata.ino(),
            length: metadata.len(),
            modified: (metadata.mtime(), metadata.mtime_nsec()),
            changed: (metadata.ctime(), metadata.ctime_nsec()),
        })
    }
}

impl ExecutableIdentity {
    pub(super) fn read(path: &Path, deadline: Deadline) -> Result<Self> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("executable observation"));
        }
        if !path.is_absolute() {
            return Err(refusal("executable inspection requires an absolute path"));
        }
        let path = fs::canonicalize(path).map_err(|_| refusal("executable path is unavailable"))?;
        let before = FileStamp::read(
            fs::metadata(&path).map_err(|_| refusal("executable metadata is unavailable"))?,
        )?;
        let file = File::from(
            rustix::fs::open(
                &path,
                OFlags::RDONLY | OFlags::CLOEXEC | OFlags::NONBLOCK | OFlags::NOFOLLOW,
                Mode::empty(),
            )
            .map_err(|_| refusal("executable file is unavailable"))?,
        );
        let opened = FileStamp::read(
            file.metadata()
                .map_err(|_| refusal("executable metadata is unavailable"))?,
        )?;
        if opened != before {
            return Err(refusal("executable changed while opening"));
        }
        let mut bounded = file.take(MAX_BYTES + 1);
        let mut bytes = Vec::new();
        let mut buffer = [0; 32 * 1024];
        loop {
            check_deadline(deadline)?;
            let count = bounded
                .read(&mut buffer)
                .map_err(|_| refusal("executable read failed"))?;
            if count == 0 {
                break;
            }
            bytes.extend_from_slice(&buffer[..count]);
        }
        let after = FileStamp::read(
            bounded
                .get_ref()
                .metadata()
                .map_err(|_| refusal("executable metadata is unavailable"))?,
        )?;
        let named = FileStamp::read(
            fs::metadata(&path).map_err(|_| refusal("executable path disappeared"))?,
        )?;
        if after != before || named != before || bytes.len() as u64 != before.length {
            return Err(refusal("executable changed during observation"));
        }
        let identity = Self {
            path,
            sha256: Sha256::digest(&bytes).into(),
            image_uuid: image_uuid(&bytes)?,
            file: before,
        };
        check_deadline(deadline)?;
        Ok(identity)
    }

    pub(super) fn revalidate(&self, path: &Path, uuid: [u8; 16], deadline: Deadline) -> Result<()> {
        let current = Self::read(path, deadline)?;
        if current != *self || uuid != self.image_uuid {
            return Err(refusal("bound executable identity changed"));
        }
        Ok(())
    }
}

fn check_deadline(deadline: Deadline) -> Result<()> {
    if deadline.has_passed() {
        let mut error = GlassError::caller_deadline_elapsed("executable observation");
        // Only file reads ran; compound callers preserve earlier dispatch with after_dispatch.
        if let GlassError::Bounded { dispatch, .. } = &mut error {
            *dispatch = BoundDispatch::NotDispatched;
        }
        Err(error)
    } else {
        Ok(())
    }
}

fn image_uuid(bytes: &[u8]) -> Result<[u8; 16]> {
    let malformed = || refusal("executable requires a complete thin 64-bit Mach-O UUID");
    let word = |offset| -> Result<u32> {
        let bytes = bytes.get(offset..offset + 4).ok_or_else(malformed)?;
        Ok(u32::from_le_bytes(
            bytes.try_into().map_err(|_| malformed())?,
        ))
    };
    if word(0)? != 0xfeed_facf || word(12)? != 2 || bytes.len() < 32 {
        return Err(malformed());
    }
    let count = word(16)? as usize;
    let size = word(20)? as usize;
    if count == 0 || count > 256 || size > 1024 * 1024 || size > bytes.len() - 32 {
        return Err(malformed());
    }
    let end = 32 + size;
    let mut cursor = 32;
    let mut uuid = None;
    for _ in 0..count {
        if cursor + 8 > end {
            return Err(malformed());
        }
        let command = word(cursor)?;
        let size = word(cursor + 4)? as usize;
        if size < 8 || !size.is_multiple_of(8) || size > end - cursor {
            return Err(malformed());
        }
        if command == 0x1b {
            if size != 24 || uuid.is_some() {
                return Err(malformed());
            }
            uuid = Some(
                bytes[cursor + 8..cursor + 24]
                    .try_into()
                    .map_err(|_| malformed())?,
            );
        }
        cursor += size;
    }
    if cursor != end {
        return Err(malformed());
    }
    uuid.filter(|uuid| *uuid != [0; 16]).ok_or_else(malformed)
}

pub(super) fn kernel_path(bytes: &[u8], count: i32) -> Result<PathBuf> {
    let count =
        usize::try_from(count).map_err(|_| refusal("kernel executable path is unavailable"))?;
    if count == 0
        || count >= bytes.len()
        || bytes[count] != 0
        || bytes[..count].contains(&0)
        || bytes[0] != b'/'
    {
        return Err(refusal("kernel executable path is incomplete"));
    }
    Ok(PathBuf::from(std::ffi::OsStr::from_bytes(&bytes[..count])))
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{BoundKind, Whose};

    fn image() -> Vec<u8> {
        let mut bytes = vec![0; 56];
        bytes[0..4].copy_from_slice(&0xfeed_facf_u32.to_le_bytes());
        bytes[12..16].copy_from_slice(&2_u32.to_le_bytes());
        bytes[16..20].copy_from_slice(&1_u32.to_le_bytes());
        bytes[20..24].copy_from_slice(&24_u32.to_le_bytes());
        bytes[32..36].copy_from_slice(&0x1b_u32.to_le_bytes());
        bytes[36..40].copy_from_slice(&24_u32.to_le_bytes());
        bytes[40..56].fill(7);
        bytes
    }

    #[test]
    fn incomplete_ambiguous_or_unavailable_image_records_refuse() {
        let original = image();
        assert_eq!(image_uuid(&original).unwrap(), [7; 16]);
        for length in 0..original.len() {
            assert!(image_uuid(&original[..length]).is_err(), "length {length}");
        }
        for (offset, value) in [
            (0, 0xcafe_babe),
            (12, 6),
            (16, 0),
            (16, 257),
            (20, 25),
            (36, 0),
            (36, 16),
            (36, u32::MAX),
        ] {
            let mut bytes = original.clone();
            bytes[offset..offset + 4].copy_from_slice(&value.to_le_bytes());
            assert!(image_uuid(&bytes).is_err());
        }
        let mut absent = original.clone();
        absent[40..56].fill(0);
        assert!(image_uuid(&absent).is_err());
        let mut duplicate = original.clone();
        duplicate.extend_from_slice(&original[32..]);
        duplicate[16..20].copy_from_slice(&2_u32.to_le_bytes());
        duplicate[20..24].copy_from_slice(&48_u32.to_le_bytes());
        assert!(image_uuid(&duplicate).is_err());
    }

    #[test]
    fn equal_uuid_does_not_hide_changed_bytes_or_a_replaced_file() {
        let directory = tempfile::tempdir().unwrap();
        let path = directory.path().join("fixture");
        fs::write(&path, image()).unwrap();
        let identity = ExecutableIdentity::read(&path, Deadline::from_millis(1_000)).unwrap();
        identity
            .revalidate(&path, [7; 16], Deadline::from_millis(1_000))
            .unwrap();
        let mut wrong_digest = identity.clone();
        wrong_digest.sha256[0] ^= 1;
        assert!(
            wrong_digest
                .revalidate(&path, [7; 16], Deadline::from_millis(1_000))
                .is_err()
        );
        assert!(
            identity
                .revalidate(&path, [8; 16], Deadline::from_millis(1_000))
                .is_err()
        );
        let mut changed = image();
        changed.push(1);
        fs::write(&path, changed).unwrap();
        assert!(
            identity
                .revalidate(&path, [7; 16], Deadline::from_millis(1_000))
                .is_err()
        );
        let replacement = directory.path().join("replacement");
        fs::write(&replacement, image()).unwrap();
        fs::rename(&replacement, &path).unwrap();
        assert!(
            identity
                .revalidate(&path, [7; 16], Deadline::from_millis(1_000))
                .is_err()
        );
        fs::remove_file(&path).unwrap();
        assert!(
            identity
                .revalidate(&path, [7; 16], Deadline::from_millis(1_000))
                .is_err()
        );
    }

    #[test]
    fn nonregular_oversized_relative_and_expired_reads_refuse() {
        let directory = tempfile::tempdir().unwrap();
        assert!(ExecutableIdentity::read(directory.path(), Deadline::from_millis(1_000)).is_err());
        let path = directory.path().join("oversized");
        File::create(&path).unwrap().set_len(MAX_BYTES + 1).unwrap();
        assert!(ExecutableIdentity::read(&path, Deadline::from_millis(1_000)).is_err());
        assert!(
            ExecutableIdentity::read(Path::new("fixture"), Deadline::from_millis(1_000)).is_err()
        );
        let error = ExecutableIdentity::read(&path, Deadline::from_millis(0)).unwrap_err();
        assert_eq!(error.bound(), Some(BoundKind::NotStarted));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn file_read_expiry_is_pre_dispatch_and_preserves_prior_external_work() {
        let error = check_deadline(Deadline::from_millis(0)).unwrap_err();
        assert_eq!(error.bound(), Some(BoundKind::TimedOut));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert_eq!(
            error.after_dispatch().bound_dispatch(),
            Some(BoundDispatch::MayHaveDispatched)
        );
    }

    #[test]
    fn kernel_paths_require_a_complete_absolute_nul_terminated_response() {
        let bytes = b"/tmp/fixture\0";
        let count = bytes.len() as i32 - 1;
        assert_eq!(
            kernel_path(bytes, count).unwrap(),
            Path::new("/tmp/fixture")
        );
        for count in [-1, 0, 1, bytes.len() as i32, bytes.len() as i32 + 1] {
            assert!(kernel_path(bytes, count).is_err());
        }
        assert!(kernel_path(b"relative\0", 8).is_err());
        assert!(kernel_path(b"/tmp/\0fixture\0", count + 1).is_err());
    }
}
