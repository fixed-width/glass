//! Managed-AVD resolution: where the emulator binary / AVD come from, what flags
//! to boot with, how to tell which device glass just booted, and whether to
//! attach or boot. Pure helpers here; `boot_avd` (subprocess) and the
//! `EmulatorRegistry` (cleanup) follow in later tasks.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::{GlassError, Result};

use crate::adb::Adb;
use crate::target::Device;

/// Attach-or-boot policy from `GLASS_ANDROID_LIFECYCLE`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Lifecycle {
    /// Attach if a device is online, else boot the configured AVD.
    Auto,
    /// Only ever attach; error if no device is online.
    Attach,
}

impl Lifecycle {
    pub fn from_env(v: Option<&str>) -> Lifecycle {
        match v {
            Some(s) if s.eq_ignore_ascii_case("attach") => Lifecycle::Attach,
            _ => Lifecycle::Auto,
        }
    }
}

/// What to do given the current device list + config.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    Attach(String),
    Boot,
    Error(String),
}

/// Resolve the `emulator` binary: `GLASS_EMULATOR`, else `$SDK/emulator/emulator` from a
/// discovered SDK root (env or a common install location), else `"emulator"` (on `PATH`).
pub fn resolve_emulator_bin(
    get: &dyn Fn(&str) -> Option<String>,
    exists: &dyn Fn(&std::path::Path) -> bool,
) -> String {
    if let Some(bin) = get("GLASS_EMULATOR").filter(|s| !s.is_empty()) {
        return bin;
    }
    if let Some(root) = crate::sdk::resolve_sdk_root(get, exists) {
        return root.path.join("emulator").join(emulator_exe()).to_string_lossy().into_owned();
    }
    "emulator".to_string()
}

fn emulator_exe() -> &'static str {
    #[cfg(windows)]
    {
        "emulator.exe"
    }
    #[cfg(not(windows))]
    {
        "emulator"
    }
}

/// Parse `emulator -list-avds` (AVD names, one per line; skip INFO/WARNING noise).
pub fn parse_list_avds(out: &str) -> Vec<String> {
    out.lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.contains('|') && !l.contains(' '))
        .map(str::to_string)
        .collect()
}

/// Pick the AVD: `GLASS_AVD` (must exist), else the sole AVD; error on ambiguous/none/missing.
pub fn choose_avd(want: Option<&str>, avds: &[String]) -> Result<String> {
    let names = |a: &[String]| a.join(", ");
    if let Some(w) = want.filter(|s| !s.is_empty()) {
        return if avds.iter().any(|a| a == w) {
            Ok(w.to_string())
        } else {
            Err(GlassError::Backend(format!("GLASS_AVD={w} not found; AVDs: [{}]", names(avds))))
        };
    }
    match avds {
        [] => Err(GlassError::Backend(
            "no AVDs found; create one (e.g. `avdmanager create avd`) or set GLASS_AVD".into(),
        )),
        [one] => Ok(one.clone()),
        many => Err(GlassError::Backend(format!(
            "{} AVDs; set GLASS_AVD to one of: [{}]",
            many.len(),
            names(many)
        ))),
    }
}

/// Headless boot args for `emulator`, plus any whitespace-split `extra` (`GLASS_EMULATOR_ARGS`).
pub fn emulator_args(avd: &str, extra: Option<&str>) -> Vec<String> {
    let mut v: Vec<String> = ["-avd", avd, "-no-window", "-no-audio", "-no-boot-anim"]
        .iter()
        .map(|s| s.to_string())
        .collect();
    if let Some(extra) = extra {
        v.extend(extra.split_whitespace().map(str::to_string));
    }
    v
}

/// The serial present in `after` but not `before` (the device glass just booted).
/// Returns the first such serial; assumes no unrelated emulator boots during glass's
/// own boot window (otherwise the wrong new device could be picked).
pub fn new_serial(before: &[Device], after: &[Device]) -> Option<String> {
    after
        .iter()
        .find(|d| !before.iter().any(|b| b.serial == d.serial))
        .map(|d| d.serial.clone())
}

/// Attach-or-boot decision. Attach-preferred; a specific requested serial that is
/// offline is an error (never boot a mismatched serial).
pub fn decide(online: &[Device], serial_env: Option<&str>, lifecycle: Lifecycle) -> Action {
    let names = |d: &[Device]| d.iter().map(|x| x.serial.as_str()).collect::<Vec<_>>().join(", ");
    if let Some(want) = serial_env.filter(|s| !s.is_empty()) {
        return if online.iter().any(|d| d.serial == want) {
            Action::Attach(want.to_string())
        } else {
            Action::Error(format!(
                "GLASS_ANDROID_SERIAL={want} is not online; online: [{}]",
                names(online)
            ))
        };
    }
    match online {
        [] => match lifecycle {
            Lifecycle::Auto => Action::Boot,
            Lifecycle::Attach => Action::Error(
                "no online device; start an emulator or set GLASS_ANDROID_LIFECYCLE=auto".into(),
            ),
        },
        [one] => Action::Attach(one.serial.clone()),
        many => Action::Error(format!(
            "{} online devices; set GLASS_ANDROID_SERIAL to one of: [{}]",
            many.len(),
            names(many)
        )),
    }
}

/// Serials of emulators glass booted itself, so they can be stopped on shutdown.
/// Cloneable + `Send` (shared `Arc`); glass-mcp threads one clone into the platform
/// factory (to register boots) and another into the `Glass` shutdown hook (to kill).
#[derive(Clone, Default)]
pub struct EmulatorRegistry {
    booted: Arc<Mutex<Vec<String>>>,
}

impl EmulatorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an emulator serial glass booted.
    pub fn register(&self, serial: String) {
        if let Ok(mut g) = self.booted.lock() {
            g.push(serial);
        }
    }

    /// Stop every registered emulator (`adb -s <serial> emu kill`) and clear the list.
    /// Best-effort: a device already gone is fine. Resolves adb from env.
    pub fn kill_all(&self) {
        let adb = Adb::from_env();
        let serials = self.booted.lock().map(|mut g| std::mem::take(&mut *g)).unwrap_or_default();
        for s in serials {
            let _ = adb.with_serial(s).run(["emu", "kill"]);
        }
    }

    #[cfg(test)]
    pub fn serials(&self) -> Vec<String> {
        self.booted.lock().map(|g| g.clone()).unwrap_or_default()
    }
}

/// Boot the configured AVD headless and return the serial of the device that came up.
/// Spawns the emulator detached, waits for a new device + `sys.boot_completed`, and
/// errors (after killing the half-booted emulator) on timeout or spawn failure.
pub fn boot_avd(base: &Adb, get: &dyn Fn(&str) -> Option<String>) -> Result<String> {
    let bin = resolve_emulator_bin(get, &|p| p.exists());
    let avds = parse_list_avds(&run_emulator_list(&bin)?);
    let avd = choose_avd(get("GLASS_AVD").as_deref(), &avds)?;
    let args = emulator_args(&avd, get("GLASS_EMULATOR_ARGS").as_deref());

    let before = crate::target::parse_devices(&base.run(["devices"])?);

    let mut child = Command::new(&bin)
        .args(&args)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| GlassError::Backend(format!("failed to spawn emulator `{bin}`: {e}")))?;

    let timeout_ms: u64 =
        get("GLASS_EMULATOR_BOOT_TIMEOUT_MS").and_then(|s| s.parse().ok()).unwrap_or(120_000);
    let deadline = Instant::now() + Duration::from_millis(timeout_ms);
    loop {
        if let Ok(Some(status)) = child.try_wait() {
            let mut err = String::new();
            if let Some(mut e) = child.stderr.take() {
                use std::io::Read as _;
                let _ = e.read_to_string(&mut err);
            }
            // Child already exited — no kill needed, but we must wait to reap it.
            let _ = child.wait();
            return Err(GlassError::Backend(format!(
                "emulator exited before boot (status {status}): {}",
                err.trim()
            )));
        }
        let online = crate::target::parse_devices(&base.run(["devices"])?);
        if let Some(serial) = new_serial(&before, &online) {
            let adb = base.with_serial(serial.clone());
            if adb.run(["shell", "getprop", "sys.boot_completed"])
                .map(|o| o.trim() == "1")
                .unwrap_or(false)
            {
                return Ok(serial);
            }
        }
        if Instant::now() >= deadline {
            // Belt-and-suspenders: ask the device to shut down gracefully, then kill the
            // host emulator child so it can't hold the AVD lock after we return an error.
            if let Some(serial) = new_serial(
                &before,
                &crate::target::parse_devices(&base.run(["devices"]).unwrap_or_default()),
            ) {
                let _ = base.with_serial(serial).run(["emu", "kill"]);
            }
            let _ = child.kill();
            let _ = child.wait();
            return Err(GlassError::Backend(format!(
                "emulator did not reach sys.boot_completed within {timeout_ms}ms"
            )));
        }
        std::thread::sleep(Duration::from_millis(500));
    }
}

fn run_emulator_list(bin: &str) -> Result<String> {
    let out = Command::new(bin)
        .arg("-list-avds")
        .output()
        .map_err(|e| GlassError::Backend(format!("failed to run `{bin} -list-avds`: {e}")))?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Device;

    fn dev(s: &str) -> Device { Device { serial: s.into(), state: "device".into() } }

    #[test]
    fn emulator_bin_prefers_glass_then_sdk_then_path() {
        let env = |k: &str| match k {
            "GLASS_EMULATOR" => Some("/custom/emulator".to_string()),
            _ => None,
        };
        assert_eq!(resolve_emulator_bin(&env, &|_| true), "/custom/emulator");

        let env = |k: &str| match k {
            "ANDROID_SDK_ROOT" => Some("/sdk".to_string()),
            _ => None,
        };
        assert_eq!(resolve_emulator_bin(&env, &|_| true), "/sdk/emulator/emulator");

        let env = |k: &str| match k {
            "ANDROID_HOME" => Some("/home/sdk".to_string()),
            _ => None,
        };
        assert_eq!(resolve_emulator_bin(&env, &|_| true), "/home/sdk/emulator/emulator");

        let env = |_: &str| None;
        assert_eq!(resolve_emulator_bin(&env, &|_| false), "emulator");
    }

    // Default install locations are OS-specific (Linux paths here); gate so the
    // cross-platform CI (incl. the Windows job) doesn't run this assertion.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn emulator_from_discovered_default_sdk() {
        // No env at all, but a default SDK location exists on disk.
        let env = |k: &str| match k {
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        let exists = |p: &std::path::Path| p == std::path::Path::new("/home/u/android-sdk");
        assert_eq!(resolve_emulator_bin(&env, &exists), "/home/u/android-sdk/emulator/emulator");
    }

    #[test]
    fn list_avds_parses_names_only() {
        let out = "INFO | Storing crashdata\nPixel_6\nglass\n";
        assert_eq!(parse_list_avds(out), vec!["Pixel_6".to_string(), "glass".to_string()]);
    }

    #[test]
    fn choose_avd_sole_or_named_or_errors() {
        assert_eq!(choose_avd(None, &["glass".into()]).unwrap(), "glass");
        assert_eq!(choose_avd(Some("glass"), &["a".into(), "glass".into()]).unwrap(), "glass");
        assert!(choose_avd(None, &["a".into(), "b".into()]).is_err());
        assert!(choose_avd(None, &[]).is_err());
        assert!(choose_avd(Some("nope"), &["a".into()]).is_err());
    }

    #[test]
    fn emulator_args_are_headless_plus_extra() {
        assert_eq!(
            emulator_args("glass", None),
            ["-avd", "glass", "-no-window", "-no-audio", "-no-boot-anim"]
        );
        let with = emulator_args("glass", Some("-no-snapshot -gpu swiftshader_indirect"));
        assert_eq!(with.last().unwrap(), "swiftshader_indirect");
        assert!(with.contains(&"-no-snapshot".to_string()));
    }

    #[test]
    fn new_serial_is_the_added_device() {
        let before = vec![dev("emulator-5554")];
        let after = vec![dev("emulator-5554"), dev("emulator-5556")];
        assert_eq!(new_serial(&before, &after).as_deref(), Some("emulator-5556"));
        assert_eq!(new_serial(&before, &before), None);
    }

    #[test]
    fn decide_attach_or_boot_or_error() {
        assert_eq!(decide(&[dev("emulator-5554")], None, Lifecycle::Auto),
                   Action::Attach("emulator-5554".into()));
        assert_eq!(decide(&[], None, Lifecycle::Auto), Action::Boot);
        assert!(matches!(decide(&[], None, Lifecycle::Attach), Action::Error(_)));
        assert!(matches!(
            decide(&[dev("a"), dev("b")], None, Lifecycle::Auto), Action::Error(_)));
        assert_eq!(decide(&[dev("a"), dev("b")], Some("b"), Lifecycle::Auto),
                   Action::Attach("b".into()));
        assert!(matches!(decide(&[dev("a")], Some("z"), Lifecycle::Auto), Action::Error(_)));
    }

    #[test]
    fn lifecycle_from_env() {
        assert_eq!(Lifecycle::from_env(Some("attach")), Lifecycle::Attach);
        assert_eq!(Lifecycle::from_env(Some("AUTO")), Lifecycle::Auto);
        assert_eq!(Lifecycle::from_env(None), Lifecycle::Auto);
    }

    #[test]
    fn registry_records_serials() {
        let r = EmulatorRegistry::new();
        let r2 = r.clone();
        r.register("emulator-5554".into());
        r2.register("emulator-5556".into());
        assert_eq!(r.serials(), vec!["emulator-5554".to_string(), "emulator-5556".to_string()]);
    }
}
