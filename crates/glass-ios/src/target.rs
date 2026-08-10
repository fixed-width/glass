//! Resolves an iOS Simulator to drive: which UDID, and whether it needs booting.
//! Delegates the pure attach-or-boot decision to [`crate::device::resolve`], then does the
//! one piece of I/O this crate performs proactively — running `bootstatus -b` to boot and
//! wait for a device this crate chose to start.

use std::process::{Command, Stdio};
use std::sync::{Arc, Mutex};

use glass_core::{GlassError, Result};

use crate::device::{Resolve, parse_devices, resolve};
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
    /// The runner `shutdown_all` uses, held so a test can point it at a stand-in — one built
    /// inside `shutdown_all` put a real `xcrun simctl shutdown` in a unit test.
    simctl: Simctl,
}

impl SimulatorRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    #[cfg(test)]
    fn at(program: &str) -> Self {
        Self {
            booted: Arc::default(),
            simctl: Simctl::at(program),
        }
    }

    /// Record a simulator UDID glass booted.
    pub fn register(&self, udid: String) {
        if let Ok(mut g) = self.booted.lock() {
            g.push(udid);
        }
    }

    /// Ask every registered simulator to shut down (`xcrun simctl shutdown <udid>`) and clear the
    /// list, without waiting for CoreSimulator to finish.
    ///
    /// **Measured at 3.26-3.40s per device** on an M4 mini shutting down a freshly booted, idle
    /// simulator — over `glass_core::TEARDOWN_BUDGET` (3s) by itself, before stopping the app is
    /// counted. This is the process-exit path's last step, so waiting it out meant every exit that
    /// had booted a simulator overran the budget and was abandoned anyway (glass#427).
    ///
    /// So it is a request, not a wait: the work happens in CoreSimulator, which outlives this
    /// process and finishes on its own. Android's counterpart already behaves this way —
    /// `adb emu kill` returns in 2ms and lets the VM go down after it.
    ///
    /// Best-effort in both directions: a simulator already stopped, or a host with no simulator
    /// support at all, is fine. What it costs is the report — nothing here can know whether the
    /// shutdown succeeded, where waiting would have.
    pub fn shutdown_all(&self) {
        let udids = self
            .booted
            .lock()
            .map(|mut g| std::mem::take(&mut *g))
            .unwrap_or_default();
        for udid in udids {
            let mut cmd = Command::new(self.simctl.program());
            cmd.args(self.simctl.full_args(&["shutdown", &udid]));
            cmd.stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null());
            // Reaped on a thread of its own so a caller that does NOT exit — a test, or a future
            // host that keeps running — leaves no zombie behind. The thread outliving this
            // process is the point, not an oversight: the shutdown it is waiting on does too.
            match cmd.spawn() {
                Ok(mut child) => {
                    std::thread::spawn(move || {
                        let _ = child.wait();
                    });
                }
                Err(e) => eprintln!("glass-ios: could not ask {udid} to shut down: {e}"),
            }
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

    /// A `SimTarget` for `IosPlatform` unit tests, whose `simctl` runs `true` instead of
    /// `xcrun`. Both ends of a fixture's life reach `simctl` — the log stream it spawns as it is
    /// built, the `terminate` it issues as it drops — so a real one sends both into
    /// CoreSimulator against a bogus UDID.
    #[cfg(test)]
    pub(crate) fn for_test() -> Self {
        Self::for_test_at("true")
    }

    /// [`SimTarget::for_test`] against a named stand-in — a `FakeSimctl`, for a test that
    /// asserts which calls were made rather than only that none escaped.
    #[cfg(test)]
    pub(crate) fn for_test_at(program: impl Into<String>) -> Self {
        Self {
            simctl: Simctl::at(program),
            udid: "test-udid".to_string(),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashMap;
    use std::time::{Duration, Instant};

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

    /// Pinned because losing it is invisible here: with no `xcrun` on the host every escaped
    /// call fails silently, while on a macOS runner the ~20 fixtures built from this reach
    /// CoreSimulator with a bogus UDID, twice each.
    #[test]
    fn the_test_target_never_names_the_real_tool() {
        assert_ne!(SimTarget::for_test().simctl().program(), "xcrun");
    }

    /// Waited for, not read straight away: `shutdown_all` spawns and returns, so the argv lands
    /// after it.
    #[test]
    fn shutdown_all_shuts_down_what_it_recorded_and_clears_the_registry() {
        let fake = crate::simctl::FakeSimctl::new();
        let r = SimulatorRegistry::at(&fake.program());
        r.register("AAA".into());

        r.shutdown_all();

        assert!(
            fake.wait_called("shutdown AAA", Duration::from_secs(5)),
            "{:?}",
            fake.calls()
        );
        assert!(r.udids().is_empty());
    }

    /// Shutdown is best-effort — a simulator already stopped answers non-zero — but the registry
    /// must still be cleared, or a second `shutdown_all` retries a device that is already gone.
    #[test]
    fn a_shutdown_that_fails_still_clears_the_registry() {
        let fake = crate::simctl::FakeSimctl::new();
        fake.fails("shutdown");
        let r = SimulatorRegistry::at(&fake.program());
        r.register("AAA".into());

        r.shutdown_all();

        assert!(
            fake.wait_called("shutdown AAA", Duration::from_secs(5)),
            "{:?}",
            fake.calls()
        );
        assert!(r.udids().is_empty());
    }

    /// The measured defect (glass#427): a real `simctl shutdown` takes ~3.3s, which is over the
    /// whole teardown budget on its own, so process exit must not wait for it.
    #[test]
    fn shutdown_all_returns_without_waiting_for_the_simulator_to_go_down() {
        let fake = crate::simctl::FakeSimctl::new();
        fake.slow("shutdown", 3);
        let r = SimulatorRegistry::at(&fake.program());
        r.register("AAA".into());

        let started = Instant::now();
        r.shutdown_all();
        let waited = started.elapsed();

        assert!(
            waited < Duration::from_secs(1),
            "waited {waited:?} for a shutdown that takes 3s — process exit is blocked on \
             CoreSimulator"
        );
        // And it really did ask, rather than returning fast by doing nothing.
        assert!(
            fake.wait_called("shutdown AAA", Duration::from_secs(10)),
            "{:?}",
            fake.calls()
        );
    }
}
