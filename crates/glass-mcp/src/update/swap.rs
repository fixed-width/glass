//! Putting the new binary where the old one was.
//!
//! The two platforms need genuinely different mechanisms, so they are two functions with one
//! signature rather than one function with an internal branch.

use std::path::Path;

/// Replace `target` with `temp`.
///
/// Unix: a single `rename` within one directory, which is atomic — there is no instant at which
/// `target` is missing or half-written. That is why the download lands beside the target rather
/// than in `/tmp`: a cross-filesystem move would be a copy, and a copy has a window.
#[cfg(unix)]
pub(crate) fn swap(temp: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o755))
        .map_err(|e| anyhow::anyhow!("could not make {} executable: {e}", temp.display()))?;
    std::fs::rename(temp, target)
        .map_err(|e| anyhow::anyhow!("could not replace {}: {e}", target.display()))?;
    Ok(())
}

/// Windows cannot do this atomically: the file being replaced is the running image of the process
/// doing the replacing, and Windows holds a lock against writing or deleting it. It does permit
/// *renaming* it, so the running image is moved aside first and the new binary takes its place.
///
/// If the second rename fails the first is undone, so a failed update leaves the original binary
/// exactly where it was.
#[cfg(windows)]
pub(crate) fn swap(temp: &Path, target: &Path) -> anyhow::Result<()> {
    let mut displaced = target.as_os_str().to_os_string();
    displaced.push(format!(".old-{}", std::process::id()));
    let displaced = std::path::PathBuf::from(displaced);

    std::fs::rename(target, &displaced)
        .map_err(|e| anyhow::anyhow!("could not move the running binary aside: {e}"))?;
    if let Err(e) = std::fs::rename(temp, target) {
        // Put it back. The update fails, but the install is intact.
        let _ = std::fs::rename(&displaced, target);
        return Err(anyhow::anyhow!(
            "could not put the new binary in place: {e}"
        ));
    }
    Ok(())
}

/// Delete any binaries a previous update moved aside.
///
/// Windows only, and best-effort: the displaced file cannot be deleted while it is still some
/// process's running image, which is exactly the case during the update that created it. The next
/// run is when it goes away, so failure here is expected and silent.
#[cfg(windows)]
pub(crate) fn sweep_old(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        let name = entry.file_name();
        if name.to_string_lossy().starts_with("glass-mcp.exe.old-") {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// No displaced binaries exist on Unix — the rename is atomic and leaves nothing behind.
#[cfg(unix)]
pub(crate) fn sweep_old(_dir: &Path) {}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_target_ends_up_holding_the_new_bytes() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("glass-mcp");
        let temp = dir.path().join(".glass-mcp.update-1");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&temp, b"new").unwrap();

        swap(&temp, &target).expect("swap");

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        assert!(!temp.exists(), "the temp file is consumed by the swap");
    }

    /// `rename` vs `copy`: the target must end up being the temp file's inode, not its own with
    /// new contents. This is the only assertion here that distinguishes an atomic rename from a
    /// copy-then-delete — `the_target_ends_up_holding_the_new_bytes` passes identically for both.
    ///
    /// It also pins the behavior the post-update notice describes: because the old inode survives
    /// unlinked, an already-running `serve --http` goes on serving the OLD build until restarted.
    #[cfg(unix)]
    #[test]
    fn the_swap_moves_the_temp_files_inode_into_place() {
        use std::os::unix::fs::MetadataExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("glass-mcp");
        let temp = dir.path().join(".glass-mcp.update-1");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&temp, b"new").unwrap();
        let old_ino = std::fs::metadata(&target).unwrap().ino();
        let temp_ino = std::fs::metadata(&temp).unwrap().ino();

        swap(&temp, &target).expect("swap");

        let new_ino = std::fs::metadata(&target).unwrap().ino();
        assert_eq!(
            new_ino, temp_ino,
            "the temp file's inode must land at the target path"
        );
        assert_ne!(
            new_ino, old_ino,
            "a copy would have kept the target's original inode"
        );
    }

    #[cfg(unix)]
    #[test]
    fn the_swapped_binary_is_executable() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("glass-mcp");
        let temp = dir.path().join(".glass-mcp.update-1");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&temp, b"new").unwrap();
        // Downloaded files land 0644; without the chmod the swapped-in binary cannot be run.
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o644)).unwrap();

        swap(&temp, &target).expect("swap");

        let mode = std::fs::metadata(&target).unwrap().permissions().mode();
        assert_eq!(mode & 0o777, 0o755, "got {:o}", mode & 0o777);
    }

    #[cfg(windows)]
    #[test]
    fn the_displaced_binary_is_left_behind_for_the_sweep() {
        let dir = tempfile::tempdir().expect("tempdir");
        let target = dir.path().join("glass-mcp.exe");
        let temp = dir.path().join(".glass-mcp.update-1");
        std::fs::write(&target, b"old").unwrap();
        std::fs::write(&temp, b"new").unwrap();

        swap(&temp, &target).expect("swap");

        assert_eq!(std::fs::read(&target).unwrap(), b"new");
        let leftovers: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("glass-mcp.exe.old-"))
            .collect();
        assert_eq!(
            leftovers.len(),
            1,
            "expected one displaced binary, got {leftovers:?}"
        );

        sweep_old(dir.path());
        let after: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with("glass-mcp.exe.old-"))
            .collect();
        assert!(
            after.is_empty(),
            "sweep_old must delete what it can: {after:?}"
        );
    }
}
