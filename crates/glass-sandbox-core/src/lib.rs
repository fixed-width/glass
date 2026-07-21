//! Pure, portable launch-target *resolution* atoms shared by glass's per-OS sandbox crates
//! (`glass-sandbox-linux`, `glass-sandbox-macos`). No OS-specific containment logic lives here —
//! only "given a program + args + cwd, what absolute host paths does the launch actually touch,
//! resolved the way the child is exec'd." Each backend applies its OWN exposure guard/emit on top.
//! std-only; builds on every target so a `--workspace` build never breaks.
#![forbid(unsafe_code)]

use std::ffi::OsStr;
use std::path::{Path, PathBuf};

/// Resolve a token to an absolute host path: an absolute token as-is, a relative one against
/// `cwd` (`execvp`/shell semantics). `None` for a relative token when `cwd` is unknown — the
/// caller then skips it rather than resolving against a wrong root like `/`.
pub fn abs_token(tok: &Path, cwd: Option<&Path>) -> Option<PathBuf> {
    if tok.is_absolute() {
        Some(tok.to_path_buf())
    } else {
        cwd.map(|c| c.join(tok))
    }
}

/// The first `$PATH` entry holding an executable regular file named `program`, resolved the way
/// `execvp` resolves a bare command name. `None` when `$PATH` is unset or nothing matches.
pub fn resolve_on_path(program: &OsStr) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    resolve_on_path_in(program, &path)
}

/// [`resolve_on_path`] against an explicit `$PATH` value — the testable seam (no global env).
pub fn resolve_on_path_in(program: &OsStr, path: &OsStr) -> Option<PathBuf> {
    std::env::split_paths(path)
        .map(|dir| dir.join(program))
        .find(|cand| is_executable_file(cand))
}

/// Whether `p` is (or resolves through symlinks to) a regular file that is executable — `execvp`'s
/// "is this runnable" test. The execute-bit check is a Unix concept (mode `& 0o111`); on non-unix
/// hosts (where glass has no Seatbelt/bwrap sandbox) it degrades to "is a regular file" so the
/// crate still compiles as a `--workspace` member.
#[cfg(unix)]
pub fn is_executable_file(p: &Path) -> bool {
    use std::os::unix::fs::PermissionsExt;
    std::fs::metadata(p)
        .map(|m| m.is_file() && m.permissions().mode() & 0o111 != 0)
        .unwrap_or(false)
}

#[cfg(not(unix))]
pub fn is_executable_file(p: &Path) -> bool {
    std::fs::metadata(p).map(|m| m.is_file()).unwrap_or(false)
}

/// Best-effort path canonicalization that never panics on a nonexistent path: the resolved path,
/// or the raw path unchanged if `canonicalize` fails (e.g. the path doesn't exist yet).
pub fn canon(p: &Path) -> PathBuf {
    p.canonicalize().unwrap_or_else(|_| p.to_path_buf())
}

/// The canonicalized directory to expose for a path: the path itself when it is a directory, else
/// its parent. Canonicalized so a caller's shadowed-root guard sees a `..`-free path.
pub fn dir_of(p: &Path) -> PathBuf {
    if p.is_dir() {
        canon(p)
    } else {
        canon(p.parent().unwrap_or(p))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::ffi::OsStr;
    use std::os::unix::fs::PermissionsExt;
    use std::path::{Path, PathBuf};

    #[test]
    fn abs_token_absolute_passes_through_relative_needs_cwd() {
        assert_eq!(
            abs_token(Path::new("/a/b"), None),
            Some(PathBuf::from("/a/b"))
        );
        assert_eq!(
            abs_token(Path::new("x/y"), Some(Path::new("/c"))),
            Some(PathBuf::from("/c/x/y"))
        );
        assert_eq!(abs_token(Path::new("x/y"), None), None);
    }

    #[test]
    fn resolve_on_path_in_finds_first_executable_match() {
        let dir = tempfile::tempdir().unwrap();
        let exe = dir.path().join("mytool");
        std::fs::write(&exe, b"").unwrap();
        std::fs::set_permissions(&exe, std::fs::Permissions::from_mode(0o755)).unwrap();
        let path = std::env::join_paths([dir.path()]).unwrap();
        assert_eq!(resolve_on_path_in(OsStr::new("mytool"), &path), Some(exe));
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
    fn dir_of_returns_parent_for_file_and_self_for_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("f");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(dir_of(&file), sub.canonicalize().unwrap());
        assert_eq!(dir_of(&sub), sub.canonicalize().unwrap());
    }
}
