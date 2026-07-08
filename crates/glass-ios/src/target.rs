//! Resolves an iOS Simulator to drive: which UDID, and whether it needs booting.
//! Delegates the pure attach-or-boot decision to [`crate::device::resolve`], then does the
//! one piece of I/O this crate performs proactively — running `bootstatus -b` to boot and
//! wait for a device this crate chose to start.

use std::sync::Mutex;

use glass_core::{GlassError, Result};

use crate::device::{parse_devices, resolve, Resolve};
use crate::simctl::Simctl;

/// Shuts down simulators glass booted, on drop, unless the user asked to keep them.
#[derive(Default)]
pub struct SimulatorRegistry {
    booted: Mutex<Vec<String>>,
}

impl SimulatorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a simulator UDID glass booted, so it gets shut down on drop.
    pub fn register(&self, udid: String) {
        // Recover a poisoned lock rather than panic: the vec is a plain list of UDIDs and a
        // prior panic can't have left it in a state that matters for a best-effort sweep.
        self.booted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .push(udid);
    }
}

impl Drop for SimulatorRegistry {
    fn drop(&mut self) {
        let s = Simctl::new();
        // Never panic inside drop: a panic here during unwind would abort the process. Recover
        // a poisoned lock and shut down whatever UDIDs we recorded, ignoring per-command errors.
        for udid in self
            .booted
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .drain(..)
        {
            let _ = s.run(&["shutdown", &udid]);
        }
    }
}

/// Read the UDID/name/keep preferences from `GLASS_IOS_UDID`, `GLASS_IOS_DEVICE`, and
/// `GLASS_SIMULATOR_KEEP` via the given getter (env lookup is injected so this stays pure
/// and testable without touching real process env).
#[cfg_attr(not(test), allow(dead_code))]
pub fn wants(get: &dyn Fn(&str) -> Option<String>) -> (Option<String>, Option<String>, bool) {
    let udid = get("GLASS_IOS_UDID").filter(|s| !s.is_empty());
    let name = get("GLASS_IOS_DEVICE").filter(|s| !s.is_empty());
    let keep = get("GLASS_SIMULATOR_KEEP")
        .filter(|s| !s.is_empty())
        .is_some();
    (udid, name, keep)
}

/// A resolved, booted iOS Simulator: its UDID and a `Simctl` bound to it.
pub struct SimTarget {
    simctl: Simctl,
    udid: String,
}

impl SimTarget {
    /// Resolve a device from env/list (attach if one's already running or named, else boot
    /// the newest available iPhone), booting and waiting for it via `bootstatus -b` when
    /// needed, and registering it in `reg` for shutdown-on-drop unless `GLASS_SIMULATOR_KEEP`
    /// is set.
    pub fn from_env(reg: &SimulatorRegistry) -> Result<Self> {
        let get = |k: &str| std::env::var(k).ok();
        let (want_udid, want_name, keep) = wants(&get);

        let base = Simctl::new();
        let list = base.run(&["list", "devices", "available", "--json"])?;
        let devices = parse_devices(&list)?;
        let udid = match resolve(&devices, want_udid.as_deref(), want_name.as_deref()) {
            Resolve::Attach(u) => u,
            Resolve::Error(msg) => return Err(GlassError::Backend(msg)),
            Resolve::Boot(u) => {
                // `bootstatus -b` boots if needed and blocks until fully booted.
                base.run(&["bootstatus", &u, "-b"])?;
                if !keep {
                    reg.register(u.clone());
                }
                u
            }
        };
        Ok(Self {
            simctl: base.bind(udid.clone()),
            udid,
        })
    }

    /// The `Simctl` client bound to the resolved device.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn simctl(&self) -> &Simctl {
        &self.simctl
    }

    /// The resolved device's UDID.
    #[cfg_attr(not(test), allow(dead_code))]
    pub fn udid(&self) -> &str {
        &self.udid
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;

    fn getter(m: HashMap<&'static str, &'static str>) -> impl Fn(&str) -> Option<String> {
        move |k: &str| m.get(k).map(|s| s.to_string())
    }

    #[test]
    fn wants_reads_all_three_env_vars() {
        let g = getter(HashMap::from([
            ("GLASS_IOS_UDID", "AAA"),
            ("GLASS_IOS_DEVICE", "iPhone 17"),
            ("GLASS_SIMULATOR_KEEP", "1"),
        ]));
        assert_eq!(
            wants(&g),
            (Some("AAA".into()), Some("iPhone 17".into()), true)
        );
    }

    #[test]
    fn wants_empty_keep_is_false() {
        let g = getter(HashMap::from([("GLASS_SIMULATOR_KEEP", "")]));
        assert_eq!(wants(&g), (None, None, false));
    }
}
