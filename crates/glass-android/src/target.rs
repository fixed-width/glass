use glass_core::{GlassError, Result};

use crate::adb::Adb;
use crate::avd::{boot_avd, decide, Action, EmulatorRegistry, Lifecycle};

/// One row of `adb devices`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Device {
    pub serial: String,
    pub state: String,
}

/// Resolves which device adb talks to (and, later, owns its lifecycle).
pub trait AdbTarget {
    /// An `Adb` client bound to the resolved serial.
    fn adb(&self) -> &Adb;
}

/// Attaches to an already-running emulator (P0). Serial from `GLASS_ANDROID_SERIAL`,
/// else the sole online device.
pub struct AttachedDevice {
    adb: Adb,
}

impl AttachedDevice {
    /// Resolve the target: list devices, pick the serial, verify it has finished booting.
    pub fn resolve(base: Adb, serial_env: Option<&str>) -> Result<Self> {
        let listing = base.run(["devices"])?;
        let online: Vec<Device> =
            parse_devices(&listing).into_iter().filter(|d| d.state == "device").collect();
        let serial = choose_serial(serial_env, &online)?;
        let adb = base.with_serial(serial);
        ensure_booted(&adb)?;
        Ok(Self { adb })
    }

    /// Wrap an already-serial-bound adb client.
    pub fn from_adb(adb: Adb) -> Self {
        Self { adb }
    }
}

/// Resolve an adb target: attach to an online device, or (lifecycle `auto`) boot the
/// configured AVD, register it for cleanup, and attach. Attach-preferred.
pub fn resolve(base: Adb, registry: &EmulatorRegistry) -> Result<AttachedDevice> {
    let get = |k: &str| std::env::var(k).ok();
    let online: Vec<Device> = parse_devices(&base.run(["devices"])?)
        .into_iter()
        .filter(|d| d.state == "device")
        .collect();
    let lifecycle = Lifecycle::from_env(get("GLASS_ANDROID_LIFECYCLE").as_deref());
    let serial = match decide(&online, get("GLASS_ANDROID_SERIAL").as_deref(), lifecycle) {
        Action::Attach(s) => s,
        Action::Error(msg) => return Err(GlassError::Backend(msg)),
        Action::Boot => {
            let s = boot_avd(&base, &get)?;
            if get("GLASS_EMULATOR_KEEP").filter(|v| !v.is_empty()).is_none() {
                registry.register(s.clone());
            }
            s
        }
    };
    let adb = base.with_serial(serial);
    ensure_booted(&adb)?;
    Ok(AttachedDevice::from_adb(adb))
}

impl AdbTarget for AttachedDevice {
    fn adb(&self) -> &Adb {
        &self.adb
    }
}

/// Parse `adb devices` into rows, skipping the header and daemon `*` noise.
pub fn parse_devices(output: &str) -> Vec<Device> {
    output
        .lines()
        .map(str::trim)
        .filter(|l| !l.is_empty() && !l.starts_with("List of devices") && !l.starts_with('*'))
        .filter_map(|l| {
            let mut it = l.split_whitespace();
            let serial = it.next()?.to_string();
            let state = it.next().unwrap_or_default().to_string();
            Some(Device { serial, state })
        })
        .collect()
}

/// Pure device-selection policy.
pub fn choose_serial(serial_env: Option<&str>, online: &[Device]) -> Result<String> {
    let names = |d: &[Device]| {
        d.iter().map(|x| x.serial.as_str()).collect::<Vec<_>>().join(", ")
    };
    if let Some(want) = serial_env.filter(|s| !s.is_empty()) {
        return if online.iter().any(|d| d.serial == want) {
            Ok(want.to_string())
        } else {
            Err(GlassError::Backend(format!(
                "GLASS_ANDROID_SERIAL={want} is not an online device; online: [{}]",
                names(online)
            )))
        };
    }
    match online {
        [] => Err(GlassError::Backend(
            "no online adb devices; start an emulator (e.g. `emulator -avd <name>`) then check `adb devices`".into(),
        )),
        [one] => Ok(one.serial.clone()),
        many => Err(GlassError::Backend(format!(
            "{} online devices; set GLASS_ANDROID_SERIAL to one of: [{}]",
            many.len(),
            names(many)
        ))),
    }
}

fn ensure_booted(adb: &Adb) -> Result<()> {
    let out = adb.run(["shell", "getprop", "sys.boot_completed"])?;
    if out.trim() == "1" {
        Ok(())
    } else {
        Err(GlassError::Backend(
            "device has not finished booting (sys.boot_completed != 1); wait for the emulator's home screen".into(),
        ))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::GlassError;

    const LISTING: &str = "List of devices attached\n\
                           emulator-5554\tdevice\n\
                           emulator-5556\toffline\n";

    #[test]
    fn parse_devices_skips_header_and_keeps_state() {
        let d = parse_devices(LISTING);
        assert_eq!(d.len(), 2);
        assert_eq!(d[0], Device { serial: "emulator-5554".into(), state: "device".into() });
        assert_eq!(d[1].state, "offline");
    }

    #[test]
    fn parse_devices_ignores_daemon_noise() {
        let out = "* daemon not running; starting now at tcp:5037\n\
                   * daemon started successfully\n\
                   List of devices attached\n\
                   emulator-5554\tdevice\n";
        assert_eq!(parse_devices(out).len(), 1);
    }

    #[test]
    fn choose_serial_picks_the_only_online_device() {
        let online = vec![Device { serial: "emulator-5554".into(), state: "device".into() }];
        assert_eq!(choose_serial(None, &online).unwrap(), "emulator-5554");
    }

    #[test]
    fn choose_serial_errors_when_none_online() {
        let err = choose_serial(None, &[]).unwrap_err();
        assert!(matches!(err, GlassError::Backend(_)));
        assert!(err.to_string().contains("no online adb devices"));
    }

    #[test]
    fn choose_serial_requires_disambiguation_when_many() {
        let online = vec![
            Device { serial: "emulator-5554".into(), state: "device".into() },
            Device { serial: "emulator-5556".into(), state: "device".into() },
        ];
        let err = choose_serial(None, &online).unwrap_err();
        assert!(err.to_string().contains("GLASS_ANDROID_SERIAL"));
    }

    #[test]
    fn choose_serial_honors_env_when_present_and_online() {
        let online = vec![
            Device { serial: "emulator-5554".into(), state: "device".into() },
            Device { serial: "emulator-5556".into(), state: "device".into() },
        ];
        assert_eq!(choose_serial(Some("emulator-5556"), &online).unwrap(), "emulator-5556");
    }

    #[test]
    fn choose_serial_rejects_env_not_online() {
        let online = vec![Device { serial: "emulator-5554".into(), state: "device".into() }];
        let err = choose_serial(Some("emulator-9999"), &online).unwrap_err();
        assert!(err.to_string().contains("emulator-9999"));
    }
}
