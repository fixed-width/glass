//! Resolves an iOS Simulator to drive: which UDID, and whether it needs booting.
//! Delegates the pure attach-or-boot decision to [`crate::device::resolve`], then does the
//! one piece of I/O this crate performs proactively — running `bootstatus -b` to boot and
//! wait for a device this crate chose to start.

use std::sync::{Arc, Mutex};

use glass_core::{GlassError, Result};

use crate::device::{parse_devices, resolve, Resolve};
use crate::simctl::Simctl;

/// UDIDs of simulators glass booted itself, so they can be shut down explicitly rather than
/// left running. Cloneable + `Send` (shared `Arc`); glass-mcp threads one clone into the
/// platform factory (to register boots) and another into the `Glass` shutdown hook (to shut
/// them down). No `Drop` impl: a clone going out of scope (e.g. the factory clone, dropped
/// after each `glass_start`) must not shut down simulators that are still in use — only an
/// explicit `shutdown_all` call does that.
#[derive(Clone, Default)]
pub struct SimulatorRegistry {
    booted: Arc<Mutex<Vec<String>>>,
}

impl SimulatorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Record a simulator UDID glass booted.
    pub fn register(&self, udid: String) {
        if let Ok(mut g) = self.booted.lock() {
            g.push(udid);
        }
    }

    /// Shut down every registered simulator (`xcrun simctl shutdown <udid>`) and clear the
    /// list. Best-effort: a simulator already stopped, or a host with no simulator support at
    /// all, is fine — each shutdown's result is discarded.
    pub fn shutdown_all(&self) {
        let udids = self
            .booted
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        let s = Simctl::new();
        for udid in udids {
            let _ = s.run(&["shutdown", &udid]);
        }
    }

    #[cfg(test)]
    pub fn udids(&self) -> Vec<String> {
        self.booted.lock().map(|g| g.clone()).unwrap_or_default()
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
    /// needed, and registering it in `reg` — for shutdown by a later `reg.shutdown_all()` call
    /// — unless `GLASS_SIMULATOR_KEEP` is set.
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
        Ok(Self { simctl: base, udid })
    }

    /// The `Simctl` client bound to the resolved device.
    pub fn simctl(&self) -> &Simctl {
        &self.simctl
    }

    /// The resolved device's UDID.
    pub fn udid(&self) -> &str {
        &self.udid
    }

    /// A `SimTarget` for `IosPlatform` unit tests that never touch a real simulator (a
    /// `NoActiveSession`/state-machine guard fires before `target` is used).
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self {
            simctl: Simctl::new(),
            udid: "test-udid".to_string(),
        }
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

    #[test]
    fn registry_records_udids_across_clones() {
        let r = SimulatorRegistry::new();
        let r2 = r.clone();
        r.register("AAA".into());
        r2.register("BBB".into());
        assert_eq!(r.udids(), vec!["AAA".to_string(), "BBB".to_string()]);
    }

    #[test]
    fn shutdown_all_clears_the_registry() {
        let r = SimulatorRegistry::new();
        r.register("AAA".into());
        // Best-effort: `xcrun simctl shutdown` may fail in a CI sandbox with no real
        // simulator, but the registry is cleared regardless.
        r.shutdown_all();
        assert!(r.udids().is_empty());
    }
}
