//! Scoped local account creation; credentials never cross a command-line boundary.

use std::ffi::CString;

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct DesktopAccess {
    pub authorized: bool,
    pub member: bool,
    pub group_uuid: Option<String>,
}

fn desktop_access_using(
    name: &str,
    uuid: &str,
    uid: u32,
    change: Option<(&str, bool)>,
) -> Result<DesktopAccess, String> {
    unsafe extern "C" {
        fn glass_desktop_access(
            name: *const std::ffi::c_char,
            uuid: *const std::ffi::c_char,
            uid: u32,
            action: i32,
            expected_group: *const std::ffi::c_char,
            group: *mut std::ffi::c_char,
            member: *mut i32,
            authorized: *mut i32,
        ) -> i32;
    }
    let name = CString::new(name).map_err(|_| "invalid account name")?;
    let uuid = CString::new(uuid).map_err(|_| "invalid account identity")?;
    let group = change
        .map(|(uuid, _)| CString::new(uuid))
        .transpose()
        .map_err(|_| "invalid access group identity")?;
    let action = change.map_or(0, |(_, member)| if member { 1 } else { 2 });
    let mut output = [0i8; 37];
    let (mut member, mut authorized) = (-1, -1);
    // SAFETY: all C strings and fixed-size output buffers stay live for the synchronous call.
    let code = unsafe {
        glass_desktop_access(
            name.as_ptr(),
            uuid.as_ptr(),
            uid,
            action,
            group
                .as_ref()
                .map_or(std::ptr::null(), |group| group.as_ptr()),
            output.as_mut_ptr(),
            &mut member,
            &mut authorized,
        )
    };
    if code != 0 {
        return Err(format!(
            "macOS Screen Sharing account access failed: {code}"
        ));
    }
    if ![0, 1].contains(&member) || ![0, 1].contains(&authorized) {
        return Err("invalid Screen Sharing access result".into());
    }
    // SAFETY: the bridge writes at most 36 UUID characters and a terminating NUL.
    let group = unsafe { std::ffi::CStr::from_ptr(output.as_ptr()) }
        .to_str()
        .map_err(|_| "invalid access group UUID")?;
    Ok(DesktopAccess {
        authorized: authorized == 1,
        member: member == 1,
        group_uuid: (!group.is_empty()).then(|| group.to_owned()),
    })
}

pub fn desktop_access(name: &str, uuid: &str, uid: u32) -> Result<DesktopAccess, String> {
    desktop_access_using(name, uuid, uid, None)
}

pub fn set_desktop_access(
    name: &str,
    uuid: &str,
    uid: u32,
    group_uuid: &str,
    member: bool,
) -> Result<DesktopAccess, String> {
    desktop_access_using(name, uuid, uid, Some((group_uuid, member)))
}

#[derive(Default)]
#[repr(C)]
struct NativeError {
    domain: u32,
    code: i64,
}

unsafe extern "C" {
    fn glass_account_name_unused(name: *const std::ffi::c_char) -> i32;
    fn glass_account_available_uid(minimum: u32, output: *mut u32) -> i32;
    fn glass_account_verify_identity(
        name: *const std::ffi::c_char,
        uuid: *const std::ffi::c_char,
        uid: u32,
    ) -> i32;
    fn glass_account_prepare(
        name: *const std::ffi::c_char,
        uuid: *const std::ffi::c_char,
        uid: u32,
        password: *const u8,
        length: usize,
        record_only: bool,
        initial: bool,
        diagnostic: *mut NativeError,
    ) -> i32;
}

pub fn require_unused_name(name: &str) -> Result<(), String> {
    let name = CString::new(name).map_err(|_| "invalid account name")?;
    // SAFETY: this is a read-only query; name stays live for the call.
    let code = unsafe { glass_account_name_unused(name.as_ptr()) };
    if code == 2 {
        return Err("that account name already exists; use Set up for the existing account or choose a different new name".into());
    }
    status(code)
}

pub fn verify_identity(name: &str, uuid: &str, uid: u32) -> Result<(), String> {
    let name = CString::new(name).map_err(|_| "invalid account name")?;
    let uuid = CString::new(uuid).map_err(|_| "invalid account UUID")?;
    // SAFETY: the helper only queries the directory; both strings stay live until it returns.
    status(unsafe { glass_account_verify_identity(name.as_ptr(), uuid.as_ptr(), uid) })
}

fn status(code: i32) -> Result<(), String> {
    match code {
        0 => Ok(()),
        1 => Err("account creation requires administrator authorization and valid account details".into()),
        2 => Err("the account identity or UID is already in use or has changed; existing accounts were retained".into()),
        4 => Err("macOS could not initialize the account password; the account was retained without enrollment".into()),
        5 => Err("this resumed account already has credentials; supply its original password or select it as an existing account".into()),
        7 => Err("the account could not be verified as a standard account; nothing was enrolled".into()),
        8 => Err("the new account password could not be verified; account retained for recovery".into()),
        10 => Err("a new record's UID became ambiguous and its withdrawal could not be verified; no account was enrolled".into()),
        11 => Err("the account UID changed or became ambiguous after password verification; account retained without enrollment".into()),
        12 => Err("the selected UID has live processes or its inventory is unavailable; no account was created".into()),
        _ => Err(format!("local account preparation did not complete (status {code}); completed account data was retained")),
    }
}

fn preparation_status(code: i32, diagnostic: NativeError) -> Result<(), String> {
    status(code).map_err(|message| {
        let domain = match diagnostic.domain {
            0 => return message,
            1 => "Open Directory",
            2 => "POSIX",
            3 => "Cocoa",
            4 => "OSStatus",
            _ => "macOS",
        };
        format!("{message} ({domain} code {})", diagnostic.code)
    })
}

pub fn available_uid(minimum: u32) -> Result<u32, String> {
    let mut uid = 0;
    // SAFETY: the helper only queries directory records and writes the scalar output.
    status(unsafe { glass_account_available_uid(minimum, &mut uid) })?;
    Ok(uid)
}

fn prepare(
    name: &str,
    uuid: &str,
    uid: u32,
    password: &[u8],
    record_only: bool,
    initial: bool,
) -> Result<(), String> {
    let name = CString::new(name).map_err(|_| "invalid account name")?;
    let uuid = CString::new(uuid).map_err(|_| "invalid account UUID")?;
    if password.contains(&0) {
        return Err("account password contains a NUL character".into());
    }
    let mut diagnostic = NativeError::default();
    // SAFETY: the strings, password slice and writable scalar diagnostic remain live for the call.
    let code = unsafe {
        glass_account_prepare(
            name.as_ptr(),
            uuid.as_ptr(),
            uid,
            password.as_ptr(),
            password.len(),
            record_only,
            initial,
            &mut diagnostic,
        )
    };
    preparation_status(code, diagnostic)
}

pub fn create_record(name: &str, uuid: &str, uid: u32, allow_creation: bool) -> Result<(), String> {
    prepare(name, uuid, uid, &[], true, allow_creation)
}

pub fn set_password(
    name: &str,
    uuid: &str,
    uid: u32,
    password: &[u8],
    allow_initial: bool,
) -> Result<(), String> {
    prepare(name, uuid, uid, password, false, allow_initial)
}

#[cfg(test)]
mod tests {
    #[test]
    fn initial_password_failures_preserve_native_errors_without_assuming_password_policy() {
        let message = super::preparation_status(
            4,
            super::NativeError {
                domain: 2,
                code: 13,
            },
        )
        .unwrap_err();
        assert!(message.ends_with("(POSIX code 13)"));
        assert!(message.contains("retained without enrollment"));
        assert!(!message.contains("policy"));
    }

    #[test]
    fn native_error_codes_are_reported_without_native_descriptions() {
        let error = super::preparation_status(
            3,
            super::NativeError {
                domain: 1,
                code: 4001,
            },
        )
        .unwrap_err();
        assert!(error.ends_with("(Open Directory code 4001)"));
        assert!(
            super::preparation_status(
                0,
                super::NativeError {
                    domain: 1,
                    code: 4001
                }
            )
            .is_ok()
        );
        assert_eq!(
            super::preparation_status(3, super::NativeError::default()),
            super::status(3)
        );
    }

    #[test]
    fn numeric_process_guard_sees_the_current_user_and_an_unused_uid() {
        unsafe extern "C" {
            fn glass_private_uid_empty(uid: u32) -> i32;
        }
        // SAFETY: this helper only reads process metadata for a scalar UID.
        assert_eq!(
            unsafe { glass_private_uid_empty(rustix::process::getuid().as_raw()) },
            0
        );
        let mut minimum = 501;
        loop {
            let uid = super::available_uid(minimum).unwrap();
            // SAFETY: read-only query of an unallocated numeric UID, with no process mutation.
            if unsafe { glass_private_uid_empty(uid) } == 1 {
                break;
            }
            minimum = uid + 1;
        }
    }

    #[test]
    fn native_preparation_refuses_unprivileged_calls_before_directory_mutation() {
        if !rustix::process::geteuid().is_root() {
            assert!(
                super::set_password(
                    "glass_refusal",
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    59999,
                    b"test-only",
                    true
                )
                .is_err()
            );
            assert!(
                super::create_record(
                    "glass_refusal",
                    "aaaaaaaa-bbbb-4ccc-8ddd-eeeeeeeeeeee",
                    59999,
                    true
                )
                .is_err()
            );
        }
    }

    #[test]
    fn available_uid_is_a_bounded_read_only_directory_query() {
        let uid = super::available_uid(501).unwrap();
        assert!((501..60000).contains(&uid));
        assert!(super::available_uid(0).is_err());
    }
}
