//! Managed-AVD resolution: where the emulator binary / AVD come from, what flags
//! to boot with, how to tell which device glass just booted, and whether to
//! attach or boot. Pure helpers here; `boot_avd` (subprocess) and the
//! `EmulatorRegistry` (cleanup) follow in later tasks.

use std::process::Command;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use glass_core::Deadline;
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
        return root
            .path
            .join("emulator")
            .join(emulator_exe())
            .to_string_lossy()
            .into_owned();
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
            Err(GlassError::Backend(format!(
                "GLASS_AVD={w} not found; AVDs: [{}]",
                names(avds)
            )))
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
    let names = |d: &[Device]| {
        d.iter()
            .map(|x| x.serial.as_str())
            .collect::<Vec<_>>()
            .join(", ")
    };
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
    booted: Arc<Mutex<Vec<Adb>>>,
}

impl EmulatorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record an emulator glass booted, along with the adb client that reaches it.
    ///
    /// Do not resolve one at shutdown instead: `Adb::from_env` reads `GLASS_ADB` and the SDK
    /// layout as they are *then*, so the kill could go to a different adb than the boot did.
    pub fn register(&self, adb: &Adb, serial: String) {
        if let Ok(mut g) = self.booted.lock() {
            g.push(adb.with_serial(serial));
        }
    }

    /// Stop every registered emulator (`adb -s <serial> emu kill`) by `deadline` and clear the
    /// list. Best-effort: a device already gone is fine, and [`Deadline::UNBOUNDED`] off the
    /// teardown path, where each call keeps its own budget.
    ///
    /// `adb emu kill` was observed at 2ms — it acknowledges and lets the VM go down on its own
    /// time — so this is the cheapest step in teardown and the worst one to skip (glass#422).
    pub fn kill_all(&self, deadline: Deadline) {
        let clients = self
            .booted
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        for adb in clients {
            let killed = adb.run_until(["emu", "kill"], deadline);
            glass_core::note_if_skipped("killing a glass-booted emulator", &killed);
        }
    }

    #[cfg(test)]
    pub fn serials(&self) -> Vec<String> {
        self.booted
            .lock()
            .map(|g| {
                g.iter()
                    .filter_map(|a| a.serial().map(str::to_string))
                    .collect()
            })
            .unwrap_or_default()
    }
}

/// How long a booting emulator may take to reach `sys.boot_completed` when
/// `GLASS_EMULATOR_BOOT_TIMEOUT_MS` is unset. Public because a client that waits out a boot has to
/// outlast the budget granted here.
pub const DEFAULT_BOOT_TIMEOUT_MS: u64 = 120_000;

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

    let timeout_ms: u64 = get("GLASS_EMULATOR_BOOT_TIMEOUT_MS")
        .and_then(|s| s.parse().ok())
        .unwrap_or(DEFAULT_BOOT_TIMEOUT_MS);
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
        // Tolerant, like the `getprop` below it: this polls every 500ms inside the boot deadline, so
        // one slow `devices` call is a hiccup, not a reason to abandon a boot — and propagating here
        // would skip the deadline branch that kills the emulator, leaving it holding the AVD lock.
        let online = crate::target::parse_devices(&base.run(["devices"]).unwrap_or_default());
        if let Some(serial) = new_serial(&before, &online) {
            let adb = base.with_serial(serial.clone());
            if adb
                .run(["shell", "getprop", "sys.boot_completed"])
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

/// Budget for `emulator -list-avds`. It reads the AVD directory and prints names — a fast local
/// query — but a broken SDK can wedge the binary, and this runs first on the `glass_start` boot
/// path, so an unbounded call here hangs a launch before anything else happens.
pub(crate) const EMULATOR_LIST_BUDGET: Duration = Duration::from_secs(15);

fn run_emulator_list(bin: &str) -> Result<String> {
    let mut cmd = Command::new(bin);
    cmd.arg("-list-avds");
    let out = glass_core::run_bounded(&mut cmd, EMULATOR_LIST_BUDGET, "emulator:-list-avds")?;
    Ok(String::from_utf8_lossy(&out.stdout).into_owned())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::target::Device;

    fn dev(s: &str) -> Device {
        Device {
            serial: s.into(),
            state: "device".into(),
        }
    }

    #[test]
    fn emulator_bin_prefers_glass_then_sdk_then_path() {
        let env = |k: &str| match k {
            "GLASS_EMULATOR" => Some("/custom/emulator".to_string()),
            _ => None,
        };
        assert_eq!(resolve_emulator_bin(&env, &|_| true), "/custom/emulator");

        // Shares `emulator_exe()` with the code under test, so this pins root selection and the
        // `emulator/` segment but NOT the leaf name — `emulator_exe_carries_the_platform_suffix`
        // does that.
        let want_bin = |root: &str| {
            std::path::PathBuf::from(root)
                .join("emulator")
                .join(emulator_exe())
                .to_string_lossy()
                .into_owned()
        };

        let env = |k: &str| match k {
            "ANDROID_SDK_ROOT" => Some("/sdk".to_string()),
            _ => None,
        };
        assert_eq!(resolve_emulator_bin(&env, &|_| true), want_bin("/sdk"));

        let env = |k: &str| match k {
            "ANDROID_HOME" => Some("/home/sdk".to_string()),
            _ => None,
        };
        assert_eq!(resolve_emulator_bin(&env, &|_| true), want_bin("/home/sdk"));

        let env = |_: &str| None;
        assert_eq!(resolve_emulator_bin(&env, &|_| false), "emulator");
    }

    /// Pins the leaf name directly: every other assertion builds its expectation with
    /// `emulator_exe()` itself, so none of them can falsify the suffix.
    #[test]
    fn emulator_exe_carries_the_platform_suffix() {
        #[cfg(windows)]
        assert_eq!(emulator_exe(), "emulator.exe");
        #[cfg(not(windows))]
        assert_eq!(emulator_exe(), "emulator");
    }

    // One variant per OS: the Windows arm keys off LOCALAPPDATA, so a HOME-only fixture finds
    // nothing and passes while proving nothing.
    #[cfg(not(any(target_os = "windows", target_os = "macos")))]
    #[test]
    fn emulator_from_discovered_default_sdk() {
        // No env at all, but a default SDK location exists on disk.
        let env = |k: &str| match k {
            "HOME" => Some("/home/u".to_string()),
            _ => None,
        };
        let exists = |p: &std::path::Path| p == std::path::Path::new("/home/u/android-sdk");
        assert_eq!(
            resolve_emulator_bin(&env, &exists),
            "/home/u/android-sdk/emulator/emulator"
        );
    }

    #[cfg(target_os = "windows")]
    #[test]
    fn emulator_from_discovered_default_sdk() {
        let env = |k: &str| match k {
            "LOCALAPPDATA" => Some(r"C:\Users\u\AppData\Local".to_string()),
            _ => None,
        };
        let root = r"C:\Users\u\AppData\Local\Android\Sdk";
        let exists = |p: &std::path::Path| p == std::path::Path::new(root);
        assert_eq!(
            resolve_emulator_bin(&env, &exists),
            format!(r"{root}\emulator\emulator.exe")
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn emulator_from_discovered_default_sdk() {
        let env = |k: &str| match k {
            "HOME" => Some("/Users/u".to_string()),
            _ => None,
        };
        let root = "/Users/u/Library/Android/sdk";
        let exists = |p: &std::path::Path| p == std::path::Path::new(root);
        assert_eq!(
            resolve_emulator_bin(&env, &exists),
            format!("{root}/emulator/emulator")
        );
    }

    /// The deadline has to reach here, or a wedge earlier in teardown leaves a whole emulator
    /// running — the cheapest step to make, the worst to skip (glass#422).
    #[test]
    #[cfg(unix)]
    fn a_kill_that_never_answers_gives_up_at_the_shared_deadline() {
        use crate::adb::{Answer, FakeAdb};
        use std::time::{Duration, Instant};

        let fake = FakeAdb::new(&[("*", Answer::Lingers)]);
        let reg = EmulatorRegistry::new();
        reg.register(fake.adb(), "emulator-5554".into());

        let started = Instant::now();
        reg.kill_all(Deadline::at(Instant::now() + Duration::from_millis(300)));

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} on an emulator that never answers — the 10s AdbOp budget was used \
             instead of the deadline",
            started.elapsed()
        );
        assert!(
            fake.called("emu kill"),
            "it still has to ask: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn list_avds_parses_names_only() {
        let out = "INFO | Storing crashdata\nPixel_6\nglass\n";
        assert_eq!(
            parse_list_avds(out),
            vec!["Pixel_6".to_string(), "glass".to_string()]
        );
    }

    #[test]
    fn a_noise_line_is_dropped_for_whichever_reason_gives_it_away() {
        // `list_avds_parses_names_only`'s noise line carries a pipe AND spaces, so it cannot
        // show which rule did the work. Each of these has exactly one.
        assert_eq!(parse_list_avds("INFO|crashdata\nglass\n"), ["glass"]);
        assert_eq!(parse_list_avds("Storing crashdata\nglass\n"), ["glass"]);
        assert_eq!(parse_list_avds("\n\nglass\n"), ["glass"]);
    }

    #[test]
    fn choose_avd_sole_or_named_or_errors() {
        assert_eq!(choose_avd(None, &["glass".into()]).unwrap(), "glass");
        assert_eq!(
            choose_avd(Some("glass"), &["a".into(), "glass".into()]).unwrap(),
            "glass"
        );
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
        assert_eq!(
            new_serial(&before, &after).as_deref(),
            Some("emulator-5556")
        );
        assert_eq!(new_serial(&before, &before), None);
    }

    #[test]
    fn decide_attach_or_boot_or_error() {
        assert_eq!(
            decide(&[dev("emulator-5554")], None, Lifecycle::Auto),
            Action::Attach("emulator-5554".into())
        );
        assert_eq!(decide(&[], None, Lifecycle::Auto), Action::Boot);
        assert!(matches!(
            decide(&[], None, Lifecycle::Attach),
            Action::Error(_)
        ));
        assert!(matches!(
            decide(&[dev("a"), dev("b")], None, Lifecycle::Auto),
            Action::Error(_)
        ));
        assert_eq!(
            decide(&[dev("a"), dev("b")], Some("b"), Lifecycle::Auto),
            Action::Attach("b".into())
        );
        assert!(matches!(
            decide(&[dev("a")], Some("z"), Lifecycle::Auto),
            Action::Error(_)
        ));
    }

    #[test]
    fn lifecycle_from_env() {
        assert_eq!(Lifecycle::from_env(Some("attach")), Lifecycle::Attach);
        assert_eq!(Lifecycle::from_env(Some("AUTO")), Lifecycle::Auto);
        assert_eq!(Lifecycle::from_env(None), Lifecycle::Auto);
    }

    #[test]
    #[cfg(unix)]
    fn registry_records_serials() {
        use crate::adb::{Answer, FakeAdb};
        let fake = FakeAdb::new(&[("*", Answer::Silent)]);
        let r = EmulatorRegistry::new();
        let r2 = r.clone();
        r.register(fake.adb(), "emulator-5554".into());
        r2.register(fake.adb(), "emulator-5556".into());
        assert_eq!(
            r.serials(),
            vec!["emulator-5554".to_string(), "emulator-5556".to_string()]
        );
    }

    #[test]
    #[cfg(unix)]
    fn killing_them_all_stops_each_registered_emulator_and_forgets_it() {
        // The shutdown hook calls this once. An emulator it fails to stop holds its AVD lock,
        // and the next run cannot boot the same one — the failure lands a session later, on
        // work that did nothing wrong.
        use crate::adb::{Answer, FakeAdb};
        let fake = FakeAdb::new(&[("*", Answer::Silent)]);
        let registry = EmulatorRegistry::new();
        registry.register(fake.adb(), "emulator-5554".into());
        registry.register(fake.adb(), "emulator-5556".into());

        registry.kill_all(Deadline::UNBOUNDED);

        assert_eq!(
            fake.calls(),
            ["-s emulator-5554 emu kill", "-s emulator-5556 emu kill",]
        );
        // Cleared, so a second shutdown does not kill a device someone has since started.
        assert!(registry.serials().is_empty());
        registry.kill_all(Deadline::UNBOUNDED);
        assert_eq!(fake.calls().len(), 2);
    }

    #[test]
    #[cfg(unix)]
    fn booting_waits_for_the_device_to_appear_and_finish_booting() {
        use crate::adb::{Answer, FakeAdb};

        // `devices` is asked repeatedly: once for the "before" snapshot, then in the poll loop.
        // The emulator is not there for the first two, so the boot is genuinely waited out
        // rather than answered by the very first look.
        let none = Answer::says("List of devices attached\n\n");
        let up = Answer::says("List of devices attached\nemulator-5554\tdevice\n\n");
        let booted = Answer::says("1\n");
        let fake = FakeAdb::scripted(&[
            ("devices", vec![&none, &none, &up]),
            (
                "-s emulator-5554 shell getprop sys.boot_completed",
                vec![&booted],
            ),
        ]);
        let emulator = fake.alongside("emulator", crate::adb::FAKE_EMULATOR_SCRIPT);
        std::fs::write(emulator.parent().unwrap().join("avds"), "Pixel_6\nglass\n").unwrap();

        let get = |k: &str| match k {
            "GLASS_EMULATOR" => Some(emulator.to_string_lossy().into_owned()),
            "GLASS_AVD" => Some("glass".to_string()),
            _ => None,
        };

        let serial = boot_avd(fake.adb(), &get).expect("the emulator comes up on the third look");
        assert_eq!(serial, "emulator-5554");
        // The AVD it was told to boot, not whichever one came first out of `-list-avds`.
        assert!(
            fake.read("boots").contains("-avd glass"),
            "{:?}",
            fake.read("boots")
        );
    }

    #[test]
    #[cfg(unix)]
    fn booting_gives_up_when_the_device_never_appears() {
        use crate::adb::{Answer, FakeAdb};

        let none = Answer::says("List of devices attached\n\n");
        let fake = FakeAdb::scripted(&[("devices", vec![&none])]);
        let emulator = fake.alongside("emulator", crate::adb::FAKE_EMULATOR_SCRIPT);
        std::fs::write(emulator.parent().unwrap().join("avds"), "glass\n").unwrap();

        let get = |k: &str| match k {
            "GLASS_EMULATOR" => Some(emulator.to_string_lossy().into_owned()),
            "GLASS_EMULATOR_BOOT_TIMEOUT_MS" => Some("600".to_string()),
            _ => None,
        };

        let started = std::time::Instant::now();
        let err = boot_avd(fake.adb(), &get).expect_err("a device that never appears is a failure");
        assert!(
            started.elapsed() < Duration::from_secs(20),
            "waited {:?} — the deadline never ended the wait",
            started.elapsed()
        );
        assert!(err.to_string().contains("sys.boot_completed"), "{err}");
    }

    #[test]
    #[cfg(unix)]
    fn a_named_avd_that_the_sdk_does_not_have_is_refused_before_anything_boots() {
        // The listing is what `run_emulator_list` returns; an empty or invented one would send
        // `emulator -avd` after a device that does not exist.
        use crate::adb::{Answer, FakeAdb};

        let fake = FakeAdb::new(&[("devices", Answer::says("List of devices attached\n\n"))]);
        let emulator = fake.alongside("emulator", crate::adb::FAKE_EMULATOR_SCRIPT);
        std::fs::write(emulator.parent().unwrap().join("avds"), "Pixel_6\nglass\n").unwrap();

        let get = |k: &str| match k {
            "GLASS_EMULATOR" => Some(emulator.to_string_lossy().into_owned()),
            "GLASS_AVD" => Some("not_an_avd".to_string()),
            _ => None,
        };

        let err = boot_avd(fake.adb(), &get).expect_err("an unknown AVD cannot be booted");
        assert!(err.to_string().contains("not_an_avd"), "{err}");
        // It never got as far as spawning one.
        assert_eq!(fake.read("boots"), "");
    }
}
