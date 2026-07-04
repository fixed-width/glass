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
                return Some(SdkRoot {
                    path,
                    source: SdkSource::Env(var),
                });
            }
        }
    }
    default_locations(get)
        .into_iter()
        .find(|p| exists(p))
        .map(|path| SdkRoot {
            path,
            source: SdkSource::Default,
        })
}

/// How `adb` was resolved, for honest diagnostics.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum AdbResolution {
    /// From `GLASS_ADB`.
    GlassAdb(String),
    /// `$SDK/platform-tools/adb` under a discovered root.
    Sdk { bin: String, root: SdkRoot },
    /// Fell back to `"adb"` on `PATH`.
    Path,
}

impl AdbResolution {
    /// The adb binary to invoke.
    pub fn bin(&self) -> String {
        match self {
            AdbResolution::GlassAdb(p) => p.clone(),
            AdbResolution::Sdk { bin, .. } => bin.clone(),
            AdbResolution::Path => "adb".to_string(),
        }
    }

    /// A short human description of how adb was found, for `glass doctor`.
    pub fn describe(&self) -> String {
        match self {
            AdbResolution::GlassAdb(p) => format!("{p} (via GLASS_ADB)"),
            AdbResolution::Sdk { bin, root } => {
                format!(
                    "{bin} (via {} SDK {})",
                    source_word(&root.source),
                    root.path.display()
                )
            }
            AdbResolution::Path => "adb (on PATH)".to_string(),
        }
    }
}

fn source_word(s: &SdkSource) -> &'static str {
    match s {
        SdkSource::Env(_) => "configured",
        SdkSource::Default => "discovered",
    }
}

fn adb_exe() -> &'static str {
    #[cfg(windows)]
    {
        "adb.exe"
    }
    #[cfg(not(windows))]
    {
        "adb"
    }
}

/// Resolve adb: `GLASS_ADB` → `$SDK/platform-tools/adb` (when found + exists) → `"adb"`.
pub fn resolve_adb(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> AdbResolution {
    if let Some(p) = get("GLASS_ADB").filter(|s| !s.is_empty()) {
        return AdbResolution::GlassAdb(p);
    }
    if let Some(root) = resolve_sdk_root(get, exists) {
        let bin = root.path.join("platform-tools").join(adb_exe());
        if exists(&bin) {
            return AdbResolution::Sdk {
                bin: bin.to_string_lossy().into_owned(),
                root,
            };
        }
    }
    AdbResolution::Path
}

/// Human labels for the ordered candidates discovery considers — env vars then default
/// locations — so `glass doctor` can render the search trail on failure.
pub fn sdk_search_trail(get: &dyn Fn(&str) -> Option<String>) -> Vec<String> {
    let mut t = vec!["ANDROID_SDK_ROOT".to_string(), "ANDROID_HOME".to_string()];
    t.extend(
        default_locations(get)
            .into_iter()
            .map(|p| p.display().to_string()),
    );
    t
}

/// The glass per-user data dir(s) where optional on-device artifacts (the agent jar,
/// the a11y APK) can be dropped so they're found with no env configuration.
pub fn artifact_data_dirs(get: &dyn Fn(&str) -> Option<String>) -> Vec<PathBuf> {
    let mut v = Vec::new();
    #[cfg(target_os = "windows")]
    for var in ["APPDATA", "LOCALAPPDATA"] {
        if let Some(d) = get(var).filter(|s| !s.is_empty()) {
            v.push(PathBuf::from(d).join("glass"));
        }
    }
    #[cfg(target_os = "macos")]
    if let Some(h) = get("HOME").filter(|s| !s.is_empty()) {
        v.push(PathBuf::from(format!(
            "{h}/Library/Application Support/glass"
        )));
    }
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    {
        if let Some(x) = get("XDG_DATA_HOME").filter(|s| !s.is_empty()) {
            v.push(PathBuf::from(x).join("glass"));
        } else if let Some(h) = get("HOME").filter(|s| !s.is_empty()) {
            v.push(PathBuf::from(format!("{h}/.local/share/glass")));
        }
    }
    v
}

/// The directory holding the running executable, for finding artifacts shipped next to
/// the `glass-mcp` binary. `None` if it can't be determined.
pub fn exe_dir() -> Option<PathBuf> {
    std::env::current_exe()
        .ok()
        .and_then(|p| p.parent().map(PathBuf::from))
}

/// Resolve an optional artifact (e.g. the agent jar) by `env_var` → the first `dirs`
/// entry that holds `filename` on disk → `None`. The env path is returned even when it
/// does not exist, so the caller can still warn "set but missing" rather than silently
/// discovering a different copy.
pub fn resolve_artifact(
    env_var: &str,
    filename: &str,
    dirs: &[PathBuf],
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&Path) -> bool,
) -> Option<String> {
    if let Some(p) = get(env_var).filter(|s| !s.is_empty()) {
        return Some(p);
    }
    dirs.iter()
        .map(|d| d.join(filename))
        .find(|cand| exists(cand))
        .map(|p| p.to_string_lossy().into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn getter(pairs: &'static [(&'static str, &'static str)]) -> impl Fn(&str) -> Option<String> {
        move |k| {
            pairs
                .iter()
                .find(|(n, _)| *n == k)
                .map(|(_, v)| v.to_string())
        }
    }

    #[test]
    fn adb_glass_override_wins() {
        let get = getter(&[("GLASS_ADB", "/custom/adb"), ("ANDROID_SDK_ROOT", "/sdk")]);
        assert_eq!(resolve_adb(&get, &|_| true).bin(), "/custom/adb");
    }

    #[test]
    fn adb_from_discovered_sdk() {
        let get = getter(&[("HOME", "/home/u")]);
        let exists = |p: &Path| {
            p == Path::new("/home/u/android-sdk")
                || p == Path::new("/home/u/android-sdk/platform-tools/adb")
        };
        match resolve_adb(&get, &exists) {
            AdbResolution::Sdk { bin, .. } => {
                assert_eq!(bin, "/home/u/android-sdk/platform-tools/adb")
            }
            other => panic!("expected Sdk, got {other:?}"),
        }
    }

    #[test]
    fn adb_falls_back_to_path() {
        let get = getter(&[("HOME", "/home/u")]);
        assert_eq!(resolve_adb(&get, &|_| false).bin(), "adb");
    }

    #[test]
    fn describe_names_the_discovered_sdk() {
        let r = AdbResolution::Sdk {
            bin: "/home/u/android-sdk/platform-tools/adb".into(),
            root: SdkRoot {
                path: "/home/u/android-sdk".into(),
                source: SdkSource::Default,
            },
        };
        let d = r.describe();
        assert!(
            d.contains("/home/u/android-sdk/platform-tools/adb"),
            "got {d}"
        );
        assert!(d.contains("discovered"), "got {d}");
    }

    #[test]
    fn trail_lists_env_then_defaults() {
        let get = getter(&[("HOME", "/home/u")]);
        let t = sdk_search_trail(&get);
        assert_eq!(&t[0], "ANDROID_SDK_ROOT");
        assert_eq!(&t[1], "ANDROID_HOME");
        assert!(t.iter().any(|s| s.contains("android-sdk")), "trail: {t:?}");
    }

    // Artifact resolution tests pass `dirs` explicitly, so they're OS-agnostic.
    #[test]
    fn artifact_env_override_wins() {
        let get = getter(&[("GLASS_ANDROID_AGENT_JAR", "/explicit/glass-agent.jar")]);
        let dirs = [PathBuf::from("/data/glass")];
        let r = resolve_artifact(
            "GLASS_ANDROID_AGENT_JAR",
            "glass-agent.jar",
            &dirs,
            &get,
            &|_| true,
        );
        assert_eq!(r.as_deref(), Some("/explicit/glass-agent.jar"));
    }

    #[test]
    fn artifact_found_in_data_dir_when_env_unset() {
        let get = getter(&[]);
        let dirs = [PathBuf::from("/data/glass")];
        let exists = |p: &Path| p == Path::new("/data/glass/glass-agent.jar");
        let r = resolve_artifact(
            "GLASS_ANDROID_AGENT_JAR",
            "glass-agent.jar",
            &dirs,
            &get,
            &exists,
        );
        assert_eq!(r.as_deref(), Some("/data/glass/glass-agent.jar"));
    }

    #[test]
    fn artifact_none_when_unset_and_absent() {
        let get = getter(&[]);
        let dirs = [PathBuf::from("/data/glass")];
        let r = resolve_artifact(
            "GLASS_ANDROID_AGENT_JAR",
            "glass-agent.jar",
            &dirs,
            &get,
            &|_| false,
        );
        assert_eq!(r, None);
    }

    #[test]
    fn artifact_env_set_but_missing_still_returns_the_path() {
        // So doctor can warn "set but no file there" instead of silently using another copy.
        let get = getter(&[("GLASS_ANDROID_AGENT_JAR", "/explicit/missing.jar")]);
        let dirs = [PathBuf::from("/data/glass")];
        let r = resolve_artifact(
            "GLASS_ANDROID_AGENT_JAR",
            "glass-agent.jar",
            &dirs,
            &get,
            &|_| false,
        );
        assert_eq!(r.as_deref(), Some("/explicit/missing.jar"));
    }

    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn data_dirs_prefer_xdg_then_home() {
        let get = getter(&[("XDG_DATA_HOME", "/xdg"), ("HOME", "/home/u")]);
        assert_eq!(artifact_data_dirs(&get)[0], PathBuf::from("/xdg/glass"));
        let get2 = getter(&[("HOME", "/home/u")]);
        assert_eq!(
            artifact_data_dirs(&get2)[0],
            PathBuf::from("/home/u/.local/share/glass")
        );
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
        let get = getter(&[
            ("ANDROID_SDK_ROOT", "/nope"),
            ("ANDROID_HOME", "/home/u/android-sdk"),
        ]);
        let r = resolve_sdk_root(&get, &|p| p == Path::new("/home/u/android-sdk")).unwrap();
        assert_eq!(r.source, SdkSource::Env("ANDROID_HOME"));
    }

    // Default install locations are OS-specific (Linux paths here); gate so the
    // cross-platform CI (incl. the Windows job) doesn't run this assertion.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
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
