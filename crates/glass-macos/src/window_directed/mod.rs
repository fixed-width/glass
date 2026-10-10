#![deny(unsafe_code)]
//! Read-only prerequisites for window-directed input in qualification builds.
//!
//! An executable UUID identifies an image, not its trustworthiness. Window ownership does
//! not establish window lifetime or control qualification. This module admits no input.

#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod native;
#[cfg(target_os = "macos")]
pub use native::{Readiness, probe_readiness};
#[cfg(target_os = "macos")]
#[allow(unsafe_code)]
mod target;
#[cfg(target_os = "macos")]
pub use target::{ControlObservation, OwnedTarget, TargetObservation};

#[cfg(any(target_os = "macos", test))]
use glass_core::{GlassError, Result};

/// Kernel facts distinguishing process reuse and re-exec, without qualifying an application.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub lifetime: u64,
    pub parent_lifetime: u64,
    pub exec_version: u32,
    pub executable_uuid: [u8; 16],
}

/// Validate the complete private kernel response before using its identity fields.
#[cfg(any(target_os = "macos", test))]
fn process_identity(pid: i32, bytes: &[u8]) -> Result<ProcessIdentity> {
    if pid <= 0 || bytes.len() != 56 {
        return Err(refusal("kernel process identity response is incomplete"));
    }
    let mut uuid = [0; 16];
    uuid.copy_from_slice(&bytes[..16]);
    let mut lifetime = [0; 8];
    lifetime.copy_from_slice(&bytes[16..24]);
    let mut parent = [0; 8];
    parent.copy_from_slice(&bytes[24..32]);
    let mut version = [0; 4];
    version.copy_from_slice(&bytes[32..36]);
    let identity = ProcessIdentity {
        pid,
        lifetime: u64::from_ne_bytes(lifetime),
        parent_lifetime: u64::from_ne_bytes(parent),
        exec_version: u32::from_ne_bytes(version),
        executable_uuid: uuid,
    };
    if identity.lifetime == 0 || identity.executable_uuid == [0; 16] {
        return Err(refusal("kernel process identity is unavailable"));
    }
    Ok(identity)
}

/// Reject identity changes across an operation; equality alone does not authorize dispatch.
#[cfg(any(target_os = "macos", test))]
fn require_same_process(before: ProcessIdentity, after: ProcessIdentity) -> Result<()> {
    if before != after {
        return Err(refusal(
            "process lifetime or executable changed during the query",
        ));
    }
    Ok(())
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn refusal(reason: &'static str) -> GlassError {
    GlassError::UnsupportedOperation {
        operation: "window-directed input qualification",
        reason,
    }
    .before_dispatch()
}

#[cfg(any(target_os = "macos", test))]
pub(crate) fn validate_environment<'a>(keys: impl IntoIterator<Item = &'a str>) -> Result<()> {
    if keys
        .into_iter()
        .any(|key| key.starts_with("DYLD_") || key.starts_with("GLASS_CLIP_"))
    {
        return Err(refusal(
            "injection environment is incompatible with target inspection",
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn injection_settings_refuse_instead_of_being_silently_removed() {
        for key in [
            "DYLD_INSERT_LIBRARIES",
            "DYLD_LIBRARY_PATH",
            "DYLD_IMAGE_SUFFIX",
            "GLASS_CLIP_PASTEBOARD",
            "GLASS_CLIP_SHIM_DYLIB",
        ] {
            let error = validate_environment(["PATH", key]).unwrap_err();
            assert_eq!(
                error.bound_dispatch(),
                Some(glass_core::BoundDispatch::NotDispatched)
            );
        }
        validate_environment(["PATH", "LANG"]).unwrap();
    }

    fn record() -> [u8; 56] {
        let mut bytes = [0; 56];
        bytes[..16].fill(3);
        bytes[16..24].copy_from_slice(&17_u64.to_ne_bytes());
        bytes[24..32].copy_from_slice(&9_u64.to_ne_bytes());
        bytes[32..36].copy_from_slice(&4_i32.to_ne_bytes());
        bytes
    }

    #[test]
    fn incomplete_or_unavailable_kernel_identity_is_not_a_process_reference() {
        for length in [0, 16, 32, 55, 57, 64] {
            assert!(process_identity(123, &vec![1; length]).is_err());
        }
        for pid in [0, -1] {
            assert!(process_identity(pid, &record()).is_err());
        }
        for range in [0..16, 16..24] {
            let mut bytes = record();
            bytes[range].fill(0);
            assert!(process_identity(123, &bytes).is_err());
        }
        for version in [0_u32, u32::MAX] {
            let mut bytes = record();
            bytes[32..36].copy_from_slice(&version.to_ne_bytes());
            assert_eq!(process_identity(123, &bytes).unwrap().exec_version, version);
        }
    }

    #[test]
    fn process_reuse_reexec_and_image_change_are_distinct_from_an_unchanged_reading() {
        let identity = process_identity(123, &record()).unwrap();
        assert_eq!(identity.lifetime, 17);
        assert_eq!(identity.parent_lifetime, 9);
        assert_eq!(identity.exec_version, 4);
        require_same_process(identity, identity).unwrap();
        let mut alternatives = [identity; 5];
        alternatives[0].pid += 1;
        alternatives[1].lifetime += 1;
        alternatives[2].parent_lifetime += 1;
        alternatives[3].exec_version += 1;
        alternatives[4].executable_uuid[0] ^= 1;
        for changed in alternatives {
            let error = require_same_process(identity, changed).unwrap_err();
            assert_eq!(
                error.bound_dispatch(),
                Some(glass_core::BoundDispatch::NotDispatched)
            );
        }
    }
}
