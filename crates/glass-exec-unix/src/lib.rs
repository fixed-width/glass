//! Unix exec resolution: given a program name or path, what will actually run. `execvp`'s own
//! tests — the execute bit, and `$PATH` lookup for a bare name — so a caller can tell a missing
//! binary from an installed one it cannot spawn.
//!
//! Used by every glass component that resolves an external tool: the sandbox backends
//! (`glass-sandbox-linux`, `glass-sandbox-macos`), the X11 and Wayland backends, and the AT-SPI bus
//! launcher.
//!
//! Which host paths a launch touches is a different question, answered by `glass-sandbox-unix`.
//!
//! Everything here reads POSIX semantics — the execute bit, `execvp` lookup — so compiled off unix
//! the mode check would quietly mean something else.
#![cfg(unix)]
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// The first `$PATH` entry holding an executable regular file named `program`, resolved the way
/// `execvp` resolves a bare command name. `None` when `$PATH` is unset or nothing matches.
pub fn resolve_on_path(program: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_on_path_in(program, &path)
}

/// [`resolve_on_path`] against an explicit `$PATH` value — the testable seam (no global env).
/// Public so the backends' own resolution paths can be tested the same way, without a test
/// mutating the process environment (unsound: `set_var` races any concurrent reader).
pub fn resolve_on_path_in(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|cand| is_executable_file(cand))
}

/// Whether `p` is (or resolves through symlinks to) a regular file that is executable — `execvp`'s
/// "is this runnable" test.
pub fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;

    #[test]
    fn resolve_on_path_in_finds_first_executable_match() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("mytool");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_on_path_in(OsStr::new("mytool"), &path), Some(exe));
    }

    /// Pins the documented "first `$PATH` entry wins" contract (`execvp` semantics): with two
    /// directories on `$PATH`, each holding an executable of the SAME name, the match from the
    /// FIRST directory must be returned, not merely any match.
    #[test]
    fn resolve_on_path_in_returns_the_first_match() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        for dir in [&first, &second] {
            let exe = dir.path().join("mytool");
            std::fs::write(&exe, b"").unwrap();
            std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        }
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(
            resolve_on_path_in(OsStr::new("mytool"), &path),
            Some(first.path().join("mytool")),
            "must return the FIRST $PATH entry's match, not merely any match"
        );
    }

    #[test]
    fn resolve_on_path_in_skips_non_executable_and_missing() {
        let dir = tempfile::tempdir().unwrap();
        let plain = dir.path().join("mytool");
        std::fs::write(&plain, b"").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_on_path_in(OsStr::new("mytool"), &path), None);
        assert_eq!(resolve_on_path_in(OsStr::new("absent"), &path), None);
    }

    #[test]
    fn is_executable_file_true_for_exec_false_for_dir_and_plain() {
        let dir = tempfile::tempdir().unwrap();

        let exe = dir.path().join("exe");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(is_executable_file(&exe), "an exec-bit file must be true");

        let plain = dir.path().join("plain");
        std::fs::write(&plain, b"").unwrap();
        std::fs::set_permissions(&plain, std::fs::Permissions::from_mode(0o644)).unwrap();
        assert!(
            !is_executable_file(&plain),
            "a non-exec plain file must be false"
        );

        let subdir = dir.path().join("subdir");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::set_permissions(&subdir, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(
            !is_executable_file(&subdir),
            "a directory (even an 'executable'-mode one) must be false"
        );
    }
}
