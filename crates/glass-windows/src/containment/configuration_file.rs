use std::collections::BTreeMap;
use std::ffi::OsString;
use std::io::Read;
use std::os::windows::ffi::OsStringExt;
use std::path::{Path, PathBuf};

use glass_core::{GlassError, Result};

#[derive(Clone, Default)]
pub(super) struct Section {
    pub present: bool,
    pub values: BTreeMap<String, Vec<String>>,
}

fn failure(detail: impl std::fmt::Display) -> GlassError {
    GlassError::SandboxUnavailable(format!(
        "Cannot verify Sandboxie configuration: {detail}. Ask your administrator to review configuration access."
    ))
}

fn configured_path() -> Result<Option<PathBuf>> {
    use windows::Win32::System::Registry::{HKEY_LOCAL_MACHINE, RRF_RT_REG_SZ, RegGetValueW};
    use windows::core::w;
    let mut bytes = 0;
    // SAFETY: the first call reads only the configured string's size.
    let status = unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Services\\SbieDrv"),
            w!("IniPath"),
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut bytes),
        )
    };
    if status.0 == 2 || status.0 == 3 {
        return Ok(None);
    }
    status.ok().map_err(failure)?;
    if bytes == 0 || bytes > 65536 || !bytes.is_multiple_of(2) {
        return Err(failure("invalid configured file path"));
    }
    let mut buffer = vec![0u16; bytes as usize / 2];
    // SAFETY: the destination is aligned, writable and has the queried capacity.
    unsafe {
        RegGetValueW(
            HKEY_LOCAL_MACHINE,
            w!("SYSTEM\\CurrentControlSet\\Services\\SbieDrv"),
            w!("IniPath"),
            RRF_RT_REG_SZ,
            None,
            Some(buffer.as_mut_ptr().cast()),
            Some(&mut bytes),
        )
    }
    .ok()
    .map_err(failure)?;
    let end = buffer
        .iter()
        .position(|unit| *unit == 0)
        .unwrap_or(buffer.len());
    if end == 0 {
        return Ok(None);
    }
    let value = OsString::from_wide(&buffer[..end]);
    let path = Path::new(&value);
    let path = if let Ok(suffix) = path.strip_prefix(r"\??\") {
        let mut dos = OsString::from(r"\\?\");
        dos.push(suffix.as_os_str());
        PathBuf::from(dos)
    } else {
        path.to_path_buf()
    };
    if !path.is_absolute() {
        return Err(failure("configured file path is not absolute"));
    }
    Ok(Some(path))
}

fn locate(provider: &str) -> Result<PathBuf> {
    if let Some(path) = configured_path()? {
        return Ok(path);
    }
    let mut buffer = vec![0u16; 32768];
    // SAFETY: the native call receives a live writable buffer of the supplied capacity.
    let length = unsafe {
        windows::Win32::System::SystemInformation::GetWindowsDirectoryW(Some(&mut buffer))
    } as usize;
    if length == 0 || length >= buffer.len() {
        return Err(failure("cannot resolve the Windows directory"));
    }
    let windows = PathBuf::from(OsString::from_wide(&buffer[..length])).join("Sandboxie.ini");
    match windows.try_exists() {
        Ok(true) => Ok(windows),
        Ok(false) => Ok(Path::new(provider).join("Sandboxie.ini")),
        Err(error) => Err(failure(error)),
    }
}

fn decode(bytes: &[u8]) -> Result<String> {
    if bytes.starts_with(&[0xff, 0xfe]) || bytes.starts_with(&[0xfe, 0xff]) {
        if !bytes.len().is_multiple_of(2) {
            return Err(failure("incomplete UTF-16 configuration"));
        }
        let little = bytes[0] == 0xff;
        let units = bytes[2..]
            .as_chunks::<2>()
            .0
            .iter()
            .map(|pair| {
                if little {
                    u16::from_le_bytes(*pair)
                } else {
                    u16::from_be_bytes(*pair)
                }
            })
            .collect::<Vec<_>>();
        String::from_utf16(&units).map_err(failure)
    } else {
        String::from_utf8(
            bytes
                .strip_prefix(&[0xef, 0xbb, 0xbf])
                .unwrap_or(bytes)
                .to_vec(),
        )
        .map_err(failure)
    }
}

fn parse(text: &str, name: &str) -> Result<Section> {
    let mut result = Section::default();
    let mut selected = false;
    for line in text.lines().map(str::trim) {
        if let Some(header) = line.strip_prefix('[') {
            let (header, tail) = header
                .split_once(']')
                .ok_or_else(|| failure("incomplete section header"))?;
            let tail = tail.trim();
            if header.trim().is_empty()
                || (!tail.is_empty() && !tail.starts_with(';') && !tail.starts_with('#'))
            {
                return Err(failure("invalid section header"));
            }
            selected = header.trim().eq_ignore_ascii_case(name);
            result.present |= selected;
        } else if selected
            && !line.starts_with(';')
            && !line.starts_with('#')
            && let Some((key, value)) = line.split_once('=')
        {
            result
                .values
                .entry(key.trim().to_ascii_lowercase())
                .or_default()
                .push(value.trim().into());
        }
    }
    Ok(result)
}

pub(super) fn read(provider: &str, name: &str) -> Result<Section> {
    const LIMIT: u64 = 16 * 1024 * 1024;
    let path = locate(provider)?;
    let mut bytes = Vec::new();
    std::fs::File::open(&path)
        .map_err(failure)?
        .take(LIMIT + 1)
        .read_to_end(&mut bytes)
        .map_err(failure)?;
    if bytes.len() as u64 > LIMIT {
        return Err(failure("configuration exceeds the readback limit"));
    }
    parse(&decode(&bytes)?, name)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn readback_selects_only_the_owned_section_and_keeps_repeated_values() {
        let text = "[GlobalSettings]\nEditPassword=not-exported\n[glass_own]\nEnabled=y\nTemplate=one\nTemplate=two\nClosedFilePath=C:\\用户\\state\n[Other]\nEnabled=n\n";
        let owned = parse(text, "Glass_Own").unwrap();
        assert!(owned.present);
        assert_eq!(owned.values["template"], ["one", "two"]);
        assert_eq!(owned.values["closedfilepath"], ["C:\\用户\\state"]);
        assert!(!owned.values.contains_key("editpassword"));
    }

    #[test]
    fn empty_section_is_distinct_from_confirmed_absence() {
        assert!(parse("[glass_own]\n", "glass_own").unwrap().present);
        assert!(!parse("[Other]\nEnabled=y\n", "glass_own").unwrap().present);
    }

    #[test]
    fn malformed_header_cannot_confirm_section_absence() {
        assert!(parse("[glass_own", "glass_own").is_err());
        assert!(parse("[glass_own] invalid\n", "glass_own").is_err());
    }

    #[test]
    fn utf16_readback_keeps_unicode_paths_without_lossy_conversion() {
        let text = "[glass_own]\nClosedFilePath=C:\\用户\\state\n";
        let mut bytes = vec![0xff, 0xfe];
        bytes.extend(text.encode_utf16().flat_map(u16::to_le_bytes));
        assert_eq!(decode(&bytes).unwrap(), text);
        assert!(decode(&[0xff, 0xfe, 0]).is_err());
    }
}
