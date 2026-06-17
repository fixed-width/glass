//! Android SDK discovery: locate the SDK root — and from it `adb`/`emulator` — so the
//! backend works with zero configuration, falling back through env overrides and common
//! install locations. Pure resolution: the only impurities are injected (`get` for env,
//! `exists` for filesystem presence), so every branch is unit-tested without I/O.

use std::path::{Path, PathBuf};

/// How the SDK root was located, for honest diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum SdkSource {
    /// Named by an environment variable.
    Env(&'static str),
    /// Found at a default install location on disk.
    Default,
}

/// A resolved SDK root and how it was found.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SdkRoot {
    pub path: PathBuf,
    pub source: SdkSource,
}

/// Default SDK install locations to probe for the current OS, given `$HOME`
/// (and `%LOCALAPPDATA%` on Windows) read via `get`.
fn default_locations(get: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(target_os = "windows")]
    if let Some(la) = get("LOCALAPPDATA").filter(|s| !s.is_empty()) {
        v.push(PathBuf::from(format!(r"{la}\Android\Sdk")));
    }
    #[cfg(target_os = "macos")]
    if let Some(h) = get("HOME").filter(|s| !s.is_empty()) {
        v.push(PathBuf::from(format!("{h}/Library/Android/sdk")));
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(h) = get("HOME").filter(|s| !s.is_empty()) {
            v.push(PathBuf::from(format!("{h}/Android/Sdk")));
            v.push(PathBuf::from(format!("{h}/android-sdk")));
        }
        v.push(PathBuf::from("/opt/android-sdk"));
        v.push(PathBuf::from("/usr/lib/android-sdk"));
    }
    v
}

/// Resolve the SDK root: `ANDROID_SDK_ROOT`, else `ANDROID_HOME` (each only if the dir
/// exists), else the first existing default install location. `None` when nothing is found.
pub fn resolve_sdk_root(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<SdkRoot> {
    for var in ["ANDROID_SDK_ROOT", "ANDROID_HOME"] {
        if let Some(p) = get(var).filter(|s| !s.is_empty()) {
            let path = PathBuf::from(p);
            if exists(&path) {
                return Some(SdkRoot { path, source: SdkSource::Env(var) });
            }
        }
    }
    default_locations(get)
        .into_iter()
        .find(|p| exists(p))
        .map(|path| SdkRoot { path, source: SdkSource::Default })
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| pairs.iter().find(|(n, _)| *n == k).map(|(_, v)| v.to_string())
    }

    #[test]
    fn env_root_wins_when_it_exists() {
        let get = getter(&[("ANDROID_SDK_ROOT", "/sdk"), ("HOME", "/home/u")]);
        let r = resolve_sdk_root(&get, &|p| p == Path::new("/sdk")).unwrap();
        assert_eq!(r.path, PathBuf::from("/sdk"));
        assert_eq!(r.source, SdkSource::Env("ANDROID_SDK_ROOT"));
    }

    #[test]
    fn falls_through_env_root_that_is_missing() {
        // ANDROID_SDK_ROOT set but absent; ANDROID_HOME present and exists.
        let get = getter(&[("ANDROID_SDK_ROOT", "/nope"), ("ANDROID_HOME", "/home/u/android-sdk")]);
        let r = resolve_sdk_root(&get, &|p| p == Path::new("/home/u/android-sdk")).unwrap();
        assert_eq!(r.source, SdkSource::Env("ANDROID_HOME"));
    }

    #[test]
    fn discovers_default_location_with_no_env() {
        let get = getter(&[("HOME", "/home/u")]);
        let r = resolve_sdk_root(&get, &|p| p == Path::new("/home/u/android-sdk")).unwrap();
        assert_eq!(r.path, PathBuf::from("/home/u/android-sdk"));
        assert_eq!(r.source, SdkSource::Default);
    }

    #[test]
    fn none_when_nothing_exists() {
        let get = getter(&[("HOME", "/home/u")]);
        assert!(resolve_sdk_root(&get, &|_| false).is_none());
    }
}
