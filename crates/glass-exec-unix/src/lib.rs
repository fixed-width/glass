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

/// What resolving a configured tool found.
///
/// The `NotExecutable` case is the reason this is not an `Option<PathBuf>`: a caller that only
/// learns "no" cannot tell a missing binary from an installed one whose execute bit is off, and
/// sends the user to reinstall a package that is already there.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Resolved {
    /// A runnable binary.
    Found(PathBuf),
    /// A regular file is there but carries no execute bit.
    NotExecutable(PathBuf),
    /// Nothing by that name.
    Absent,
}

impl Resolved {
    /// The runnable path, or `None` for either failure.
    pub fn found(&self) -> Option<&Path> {
        match self {
            Self::Found(p) => Some(p),
            Self::NotExecutable(_) | Self::Absent => None,
        }
    }

    /// Whether a runnable binary was found.
    pub fn is_found(&self) -> bool {
        self.found().is_some()
    }
}

/// Resolve a configured tool the way the child will be exec'd: a token containing `/` is an
/// explicit path taken as given, anything else is a bare name looked up in `path` (a `PATH`-style
/// separated list).
///
/// A search returns the first *executable* match and walks past non-executable ones, as `execvp`
/// does — resolving to one of those would run a different binary than the user's shell. When the
/// walk turns up no runnable match, the first non-executable it passed is reported rather than
/// [`Resolved::Absent`], because that is the fact the user can act on.
pub fn resolve_bin(bin: &str, path: Option<&OsStr>) -> Resolved {
    if bin.contains('/') {
        return classify(PathBuf::from(bin));
    }
    let Some(path) = path else {
        return Resolved::Absent;
    };
    let mut first_non_executable = None;
    for cand in std::env::split_paths(path).map(|dir| dir.join(bin)) {
        match classify(cand) {
            Resolved::Found(p) => return Resolved::Found(p),
            Resolved::NotExecutable(p) => {
                first_non_executable.get_or_insert(p);
            }
            Resolved::Absent => {}
        }
    }
    first_non_executable.map_or(Resolved::Absent, Resolved::NotExecutable)
}

/// One candidate path's state. A directory is [`Resolved::Absent`]: it is not a launch target
/// however its mode bits read.
fn classify(p: PathBuf) -> Resolved {
    if is_executable_file(&p) {
        Resolved::Found(p)
    } else if p.is_file() {
        Resolved::NotExecutable(p)
    } else {
        Resolved::Absent
    }
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

    /// A directory holding `name` at `mode`. 0o755 is runnable, 0o644 is the case
    /// `is_file()` used to accept.
    fn dir_with(name: &str, mode: u32) -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("tempdir");
        let bin = dir.path().join(name);
        std::fs::write(&bin, b"").expect("write");
        std::fs::set_permissions(&bin, std::fs::Permissions::from_mode(mode)).expect("chmod");
        dir
    }

    #[test]
    fn an_explicit_path_to_an_executable_is_found() {
        let dir = dir_with("Xvfb", 0o755);
        let bin = dir.path().join("Xvfb");
        assert_eq!(
            resolve_bin(bin.to_str().expect("utf-8 temp path"), None),
            Resolved::Found(bin)
        );
    }

    /// The defect in glass#374: this input used to resolve as success.
    #[test]
    fn an_explicit_path_to_a_non_executable_file_is_not_executable() {
        let dir = dir_with("Xvfb", 0o644);
        let bin = dir.path().join("Xvfb");
        assert_eq!(
            resolve_bin(bin.to_str().expect("utf-8 temp path"), None),
            Resolved::NotExecutable(bin)
        );
    }

    #[test]
    fn an_explicit_path_to_nothing_is_absent() {
        assert_eq!(resolve_bin("/nonexistent/Xvfb", None), Resolved::Absent);
    }

    /// A directory is not a launch target, however its mode bits read.
    #[test]
    fn an_explicit_path_to_a_directory_is_absent() {
        let dir = tempfile::tempdir().expect("tempdir");
        assert_eq!(
            resolve_bin(dir.path().to_str().expect("utf-8 temp path"), None),
            Resolved::Absent
        );
    }

    #[test]
    fn a_bare_name_resolves_against_the_search_list() {
        let dir = dir_with("Xvfb", 0o755);
        let path = std::env::join_paths([dir.path()]).expect("join paths");
        assert_eq!(
            resolve_bin("Xvfb", Some(&path)),
            Resolved::Found(dir.path().join("Xvfb"))
        );
    }

    /// `execvp` semantics: a non-executable match does not stop the search. Departing from this
    /// would make glass run a different binary than the user's shell does.
    #[test]
    fn the_search_walks_past_a_non_executable_match_to_a_later_executable_one() {
        let first = dir_with("Xvfb", 0o644);
        let second = dir_with("Xvfb", 0o755);
        let path = std::env::join_paths([first.path(), second.path()]).expect("join paths");
        assert_eq!(
            resolve_bin("Xvfb", Some(&path)),
            Resolved::Found(second.path().join("Xvfb")),
            "a non-executable match must not shadow a runnable one later on the list"
        );
    }

    /// When the walk finds nothing runnable, the non-executable it passed is the actionable
    /// fact — reporting `Absent` would send the user to install what is already installed.
    #[test]
    fn a_search_finding_only_a_non_executable_match_reports_it_rather_than_absent() {
        let dir = dir_with("Xvfb", 0o644);
        let path = std::env::join_paths([dir.path()]).expect("join paths");
        assert_eq!(
            resolve_bin("Xvfb", Some(&path)),
            Resolved::NotExecutable(dir.path().join("Xvfb"))
        );
    }

    /// The FIRST non-executable, not merely any — it is the one the user's `PATH` order points at.
    #[test]
    fn a_search_reports_the_first_non_executable_match_it_passed() {
        let first = dir_with("Xvfb", 0o644);
        let second = dir_with("Xvfb", 0o600);
        let path = std::env::join_paths([first.path(), second.path()]).expect("join paths");
        assert_eq!(
            resolve_bin("Xvfb", Some(&path)),
            Resolved::NotExecutable(first.path().join("Xvfb"))
        );
    }

    #[test]
    fn a_bare_name_with_no_search_list_is_absent() {
        assert_eq!(resolve_bin("Xvfb", None), Resolved::Absent);
    }

    #[test]
    fn a_bare_name_absent_from_the_search_list_is_absent() {
        let dir = dir_with("Xvfb", 0o755);
        let path = std::env::join_paths([dir.path()]).expect("join paths");
        assert_eq!(resolve_bin("Xorg", Some(&path)), Resolved::Absent);
    }

    #[test]
    fn a_found_result_exposes_its_path() {
        let dir = dir_with("Xvfb", 0o755);
        let bin = dir.path().join("Xvfb");
        assert_eq!(Resolved::Found(bin.clone()).found(), Some(bin.as_path()));
    }

    #[test]
    fn a_non_executable_result_does_not_read_as_found() {
        let dir = dir_with("Xvfb", 0o644);
        assert!(!Resolved::NotExecutable(dir.path().join("Xvfb")).is_found());
    }

    #[test]
    fn an_absent_result_exposes_no_path_and_does_not_read_as_found() {
        assert_eq!(Resolved::Absent.found(), None);
        assert!(!Resolved::Absent.is_found());
    }
}
