//! Resolve the executable inspected for containment in the child's cwd and PATH.

use std::ffi::OsStr;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};

pub(crate) fn resolve(program: &str, cwd: Option<&Path>, path: Option<&OsStr>) -> Option<PathBuf> {
    let token = Path::new(program);
    if program.contains('/') {
        let absolute = if token.is_absolute() {
            token.to_path_buf()
        } else {
            cwd?.join(token)
        };
        return Some(std::fs::canonicalize(&absolute).unwrap_or(absolute));
    }
    std::env::split_paths(path?).find_map(|directory| {
        let directory = if directory.is_absolute() {
            directory
        } else {
            cwd?.join(directory)
        };
        let candidate = directory.join(token);
        let metadata = candidate.metadata().ok()?;
        (metadata.is_file() && metadata.permissions().mode() & 0o111 != 0)
            .then(|| std::fs::canonicalize(&candidate).unwrap_or(candidate))
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn executable(path: &Path) {
        std::fs::write(path, b"test executable path").unwrap();
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o755)).unwrap();
    }

    #[test]
    fn relative_executable_uses_requested_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("build")).unwrap();
        let app = workspace.path().join("build/app");
        executable(&app);
        assert_eq!(
            resolve("./build/app", Some(workspace.path()), None),
            Some(app.canonicalize().unwrap())
        );
    }

    #[test]
    fn absolute_executable_ignores_requested_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        let app = workspace.path().join("app");
        executable(&app);
        assert_eq!(
            resolve(app.to_str().unwrap(), Some(Path::new("/missing")), None),
            Some(app.canonicalize().unwrap())
        );
    }

    #[test]
    fn bare_executable_uses_path_instead_of_same_named_cwd_file() {
        let workspace = tempfile::tempdir().unwrap();
        let binaries = tempfile::tempdir().unwrap();
        executable(&workspace.path().join("app"));
        let app = binaries.path().join("app");
        executable(&app);
        assert_eq!(
            resolve(
                "app",
                Some(workspace.path()),
                Some(binaries.path().as_os_str())
            ),
            Some(app.canonicalize().unwrap())
        );
    }

    #[test]
    fn relative_path_entry_uses_child_cwd() {
        let workspace = tempfile::tempdir().unwrap();
        std::fs::create_dir(workspace.path().join("bin")).unwrap();
        let app = workspace.path().join("bin/app");
        executable(&app);
        assert_eq!(
            resolve("app", Some(workspace.path()), Some(OsStr::new("bin"))),
            Some(app.canonicalize().unwrap())
        );
    }

    #[test]
    fn nonexecutable_path_match_is_skipped() {
        let first = tempfile::tempdir().unwrap();
        let second = tempfile::tempdir().unwrap();
        std::fs::write(first.path().join("app"), b"not executable").unwrap();
        let app = second.path().join("app");
        executable(&app);
        let path = std::env::join_paths([first.path(), second.path()]).unwrap();
        assert_eq!(
            resolve("app", None, Some(&path)),
            Some(app.canonicalize().unwrap())
        );
    }
}
