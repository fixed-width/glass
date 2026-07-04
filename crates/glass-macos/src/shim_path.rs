//! Pure clip-shim dylib path resolution. Cross-platform → unit-tested on the Linux dev box,
//! same split as [`crate::clipboard_route`]/[`crate::coords`]/[`crate::keymap`]: no OS calls
//! here, only `Path`/`PathBuf` arithmetic and existence checks against whatever paths the
//! caller hands in. `glass_macos::process::shim_dylib_path` (macOS-only) is the thin wrapper
//! that supplies the real executable directory and env override.
#![forbid(unsafe_code)]

use std::path::{Path, PathBuf};

/// File name of the shim's build artifact: `glass-clip-shim-macos`'s `crate-type =
/// ["cdylib"]` compiles to `lib<crate name, underscored>.dylib` on macOS.
pub const SHIM_DYLIB_NAME: &str = "libglass_clip_shim_macos.dylib";

/// Resolve the injected clip shim's dylib, given the running executable's directory and an
/// already-read env-override value (the caller reads `GLASS_CLIP_SHIM_DYLIB` itself — this
/// function takes no env/filesystem-location input of its own, only what's passed in).
///
/// Tiers, in order:
/// 1. `env_override`, if `Some`.
/// 2. `<exe_dir>/../Frameworks/<SHIM_DYLIB_NAME>` — a packaged `.app`'s bundle layout: the
///    executable lives in `Contents/MacOS`, and a packaged install ships the shim one level
///    up in `Contents/Frameworks`. Checked before the dev target-dir tier so an installed
///    bundle always prefers its own bundled shim over a stray dev build on the same machine.
/// 3. `<exe_dir>/<SHIM_DYLIB_NAME>` — next to the running executable.
/// 4. `<exe_dir>/../<SHIM_DYLIB_NAME>` — the cargo target dir one level up from it
///    (`current_exe` is `target/<profile>/<bin>` for a normal build, or
///    `target/<profile>/deps/<bin>-<hash>` under `cargo test`, one directory deeper than the
///    shim's own build output — hence this candidate).
///
/// Every tier, including the env override, is existence-checked (`.is_file()`) before being
/// returned — a bad override (stale/typo'd path) falls through to the remaining tiers rather
/// than being trusted blind, same fail-closed discipline throughout. `None` if none of these
/// exist: callers treat that as "not injectable" (fail-closed — no resolvable shim, no
/// injection).
pub fn resolve_shim(exe_dir: &Path, env_override: Option<PathBuf>) -> Option<PathBuf> {
    if let Some(path) = env_override {
        if path.is_file() {
            return Some(path);
        }
    }
    if let Some(bundled) = exe_dir
        .parent()
        .map(|contents| contents.join("Frameworks").join(SHIM_DYLIB_NAME))
    {
        if bundled.is_file() {
            return Some(bundled);
        }
    }
    let next_to_exe = exe_dir.join(SHIM_DYLIB_NAME);
    if next_to_exe.is_file() {
        return Some(next_to_exe);
    }
    let target_dir = exe_dir.parent()?.join(SHIM_DYLIB_NAME);
    target_dir.is_file().then_some(target_dir)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A dropped temp file: `resolve_shim` only ever returns paths that pass `.is_file()`,
    /// so every positive-case test needs a real file on disk, cleaned up on drop even if an
    /// assertion panics.
    struct TempFile(PathBuf);
    impl TempFile {
        fn create(path: PathBuf) -> Self {
            if let Some(parent) = path.parent() {
                std::fs::create_dir_all(parent).expect("create parent dirs");
            }
            std::fs::write(
                &path,
                b"stand-in dylib contents; only existence matters here",
            )
            .expect("write stand-in file");
            Self(path)
        }
    }
    impl Drop for TempFile {
        fn drop(&mut self) {
            let _ = std::fs::remove_file(&self.0);
        }
    }

    /// A fresh scratch directory per test (rather than a shared one), so parallel test
    /// threads never see each other's tiers.
    fn scratch_dir(tag: &str) -> PathBuf {
        std::env::temp_dir().join(format!("glass-shim-path-test-{tag}-{}", std::process::id()))
    }

    #[test]
    fn env_override_that_exists_wins() {
        let dir = scratch_dir("env-override");
        let dylib = TempFile::create(dir.join("override.dylib"));
        let resolved = resolve_shim(Path::new("/nonexistent/exe/dir"), Some(dylib.0.clone()));
        assert_eq!(resolved, Some(dylib.0.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn nonexistent_env_override_falls_through() {
        // Unlike a fail-open design, a bad override (stale env, typo'd path) must not be
        // trusted blind: it falls through to the remaining tiers (here, `None`, since no
        // other tier has a file either) rather than being handed back as-is.
        let bogus = PathBuf::from("/nonexistent/glass-clip-shim-test.dylib");
        let resolved = resolve_shim(Path::new("/also/nonexistent/exe/dir"), Some(bogus.clone()));
        assert_ne!(
            resolved,
            Some(bogus),
            "a nonexistent override must not be returned as-is"
        );
    }

    #[test]
    fn bundle_relative_frameworks_dir_is_preferred_over_target_dir() {
        // `…/Contents/MacOS/` (exe dir) + a file at `…/Contents/Frameworks/<name>` — the
        // packaged `.app` layout `build-app.sh` produces.
        let dir = scratch_dir("bundle");
        let exe_dir = dir.join("Contents").join("MacOS");
        let bundled = TempFile::create(
            dir.join("Contents")
                .join("Frameworks")
                .join(SHIM_DYLIB_NAME),
        );
        // Also drop a file at the target-dir tier's location (`Contents/<name>`) to prove
        // the bundle tier is checked, and wins, before it.
        let target_dir_candidate = TempFile::create(dir.join("Contents").join(SHIM_DYLIB_NAME));

        let resolved = resolve_shim(&exe_dir, None);
        assert_eq!(resolved, Some(bundled.0.clone()));
        drop(target_dir_candidate);
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn next_to_exe_tier_is_used_when_no_bundle_or_override() {
        let dir = scratch_dir("next-to-exe");
        let exe_dir = dir.clone();
        let sibling = TempFile::create(dir.join(SHIM_DYLIB_NAME));
        let resolved = resolve_shim(&exe_dir, None);
        assert_eq!(resolved, Some(sibling.0.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn target_dir_tier_is_used_as_last_resort() {
        let dir = scratch_dir("target-dir");
        let exe_dir = dir.join("deps");
        std::fs::create_dir_all(&exe_dir).expect("create exe dir");
        let target_dylib = TempFile::create(dir.join(SHIM_DYLIB_NAME));
        let resolved = resolve_shim(&exe_dir, None);
        assert_eq!(resolved, Some(target_dylib.0.clone()));
        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn none_when_nothing_resolves() {
        let dir = scratch_dir("nothing");
        let exe_dir = dir.join("Contents").join("MacOS");
        std::fs::create_dir_all(&exe_dir).expect("create exe dir");
        assert_eq!(resolve_shim(&exe_dir, None), None);
        let _ = std::fs::remove_dir_all(&dir);
    }
}
