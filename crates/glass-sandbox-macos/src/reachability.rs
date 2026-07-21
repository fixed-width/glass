//! Compute the Seatbelt re-allows that make a sandboxed launch's OWN target reachable under the
//! `/Users` read-deny, without re-exposing home. Resolution is shared with the Linux backend
//! (`glass-sandbox-core`); only the emit differs: a safe directory becomes a `(subpath …)`
//! re-allow (ro_binds), a file directly under a bare home root becomes a single-file `(literal …)`
//! re-allow (ro_files), and a target that IS a home root or above contributes nothing.

use std::path::{Path, PathBuf};

use glass_sandbox_core::{abs_token, canon, dir_of, resolve_on_path};

use crate::profile::{is_home_root_or_above, is_safe_reallow};

/// The extra re-allows a launch needs, split by the SBPL form `build_profile` emits for each:
/// `ro_binds` → `(subpath …)` (dir + siblings), `ro_files` → `(literal …)` (one file, no siblings).
#[derive(Debug, Default, PartialEq, Eq)]
pub struct LaunchReallows {
    pub ro_binds: Vec<PathBuf>,
    pub ro_files: Vec<PathBuf>,
}

/// Re-allows for the LITERAL launch target (program + path args) so the sandboxed app can read its
/// own script/asset/binary living under `/Users`. `run[0]` bare-name is `$PATH`-resolved like
/// `execvp`; relative tokens resolve against `cwd`; a token that does not `stat` is skipped.
pub fn launch_reallows(run: &[String], cwd: Option<&Path>) -> LaunchReallows {
    let mut out = LaunchReallows::default();
    let Some((program, args)) = run.split_first() else {
        return out;
    };

    // run[0]: a path token (has '/') resolves as-is / against cwd; a bare name resolves on $PATH.
    let prog_path = Path::new(program);
    if program.contains('/') {
        if let Some(p) = abs_token(prog_path, cwd) {
            push_reallows(&mut out, &p);
        }
    } else if let Some(resolved) = resolve_on_path(std::ffi::OsStr::new(program)) {
        // Only re-allow when the resolving dir is under /Users (hidden by the deny). A /usr/bin,
        // /opt/homebrew match is already visible via (subpath "/") — is_safe_reallow of that dir is
        // true, so push_reallows would emit a harmless-but-redundant subpath; skip it explicitly to
        // mirror Linux's "usr/bin contributes nothing" and avoid a puzzling re-allow.
        let dir = dir_of(&resolved);
        if dir.starts_with("/Users") {
            push_reallows(&mut out, &resolved);
        }
    }

    // run[1..]: absolute or cwd-relative path tokens (never $PATH-resolved).
    for a in args {
        if let Some(p) = abs_token(Path::new(a), cwd) {
            push_reallows(&mut out, &p);
        }
    }
    out
}

/// Append the guarded re-allows for one resolved literal launch-target path, de-duplicated.
/// Seatbelt reads the LITERAL path, but a symlink's target may live elsewhere, so BOTH the literal
/// path's directory and the resolved target's directory are considered. A target that does not
/// exist (any `stat` error) contributes nothing (fail-safe). A target that IS a home root or above
/// contributes nothing (never re-expose home). A safe directory → `ro_binds` (subpath); a directory
/// that is a bare home root → the FILE only → `ro_files` (literal).
fn push_reallows(out: &mut LaunchReallows, lit: &Path) {
    if std::fs::metadata(lit).is_err() {
        return; // a flag, a value, or a missing file — not a reachable path
    }
    let real = canon(lit);
    if is_home_root_or_above(&real) {
        return;
    }
    for dir in [dir_of(lit), dir_of(&real)] {
        if is_safe_reallow(&dir) {
            if !out.ro_binds.contains(&dir) {
                out.ro_binds.push(dir);
            }
        } else {
            // dir is a bare home root (or above): re-allow just the file as a literal, reachable
            // even directly under the home root, granting no sibling access. Use `real` (the
            // resolved file) — a literal must name the file the app actually opens.
            if !out.ro_files.contains(&real) {
                out.ro_files.push(real.clone());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // -------------------------------------------------------------------------
    // Helpers
    // -------------------------------------------------------------------------

    /// A representative synthetic bare home root for a few different usernames — used to sweep the
    /// predicates without needing real files under `/Users` (impossible on this Linux dev box).
    const SYNTHETIC_HOME_ROOTS: &[&str] = &["/Users/dev", "/Users/alice", "/Users/ci-runner"];

    // -------------------------------------------------------------------------
    // 1. Absolute path arg under a safe project dir → its dir in ro_binds, never a home root.
    // REAL: exercises push_reallows's safe-dir → ro_binds branch end-to-end via a real tempdir
    // (Linux can't canonicalize a tempdir under /Users, so this proves the general branch; the
    // /Users-specific classification is asserted separately via the predicate, which is the exact
    // guard `push_reallows` consults for a path that *is* under /Users).
    // -------------------------------------------------------------------------
    #[test]
    fn absolute_arg_under_project_dir_reallows_the_dir() {
        let proj = tempfile::tempdir().unwrap();
        let script = proj.path().join("app.py");
        std::fs::write(&script, b"").unwrap();
        let out = launch_reallows(
            &["python3".to_string(), script.to_string_lossy().into_owned()],
            None,
        );
        assert_eq!(out.ro_binds, vec![proj.path().canonicalize().unwrap()]);
        assert!(out.ro_files.is_empty());

        // Predicate: had this script lived under /Users/u/proj (a real project dir under home),
        // is_safe_reallow would route it to ro_binds exactly like the tempdir case above.
        assert!(
            is_safe_reallow(Path::new("/Users/u/proj")),
            "a real project dir under home must be classified safe (→ ro_binds)"
        );
    }

    // -------------------------------------------------------------------------
    // 2. Arg directly under a bare home root → the FILE in ro_files, the home root NOT in ro_binds.
    // PREDICATE-ONLY: `push_reallows` requires `std::fs::metadata(lit)` to succeed before routing,
    // and this dev box has no real `/Users` tree to place a file under — placing one is impossible
    // without root and would not be legitimate for a portable unit test. Verified instead: the two
    // predicates `push_reallows` actually branches on for this exact scenario. Full end-to-end
    // exercise of this branch is an ON-DEVICE (mini) verification case — see report.
    // -------------------------------------------------------------------------
    #[test]
    fn arg_directly_under_home_root_reallows_file_not_dir() {
        let file = Path::new("/Users/u/app.py");
        let dir = Path::new("/Users/u");
        // The file itself is not a home root or above, so push_reallows would NOT skip it outright.
        assert!(
            !is_home_root_or_above(file),
            "a file directly under a home root is not itself a home root"
        );
        // Its directory IS a bare home root, so is_safe_reallow routes it to the ro_files (literal)
        // branch rather than ro_binds (subpath) — never exposing the whole home as a directory.
        assert!(
            is_home_root_or_above(dir) || !is_safe_reallow(dir),
            "a bare home root directory must never be treated as a safe ro_binds target"
        );
        assert!(!is_safe_reallow(dir), "bare home root routes to ro_files");
    }

    // -------------------------------------------------------------------------
    // 3. A directory arg under a safe dir → the dir itself in ro_binds.
    // REAL: general branch (dir_of a directory input is itself); /Users classification via
    // predicate, same rationale as test 1.
    // -------------------------------------------------------------------------
    #[test]
    fn directory_arg_reallows_itself() {
        let proj = tempfile::tempdir().unwrap();
        let data = proj.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let out = launch_reallows(
            &[
                "srv".to_string(),
                "--root".to_string(),
                data.to_string_lossy().into_owned(),
            ],
            None,
        );
        assert_eq!(out.ro_binds, vec![data.canonicalize().unwrap()]);
        assert!(out.ro_files.is_empty());

        assert!(
            is_safe_reallow(Path::new("/Users/u/proj/data")),
            "a data dir nested under a project dir under home must be classified safe"
        );
    }

    // -------------------------------------------------------------------------
    // 4. Guard: an arg of /Users, /Users/<user>, or / → nothing emitted.
    // MIXED: "/" genuinely exists on Linux, so it is exercised REAL end-to-end through
    // launch_reallows (metadata succeeds, then the home-root-or-above guard fires). "/Users" and
    // "/Users/u" don't exist on this box, so exercising them through launch_reallows would only
    // prove the (wrong) "nonexistent path" skip, not the home-root guard — asserted instead via the
    // predicate directly, which is exactly what push_reallows consults after a successful stat.
    // -------------------------------------------------------------------------
    #[test]
    fn home_root_or_above_arg_is_skipped() {
        // REAL: "/" exists on every Unix host, so this proves the guard fires end-to-end.
        let out = launch_reallows(&["true".to_string(), "/".to_string()], None);
        assert!(out.ro_binds.is_empty());
        assert!(out.ro_files.is_empty());

        // PREDICATE: /Users and a bare home root, the two cases that can't be placed on disk here.
        assert!(is_home_root_or_above(Path::new("/Users")));
        assert!(is_home_root_or_above(Path::new("/Users/u")));
        assert!(is_home_root_or_above(Path::new("/")));
    }

    // -------------------------------------------------------------------------
    // 5. Relative arg resolved against cwd → its dir; relative with cwd None → skipped.
    // REAL: general resolution logic, not /Users-specific.
    // -------------------------------------------------------------------------
    #[test]
    fn relative_arg_uses_cwd_and_is_skipped_without_cwd() {
        let cwd = tempfile::tempdir().unwrap();
        let sub = cwd.path().join("assets");
        std::fs::create_dir_all(&sub).unwrap();
        std::fs::write(sub.join("data.bin"), b"").unwrap();

        let out = launch_reallows(
            &["python3".to_string(), "assets/data.bin".to_string()],
            Some(cwd.path()),
        );
        assert_eq!(out.ro_binds, vec![sub.canonicalize().unwrap()]);

        // With no cwd, the same relative token resolves to nothing and is skipped.
        let out_none = launch_reallows(
            &["python3".to_string(), "assets/data.bin".to_string()],
            None,
        );
        assert!(out_none.ro_binds.is_empty());
        assert!(out_none.ro_files.is_empty());
    }

    // -------------------------------------------------------------------------
    // 6. Symlink program: literal dir AND resolved-target dir both re-allowed.
    // REAL: exercises push_reallows's dual-dir (literal + resolved target) dedup logic.
    // -------------------------------------------------------------------------
    #[test]
    fn symlink_program_reallows_literal_and_target_dirs() {
        let bindir_root = tempfile::tempdir().unwrap();
        let bindir = bindir_root.path().join("venv/bin");
        std::fs::create_dir_all(&bindir).unwrap();

        let libdir_root = tempfile::tempdir().unwrap();
        let libdir = libdir_root.path().join("venv/lib");
        std::fs::create_dir_all(&libdir).unwrap();
        let target = libdir.join("python3.real");
        std::fs::write(&target, b"").unwrap();

        let link = bindir.join("python");
        std::os::unix::fs::symlink(&target, &link).unwrap();

        let out = launch_reallows(&[link.to_string_lossy().into_owned()], None);
        assert!(
            out.ro_binds.contains(&bindir.canonicalize().unwrap()),
            "literal symlink's dir must be re-allowed so Seatbelt can open it as written: {out:?}"
        );
        assert!(
            out.ro_binds.contains(&libdir.canonicalize().unwrap()),
            "resolved target's dir must also be re-allowed: {out:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 7. Bare-name program only under a $HOME PATH dir → its dir; under /usr/bin → nothing.
    // MIXED: the /usr/bin negative case is REAL (env is guaranteed present on a non-shadowed dir).
    // The positive "home PATH dir" case is gated on `dir.starts_with("/Users")` in launch_reallows
    // BEFORE any is_safe_reallow/tempdir logic runs, and `resolve_on_path` additionally requires the
    // executable to actually exist — so it cannot be exercised without a real `/Users` tree. Deferred
    // to ON-DEVICE (mini) verification — see report.
    // -------------------------------------------------------------------------
    #[test]
    fn bare_name_program_on_home_path_dir_is_reallowed_usr_bin_is_not() {
        // `env` is coreutils, guaranteed present under /usr/bin (a non-shadowed dir already visible
        // via the whole-filesystem read-allow), so resolving it must contribute nothing.
        let out = launch_reallows(&["env".to_string()], None);
        assert!(
            out.ro_binds.is_empty() && out.ro_files.is_empty(),
            "a bare name resolving under /usr/bin must not be re-allowed: {out:?}"
        );
    }

    // -------------------------------------------------------------------------
    // 8. Invariant over a mixed launch: no ro_binds entry is /, /Users, or a bare home root.
    // LOAD-BEARING. MIXED: the REAL half runs a mixed launch (project dir, directory arg, relative
    // arg, and the real "/" arg) through launch_reallows and asserts the invariant on the actual
    // output. The PREDICATE half closes the gap the real half can't reach on this box (no /Users
    // tree): it sweeps representative home-root paths and proves algebraically that
    // `push_reallows` could never place one in ro_binds — is_home_root_or_above is true for /,
    // /Users, and every bare home root (so push_reallows returns before touching either list), and
    // for a file living directly under a bare home root, is_safe_reallow(dir_of(file)) is false (so
    // even when not skipped outright, the routing is forced to ro_files, never ro_binds). Together
    // these are exactly the two branches push_reallows has for reaching ro_binds — proving neither
    // can produce a home-root entry closes the invariant.
    // -------------------------------------------------------------------------
    #[test]
    fn no_ro_bind_is_a_home_root_or_above() {
        // --- REAL half ---
        let proj = tempfile::tempdir().unwrap();
        let data = proj.path().join("data");
        std::fs::create_dir_all(&data).unwrap();
        let script = proj.path().join("app.py");
        std::fs::write(&script, b"").unwrap();
        let cwd = tempfile::tempdir().unwrap();
        std::fs::write(cwd.path().join("r.sh"), b"").unwrap();

        let out = launch_reallows(
            &[
                "python3".to_string(),
                script.to_string_lossy().into_owned(),
                data.to_string_lossy().into_owned(),
                "/".to_string(), // a home-root-or-above candidate: must contribute nothing
                "r.sh".to_string(),
            ],
            Some(cwd.path()),
        );
        assert!(
            !out.ro_binds.is_empty(),
            "sanity: launch should bind something"
        );
        for b in &out.ro_binds {
            assert!(
                !is_home_root_or_above(b),
                "ro_binds entry {b:?} is a home root or above"
            );
        }

        // --- PREDICATE half: sweep every path push_reallows could branch on for a home root ---
        for root in [Path::new("/"), Path::new("/Users")] {
            assert!(
                is_home_root_or_above(root),
                "{root:?} must short-circuit before either list is touched"
            );
        }
        for root in SYNTHETIC_HOME_ROOTS {
            let root = Path::new(root);
            assert!(
                is_home_root_or_above(root),
                "{root:?} (a bare home root) must short-circuit before either list is touched"
            );
            // A file living directly under that root (dir_of == root) must never route to
            // ro_binds: the file itself is not a home root or above, so it is NOT skipped
            // outright, but is_safe_reallow(root) is false, forcing the ro_files branch.
            let file = root.join("app.py");
            assert!(
                !is_home_root_or_above(&file),
                "{file:?} is a file, not itself a home root"
            );
            assert!(
                !is_safe_reallow(root),
                "{root:?} must never be classified safe — that is the only way push_reallows \
                 could place a home root's contents in ro_binds"
            );
        }
    }
}
