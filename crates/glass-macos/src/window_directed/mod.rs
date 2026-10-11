#![deny(unsafe_code)]
//! Read-only native event and live process/window ownership diagnostics.
//!
//! Current ownership cannot distinguish unobserved window-ID reuse within the same process
//! and connection. These diagnostics do not post input or provide a headless session.

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

/// Kernel facts distinguishing process reuse and re-exec, for the process being inspected.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ProcessIdentity {
    pub pid: i32,
    pub lifetime: u64,
    pub parent_lifetime: u64,
    pub exec_version: u32,
    pub executable_uuid: [u8; 16],
}

/// A current owner tuple; unobserved same-process/connection window-ID reuse can compare equal.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct WindowIdentity {
    pub process: ProcessIdentity,
    pub window_id: u32,
    pub owner_connection: i32,
    pub owner_psn: [u32; 2],
}

#[cfg(any(target_os = "macos", test))]
fn require_same_window(before: WindowIdentity, after: WindowIdentity) -> Result<()> {
    require_same_process(before.process, after.process)?;
    if before.window_id == 0
        || before.owner_connection <= 0
        || before.owner_psn == [0; 2]
        || before != after
    {
        return Err(refusal("bound window identity changed or is unavailable"));
    }
    Ok(())
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
        operation: "native window inspection",
        reason,
    }
    .before_dispatch()
}

#[cfg(test)]
mod tests {
    use super::*;

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

    fn window() -> WindowIdentity {
        WindowIdentity {
            process: process_identity(123, &record()).unwrap(),
            window_id: 47,
            owner_connection: 11,
            owner_psn: [0, 31],
        }
    }

    #[test]
    fn reused_window_number_with_a_changed_process_or_owner_refuses() {
        let original = window();
        require_same_window(original, original).unwrap();
        let mut changed = [original; 5];
        changed[0].process.lifetime += 1;
        changed[1].process.exec_version += 1;
        changed[2].owner_connection += 1;
        changed[3].owner_psn[0] += 1;
        changed[4].owner_psn[1] += 1;
        for current in changed {
            let error = require_same_window(original, current).unwrap_err();
            assert_eq!(
                error.bound_dispatch(),
                Some(glass_core::BoundDispatch::NotDispatched)
            );
        }
    }

    #[test]
    fn a_new_window_number_in_the_same_process_and_connection_refuses() {
        let original = window();
        let mut current = original;
        current.window_id += 1;
        assert!(require_same_window(original, current).is_err());
    }

    #[test]
    fn equal_but_unavailable_window_readings_refuse() {
        let mut unavailable = [window(); 3];
        unavailable[0].window_id = 0;
        unavailable[1].owner_connection = 0;
        unavailable[2].owner_psn = [0; 2];
        for reading in unavailable {
            assert!(require_same_window(reading, reading).is_err());
        }
    }
}
