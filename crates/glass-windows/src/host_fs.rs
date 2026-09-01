use glass_core::GlassError;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::OsStrExt;
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::path::Path;
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetFileSecurityW, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SetFileSecurityW,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, FILE_ATTRIBUTE_REPARSE_POINT, FILE_FLAG_BACKUP_SEMANTICS,
    FILE_FLAG_OPEN_REPARSE_POINT, FILE_SHARE_DELETE, FILE_SHARE_READ, FILE_SHARE_WRITE,
    GetFileInformationByHandle,
};
use windows::core::{PCWSTR, PWSTR};

const PRIVATE_DACL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum HostFsError {
    Open,
    Integrity,
}

/// Opens a directory itself, rather than a reparse target, and retains rename-compatible sharing.
pub fn open_directory_no_reparse(path: &Path) -> Result<File, HostFsError> {
    let file = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags((FILE_FLAG_BACKUP_SEMANTICS | FILE_FLAG_OPEN_REPARSE_POINT).0)
        .open(path)
        .map_err(|_| HostFsError::Open)?;
    let metadata = file.metadata().map_err(|_| HostFsError::Open)?;
    if !metadata.is_dir() || is_reparse(&metadata) {
        return Err(HostFsError::Integrity);
    }
    Ok(file)
}

/// Opens one named regular-file child and verifies the retained directory still names the same object.
pub fn open_file_beneath(
    directory: &File,
    directory_path: &Path,
    filename: &OsStr,
) -> Result<File, HostFsError> {
    if Path::new(filename).components().count() != 1
        || !directory_matches_path(directory, directory_path)?
    {
        return Err(HostFsError::Integrity);
    }
    let path = directory_path.join(filename);
    let file = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(&path)
        .map_err(|_| HostFsError::Open)?;
    let metadata = file.metadata().map_err(|_| HostFsError::Open)?;
    if !metadata.is_file()
        || is_reparse(&metadata)
        || !directory_matches_path(directory, directory_path)?
    {
        return Err(HostFsError::Integrity);
    }
    let resolved_file = path.canonicalize().map_err(|_| HostFsError::Open)?;
    let resolved_directory = directory_path
        .canonicalize()
        .map_err(|_| HostFsError::Open)?;
    if resolved_file.parent() != Some(resolved_directory.as_path()) {
        return Err(HostFsError::Integrity);
    }
    Ok(file)
}

fn directory_matches_path(directory: &File, path: &Path) -> Result<bool, HostFsError> {
    let current = open_directory_no_reparse(path).map_err(|_| HostFsError::Integrity)?;
    Ok(file_identity(directory)? == file_identity(&current)?)
}

fn file_identity(file: &File) -> Result<(u32, u64), HostFsError> {
    let mut information = BY_HANDLE_FILE_INFORMATION::default();
    // SAFETY: `file` owns a live kernel handle for the duration of the call and
    // `information` points to writable storage of the required type.
    unsafe {
        GetFileInformationByHandle(HANDLE(file.as_raw_handle()), &mut information)
            .map_err(|_| HostFsError::Open)?;
    }
    let index =
        (u64::from(information.nFileIndexHigh) << 32) | u64::from(information.nFileIndexLow);
    Ok((information.dwVolumeSerialNumber, index))
}

fn is_reparse(metadata: &std::fs::Metadata) -> bool {
    metadata.file_attributes() & FILE_ATTRIBUTE_REPARSE_POINT.0 != 0
}

/// Replaces a filesystem object's inherited permissions with full access for its owner and SYSTEM.
pub fn restrict_path_to_current_user(path: &Path) -> glass_core::Result<()> {
    let path = wide(path);
    let sddl = wide_text(PRIVATE_DACL);
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    // SAFETY: Both UTF-16 inputs are NUL-terminated, the output pointer is valid for the call,
    // and the returned allocation is released with LocalFree below.
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            SDDL_REVISION_1,
            &mut descriptor,
            None,
        )
        .map_err(|error| backend_error("convert private DACL", error))?;
    }
    // SAFETY: `descriptor` is the valid allocation returned above and `path` is NUL-terminated.
    let applied = unsafe {
        SetFileSecurityW(
            PCWSTR(path.as_ptr()),
            DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION,
            descriptor,
        )
    };
    // SAFETY: The security descriptor was allocated by LocalAlloc through the conversion API.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    applied
        .ok()
        .map_err(|error| backend_error("apply private DACL", error))
}

/// Reports whether a filesystem object has the protected owner-and-SYSTEM DACL used by Glass.
pub fn path_has_private_dacl(path: &Path) -> glass_core::Result<bool> {
    let path = wide(path);
    let information = DACL_SECURITY_INFORMATION | PROTECTED_DACL_SECURITY_INFORMATION;
    let mut needed = 0_u32;
    // SAFETY: `path` is NUL-terminated. A null descriptor with length zero is the documented size query.
    unsafe {
        let _ = GetFileSecurityW(PCWSTR(path.as_ptr()), information.0, None, 0, &mut needed);
    }
    if needed == 0 {
        return Err(GlassError::Backend("query private DACL size failed".into()));
    }
    let mut bytes = vec![0_u8; needed as usize];
    let descriptor = PSECURITY_DESCRIPTOR(bytes.as_mut_ptr().cast());
    // SAFETY: The byte buffer has the size returned by GetFileSecurityW and remains alive for conversion.
    unsafe {
        GetFileSecurityW(
            PCWSTR(path.as_ptr()),
            information.0,
            Some(descriptor),
            needed,
            &mut needed,
        )
        .ok()
        .map_err(|error| backend_error("read private DACL", error))?;
    }
    let mut rendered = PWSTR::null();
    let mut rendered_len = 0_u32;
    // SAFETY: `descriptor` points to the initialized security descriptor buffer and output pointers are valid.
    unsafe {
        ConvertSecurityDescriptorToStringSecurityDescriptorW(
            descriptor,
            SDDL_REVISION_1,
            information,
            &mut rendered,
            Some(&mut rendered_len),
        )
        .map_err(|error| backend_error("render private DACL", error))?;
    }
    // SAFETY: The conversion API returned `rendered_len` initialized UTF-16 code units.
    let value = unsafe {
        String::from_utf16_lossy(std::slice::from_raw_parts(
            rendered.0,
            rendered_len as usize,
        ))
    };
    // SAFETY: The rendered SDDL was allocated by LocalAlloc through the conversion API.
    unsafe {
        let _ = LocalFree(Some(HLOCAL(rendered.0.cast())));
    }
    Ok(value == PRIVATE_DACL)
}

fn wide(path: &Path) -> Vec<u16> {
    path.as_os_str().encode_wide().chain(Some(0)).collect()
}

fn wide_text(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(Some(0)).collect()
}

fn backend_error(operation: &str, error: windows::core::Error) -> GlassError {
    GlassError::Backend(format!("{operation} failed: {error}"))
}
