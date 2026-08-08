//! Bind-path resolution atoms shared by glass's two sandbox backends, bwrap on Linux
//! (`glass-sandbox-linux`) and Seatbelt on macOS (`glass-sandbox-macos`) — unix both, which is what
//! makes this crate unix. Given a program + args + cwd, which absolute host paths does the launch
//! actually touch. No OS-specific containment logic lives here: each backend applies its OWN
//! exposure guard/emit on top.
//!
//! Whether a resolved path can be *run* is a different question, answered by `glass-exec-unix`.
//!
//! `is_absolute()` reads POSIX semantics, so compiled off unix it would quietly mean something else.
#![cfg(unix)]
#![forbid(unsafe_code)]

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
    fn dir_of_returns_parent_for_file_and_self_for_dir() {
        let tmp = tempfile::tempdir().unwrap();
        let sub = tmp.path().join("sub");
        std::fs::create_dir_all(&sub).unwrap();
        let file = sub.join("f");
        std::fs::write(&file, b"").unwrap();
        assert_eq!(dir_of(&file), sub.canonicalize().unwrap());
        assert_eq!(dir_of(&sub), sub.canonicalize().unwrap());
    }

    /// Pins the never-panics/raw-fallback contract: a path that doesn't exist can't be
    /// `canonicalize`d, so `canon` must hand back the raw path unchanged rather than panicking or
    /// erroring.
    #[test]
    fn canon_returns_the_raw_path_when_it_does_not_exist() {
        assert_eq!(
            canon(Path::new("/no/such/glass/path")),
            PathBuf::from("/no/such/glass/path")
        );
    }
}
