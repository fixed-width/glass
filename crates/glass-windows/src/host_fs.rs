use glass_core::GlassError;
use std::ffi::OsStr;
use std::fs::{File, OpenOptions};
use std::os::windows::ffi::{OsStrExt, OsStringExt};
use std::os::windows::fs::{MetadataExt, OpenOptionsExt};
use std::os::windows::io::AsRawHandle;
use std::os::windows::io::FromRawHandle;
use std::path::Component;
use std::path::Path;
use windows::Wdk::Foundation::OBJECT_ATTRIBUTES;
use windows::Wdk::Storage::FileSystem::{
    FILE_NON_DIRECTORY_FILE, FILE_OPEN, FILE_OPEN_REPARSE_POINT, FILE_SYNCHRONOUS_IO_NONALERT,
    NtCreateFile,
};
use windows::Win32::Foundation::HANDLE;
use windows::Win32::Foundation::{HLOCAL, LocalFree, OBJ_CASE_INSENSITIVE, UNICODE_STRING};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetFileSecurityW, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SetFileSecurityW,
};
use windows::Win32::Storage::FileSystem::{
    BY_HANDLE_FILE_INFORMATION, DELETE, FILE_ATTRIBUTE_NORMAL, FILE_ATTRIBUTE_REPARSE_POINT,
    FILE_DISPOSITION_INFO, FILE_FLAG_BACKUP_SEMANTICS, FILE_FLAG_OPEN_REPARSE_POINT,
    FILE_ID_BOTH_DIR_INFO, FILE_READ_ATTRIBUTES, FILE_READ_DATA, FILE_SHARE_DELETE,
    FILE_SHARE_READ, FILE_SHARE_WRITE, FileDispositionInfo, FileIdBothDirectoryRestartInfo,
    GetFileInformationByHandle, GetFileInformationByHandleEx, SYNCHRONIZE,
    SetFileInformationByHandle,
};
use windows::Win32::System::IO::IO_STATUS_BLOCK;
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

/// Verifies that a retained regular-file handle still names the no-reparse object at `path`.
pub fn file_matches_path_no_reparse(file: &File, path: &Path) -> Result<bool, HostFsError> {
    let current = OpenOptions::new()
        .read(true)
        .share_mode((FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE).0)
        .custom_flags(FILE_FLAG_OPEN_REPARSE_POINT.0)
        .open(path)
        .map_err(|_| HostFsError::Open)?;
    let metadata = current.metadata().map_err(|_| HostFsError::Open)?;
    if !metadata.is_file() || is_reparse(&metadata) {
        return Err(HostFsError::Integrity);
    }
    Ok(file_identity(file)? == file_identity(&current)?)
}

/// Opens one generated single-component regular-file child relative to a retained directory handle.
///
/// The pathname is consulted only to reject an already-substituted directory before the native
/// relative open. Once that check completes, directory renames cannot redirect the child lookup.
pub fn open_file_beneath(
    directory: &File,
    directory_path: &Path,
    filename: &OsStr,
) -> Result<File, HostFsError> {
    open_file_beneath_with_hook(directory, directory_path, filename, |_| Ok(()))
}

/// Opens one single-component directory child relative to a retained directory handle.
pub fn open_directory_beneath(directory: &File, filename: &OsStr) -> Result<File, HostFsError> {
    open_relative(directory, filename, Some(true))
}

/// Opens one single-component regular-file child relative to a retained directory handle.
pub fn open_file_child(directory: &File, filename: &OsStr) -> Result<File, HostFsError> {
    open_relative(directory, filename, Some(false))
}

/// Opens a child entry itself, including a reparse object, relative to a retained directory.
pub fn open_entry_child(directory: &File, filename: &OsStr) -> Result<File, HostFsError> {
    open_relative(directory, filename, None)
}

/// Enumerates names from a retained directory handle without consulting its pathname.
pub fn directory_entry_names(directory: &File) -> Result<Vec<std::ffi::OsString>, HostFsError> {
    let mut buffer = vec![0_u8; 1024 * 1024];
    // SAFETY: `directory` is borrowed and live. `buffer` is writable for its full reported size.
    unsafe {
        GetFileInformationByHandleEx(
            HANDLE(directory.as_raw_handle()),
            FileIdBothDirectoryRestartInfo,
            buffer.as_mut_ptr().cast(),
            u32::try_from(buffer.len()).map_err(|_| HostFsError::Integrity)?,
        )
        .map_err(|_| HostFsError::Open)?;
    }
    parse_directory_names(&buffer)
}

/// Marks the exact retained filesystem object for deletion, without reopening a pathname.
pub fn remove_by_handle(file: &File) -> Result<(), HostFsError> {
    let disposition = FILE_DISPOSITION_INFO { DeleteFile: true };
    // SAFETY: `file` is live and borrowed. `disposition` has the exact structure and byte size
    // required by `FileDispositionInfoEx` and remains initialized for the call.
    unsafe {
        SetFileInformationByHandle(
            HANDLE(file.as_raw_handle()),
            FileDispositionInfo,
            (&raw const disposition).cast(),
            u32::try_from(std::mem::size_of_val(&disposition))
                .map_err(|_| HostFsError::Integrity)?,
        )
        .map_err(|_| HostFsError::Open)
    }
}

fn parse_directory_names(bytes: &[u8]) -> Result<Vec<std::ffi::OsString>, HostFsError> {
    let mut names = Vec::new();
    let mut offset = 0_usize;
    loop {
        if bytes.len().saturating_sub(offset) < std::mem::size_of::<FILE_ID_BOTH_DIR_INFO>() {
            return Err(HostFsError::Integrity);
        }
        // SAFETY: The bounds check above covers the fixed record. The API aligns every record for
        // this structure, and `offset` advances only by its validated `NextEntryOffset`.
        let record = unsafe { &*bytes.as_ptr().add(offset).cast::<FILE_ID_BOTH_DIR_INFO>() };
        let name_units = usize::try_from(record.FileNameLength)
            .ok()
            .and_then(|length| length.checked_div(std::mem::size_of::<u16>()))
            .ok_or(HostFsError::Integrity)?;
        let fixed = std::mem::offset_of!(FILE_ID_BOTH_DIR_INFO, FileName);
        let end = offset
            .checked_add(fixed)
            .and_then(|start| {
                name_units
                    .checked_mul(2)
                    .and_then(|length| start.checked_add(length))
            })
            .filter(|end| *end <= bytes.len())
            .ok_or(HostFsError::Integrity)?;
        // SAFETY: The validated range contains `name_units` UTF-16 units and the record alignment
        // guarantees the flexible filename member's u16 alignment.
        let name = unsafe {
            std::slice::from_raw_parts(bytes.as_ptr().add(offset + fixed).cast::<u16>(), name_units)
        };
        let name = std::ffi::OsString::from_wide(name);
        if name != "." && name != ".." {
            names.push(name);
        }
        if record.NextEntryOffset == 0 {
            let _ = end;
            break;
        }
        offset = offset
            .checked_add(
                usize::try_from(record.NextEntryOffset).map_err(|_| HostFsError::Integrity)?,
            )
            .filter(|next| *next > offset && *next < bytes.len())
            .ok_or(HostFsError::Integrity)?;
    }
    Ok(names)
}

#[derive(Clone, Copy)]
enum ChildOpenStage {
    BeforeOpen,
    AfterOpen,
}

fn open_file_beneath_with_hook(
    directory: &File,
    directory_path: &Path,
    filename: &OsStr,
    mut hook: impl FnMut(ChildOpenStage) -> Result<(), HostFsError>,
) -> Result<File, HostFsError> {
    let mut components = Path::new(filename).components();
    if !matches!(components.next(), Some(Component::Normal(_)))
        || components.next().is_some()
        || !directory_matches_path(directory, directory_path)?
    {
        return Err(HostFsError::Integrity);
    }
    hook(ChildOpenStage::BeforeOpen)?;
    let file = open_relative_file(directory, filename)?;
    hook(ChildOpenStage::AfterOpen)?;
    let metadata = file.metadata().map_err(|_| HostFsError::Open)?;
    if !metadata.is_file() || is_reparse(&metadata) {
        return Err(HostFsError::Integrity);
    }
    Ok(file)
}

fn open_relative_file(directory: &File, filename: &OsStr) -> Result<File, HostFsError> {
    open_relative(directory, filename, Some(false))
}

fn open_relative(
    directory: &File,
    filename: &OsStr,
    directory_child: Option<bool>,
) -> Result<File, HostFsError> {
    let mut components = Path::new(filename).components();
    if !matches!(components.next(), Some(Component::Normal(_))) || components.next().is_some() {
        return Err(HostFsError::Integrity);
    }
    let mut name = filename.encode_wide().collect::<Vec<_>>();
    if name.contains(&0) {
        return Err(HostFsError::Integrity);
    }
    let byte_len = name
        .len()
        .checked_mul(std::mem::size_of::<u16>())
        .and_then(|length| u16::try_from(length).ok())
        .ok_or(HostFsError::Integrity)?;
    let unicode_name = UNICODE_STRING {
        Length: byte_len,
        MaximumLength: byte_len,
        Buffer: PWSTR(name.as_mut_ptr()),
    };
    let attributes = OBJECT_ATTRIBUTES {
        Length: u32::try_from(std::mem::size_of::<OBJECT_ATTRIBUTES>())
            .map_err(|_| HostFsError::Open)?,
        RootDirectory: HANDLE(directory.as_raw_handle()),
        ObjectName: &unicode_name,
        Attributes: OBJ_CASE_INSENSITIVE,
        SecurityDescriptor: std::ptr::null(),
        SecurityQualityOfService: std::ptr::null(),
    };
    let mut status_block = IO_STATUS_BLOCK::default();
    let mut handle = HANDLE::default();
    // SAFETY: `directory` supplies a live directory handle and remains borrowed for the call.
    // `unicode_name` references the initialized UTF-16 `name` buffer, whose byte length fits u16;
    // native counted strings do not require a terminator. `attributes` has the required size,
    // points to `unicode_name`, uses that directory as `RootDirectory`, and has null optional
    // security pointers. `handle` and `status_block` are writable outputs. The access mask requests
    // only reads, attributes, and synchronous completion; sharing includes read, write, and delete.
    // `FILE_OPEN` cannot create, `FILE_NON_DIRECTORY_FILE` rejects directories, and
    // `FILE_OPEN_REPARSE_POINT` opens a reparse object itself so the safe wrapper can reject it.
    // On success, ownership of the returned handle transfers exactly once to `File`; on failure,
    // NT does not return an owned handle and the initialized default value is not closed.
    let status = unsafe {
        NtCreateFile(
            &mut handle,
            FILE_READ_DATA | FILE_READ_ATTRIBUTES | SYNCHRONIZE | DELETE,
            &attributes,
            &mut status_block,
            None,
            FILE_ATTRIBUTE_NORMAL,
            FILE_SHARE_READ | FILE_SHARE_WRITE | FILE_SHARE_DELETE,
            FILE_OPEN,
            directory_child.map_or_else(
                windows::Wdk::Storage::FileSystem::NTCREATEFILE_CREATE_OPTIONS::default,
                |is_directory| {
                    if is_directory {
                        windows::Wdk::Storage::FileSystem::FILE_DIRECTORY_FILE
                    } else {
                        FILE_NON_DIRECTORY_FILE
                    }
                },
            ) | FILE_SYNCHRONOUS_IO_NONALERT
                | FILE_OPEN_REPARSE_POINT,
            None,
            0,
        )
    };
    if !status.is_ok() {
        return Err(HostFsError::Open);
    }
    // SAFETY: A successful NtCreateFile call returned one owned file handle in `handle`.
    // `File` assumes that ownership and closes the handle exactly once when dropped.
    let file = unsafe { File::from_raw_handle(handle.0) };
    let metadata = file.metadata().map_err(|_| HostFsError::Open)?;
    if directory_child
        .is_some_and(|is_directory| is_reparse(&metadata) || metadata.is_dir() != is_directory)
    {
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

/// Compares the stable filesystem identities of two retained handles.
pub fn same_file_object(first: &File, second: &File) -> Result<bool, HostFsError> {
    Ok(file_identity(first)? == file_identity(second)?)
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::io::Read;

    #[test]
    fn child_open_never_follows_a_swap_open_restore_directory_race() {
        let parent = tempfile::tempdir().unwrap();
        let directory_path = parent.path().join("owned");
        let detached_path = parent.path().join("detached");
        let external_path = parent.path().join("external");
        std::fs::create_dir(&directory_path).unwrap();
        std::fs::create_dir(&external_path).unwrap();
        let filename = OsStr::new("artifact.txt");
        std::fs::write(directory_path.join(filename), "retained").unwrap();
        std::fs::write(external_path.join(filename), "external").unwrap();
        let directory = open_directory_no_reparse(&directory_path).unwrap();

        let mut file =
            open_file_beneath_with_hook(&directory, &directory_path, filename, |stage| {
                match stage {
                    ChildOpenStage::BeforeOpen => {
                        std::fs::rename(&directory_path, &detached_path)
                            .map_err(|_| HostFsError::Open)?;
                        std::fs::rename(&external_path, &directory_path)
                            .map_err(|_| HostFsError::Open)?;
                    }
                    ChildOpenStage::AfterOpen => {
                        std::fs::rename(&directory_path, &external_path)
                            .map_err(|_| HostFsError::Open)?;
                        std::fs::rename(&detached_path, &directory_path)
                            .map_err(|_| HostFsError::Open)?;
                    }
                }
                Ok(())
            })
            .unwrap();
        let mut text = String::new();
        file.read_to_string(&mut text).unwrap();

        assert_eq!(text, "retained");
        assert_eq!(
            std::fs::read_to_string(external_path.join(filename)).unwrap(),
            "external"
        );
    }

    #[test]
    fn enumeration_and_removal_stay_on_the_retained_directory_after_swap() {
        let parent = tempfile::tempdir().unwrap();
        let owned = parent.path().join("owned");
        let detached = parent.path().join("detached");
        let replacement = parent.path().join("replacement");
        std::fs::create_dir(&owned).unwrap();
        std::fs::create_dir(&replacement).unwrap();
        std::fs::write(owned.join("artifact.txt"), "owned").unwrap();
        let sentinel = replacement.join("sentinel.txt");
        std::fs::write(&sentinel, "outside").unwrap();
        let handle = open_directory_no_reparse(&owned).unwrap();
        std::fs::rename(&owned, &detached).unwrap();
        std::fs::rename(&replacement, &owned).unwrap();

        let names = directory_entry_names(&handle).unwrap();
        assert_eq!(names, [std::ffi::OsString::from("artifact.txt")]);
        let child = open_file_child(&handle, &names[0]).unwrap();
        remove_by_handle(&child).unwrap();
        drop(child);

        assert_eq!(
            std::fs::read_to_string(owned.join("sentinel.txt")).unwrap(),
            "outside"
        );
        assert!(!detached.join("artifact.txt").exists());
    }
}
