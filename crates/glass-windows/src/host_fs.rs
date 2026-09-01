use glass_core::GlassError;
use std::os::windows::ffi::OsStrExt;
use std::path::Path;
use windows::Win32::Foundation::{HLOCAL, LocalFree};
use windows::Win32::Security::Authorization::{
    ConvertSecurityDescriptorToStringSecurityDescriptorW,
    ConvertStringSecurityDescriptorToSecurityDescriptorW, SDDL_REVISION_1,
};
use windows::Win32::Security::{
    DACL_SECURITY_INFORMATION, GetFileSecurityW, PROTECTED_DACL_SECURITY_INFORMATION,
    PSECURITY_DESCRIPTOR, SetFileSecurityW,
};
use windows::core::{PCWSTR, PWSTR};

const PRIVATE_DACL: &str = "D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)";

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
