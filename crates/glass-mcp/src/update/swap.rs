//! Putting the new binary where the old one was.
//!
//! The two platforms need genuinely different mechanisms, so they are two functions with one
//! signature rather than one function with an internal branch.

use std::path::Path;

use anyhow::Context as _;

/// Replace `target` with `temp`.
///
/// Unix: a single `rename` within one directory, which is atomic — there is no instant at which
/// `target` is missing or half-written. That is why the download lands beside the target rather
/// than in `/tmp`: a cross-filesystem move would be a copy, and a copy has a window.
#[cfg(unix)]
pub(crate) fn swap(temp: &Path, target: &Path) -> anyhow::Result<()> {
    use std::os::unix::fs::PermissionsExt;
    std::fs::set_permissions(temp, std::fs::Permissions::from_mode(0o755))
        .with_context(|| format!("could not make {} executable", temp.display()))?;
    std::fs::rename(temp, target)
        .with_context(|| format!("could not replace {}", target.display()))?;
    Ok(())
}

/// Windows cannot do this atomically: the file being replaced is the running image of the process
/// doing the replacing, and Windows holds a lock against writing or deleting it. It does permit
/// *renaming* it, so the running image is moved aside first and the new binary takes its place.
///
/// If the second rename fails the first is undone, so a failed update leaves the original binary
/// exactly where it was — unless the restore itself fails, in which case the error says so rather
/// than claiming the install is intact.
#[cfg(windows)]
pub(crate) fn swap(temp: &Path, target: &Path) -> anyhow::Result<()> {
    let displaced = displaced_path(target, std::process::id());

    std::fs::rename(target, &displaced)
        .with_context(|| format!("could not move {} aside", target.display()))?;
    if let Err(e) = std::fs::rename(temp, target) {
        // Put it back — and check that it worked. If the restore ALSO fails, the target path now
        // holds nothing at all, and reporting "the install is intact" would be a lie at the worst
        // possible moment. Say what happened and name where the old binary actually is.
        if let Err(restore) = std::fs::rename(&displaced, target) {
            return Err(anyhow::anyhow!(
                "could not put the new binary in place ({e}), and could not restore the old one \
                 ({restore}) — {} is now MISSING. The previous binary is at {}; move it back.",
                target.display(),
                displaced.display()
            ));
        }
        return Err(e).with_context(|| {
            format!(
                "could not put the new binary in place: {}",
                target.display()
            )
        });
    }
    Ok(())
}

/// Where `swap` moves a displaced binary. Paired with [`is_displaced`] so the name `swap` writes
/// and the name `sweep_old` looks for cannot drift apart — renaming the binary would otherwise
/// break the sweep with no compiler or test signal.
#[cfg(windows)]
fn displaced_path(target: &Path, pid: u32) -> std::path::PathBuf {
    let mut p = target.as_os_str().to_os_string();
    p.push(format!(".old-{pid}"));
    std::path::PathBuf::from(p)
}

/// Is `entry` a binary `swap` displaced — `<exe name>.old-<pid>`?
///
/// Pure, and gated to `windows` OR `test` rather than to `windows` alone: the Windows sweep cannot
/// run on a Linux dev box, so this keeps the part of it that is ordinary string matching testable
/// there. Requiring the suffix to be all digits is what stops a user's own
/// `glass-mcp.exe.old-notes.txt` being deleted.
#[cfg(any(windows, test))]
fn is_displaced(entry: &str, exe_name: &str) -> bool {
    entry
        .strip_prefix(&format!("{exe_name}.old-"))
        .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()))
}

/// Delete any binaries a previous update moved aside.
///
/// Windows only, and best-effort: the displaced file cannot be deleted while it is still some
/// process's running image, which is exactly the case during the update that created it. The next
/// run is when it goes away, so failure here is expected and silent.
///
/// Takes the executable's path rather than its directory, so the prefix it matches is derived from
/// the same name `swap` displaced rather than hardcoded a second time.
#[cfg(windows)]
pub(crate) fn sweep_old(exe: &Path) {
    let (Some(dir), Some(exe_name)) = (exe.parent(), exe.file_name()) else {
        return;
    };
    let exe_name = exe_name.to_string_lossy().into_owned();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    for entry in entries.flatten() {
        if is_displaced(&entry.file_name().to_string_lossy(), &exe_name) {
            let _ = std::fs::remove_file(entry.path());
        }
    }
}

/// No displaced binaries exist on Unix — the rename is atomic and leaves nothing behind.
#[cfg(unix)]
pub(crate) fn sweep_old(_exe: &Path) {}

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

    /// `is_displaced` is pure and not cfg-gated precisely so this can run on the Linux dev box —
    /// the Windows sweep itself never executes here, so without this the matching rule would ship
    /// with no test on any machine a developer actually uses.
    #[test]
    fn only_a_pid_suffixed_sibling_counts_as_displaced() {
        assert!(is_displaced("glass-mcp.exe.old-1234", "glass-mcp.exe"));
        // A user's own file that merely shares the prefix must survive the sweep.
        assert!(!is_displaced(
            "glass-mcp.exe.old-notes.txt",
            "glass-mcp.exe"
        ));
        assert!(!is_displaced("glass-mcp.exe.old-", "glass-mcp.exe"));
        assert!(!is_displaced("glass-mcp.exe", "glass-mcp.exe"));
        assert!(!is_displaced("something-else.old-1234", "glass-mcp.exe"));
        // Derived from the exe's own name, so a renamed binary still matches its own displacements.
        assert!(is_displaced("other.exe.old-9", "other.exe"));
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

        sweep_old(&target);
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
