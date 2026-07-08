//! Parses `xcrun simctl list devices available --json` and decides whether to attach to a
//! running simulator or boot one. Pure functions, no I/O — the command run and its output are
//! wired in by the caller ([`crate::target::SimTarget::from_env`]).

use glass_core::{GlassError, Result};
use serde_json::Value;

/// One entry from `xcrun simctl list devices available --json`.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct SimDevice {
    pub udid: String,
    pub name: String,
    pub state: String,
    pub runtime: String,
    pub is_available: bool,
}

/// Where to attach or what to boot, decided by [`resolve`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Resolve {
    /// Attach to an already-running device by UDID.
    Attach(String),
    /// Boot this device (by UDID) before attaching.
    Boot(String),
    /// No usable device; the message explains why.
    Error(String),
}

/// Parse `xcrun simctl list devices available --json` into a flat device list.
pub fn parse_devices(json: &str) -> Result<Vec<SimDevice>> {
    let v: Value = serde_json::from_str(json)
        .map_err(|e| GlassError::Backend(format!("simctl list JSON parse failed: {e}")))?;
    let map = v
        .get("devices")
        .and_then(Value::as_object)
        .ok_or_else(|| GlassError::Backend("simctl list JSON missing `devices` object".into()))?;
    let mut out = Vec::new();
    for (runtime, arr) in map {
        for d in arr.as_array().into_iter().flatten() {
            let s = |k: &str| {
                d.get(k)
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .to_string()
            };
            out.push(SimDevice {
                udid: s("udid"),
                name: s("name"),
                state: s("state"),
                runtime: runtime.clone(),
                is_available: d
                    .get("isAvailable")
                    .and_then(Value::as_bool)
                    .unwrap_or(false),
            });
        }
    }
    Ok(out)
}

/// Sort key so the newest runtime sorts last: compare the runtime identifier's version
/// digits (iOS-26-5 > iOS-18-0). Falls back to 0 for an unrecognized identifier.
fn runtime_rank(runtime: &str) -> (u32, u32) {
    let tail = runtime.rsplit("iOS-").next().unwrap_or(runtime);
    let mut it = tail.split('-').filter_map(|p| p.parse::<u32>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Decide whether to attach to a running simulator or boot one, given the current
/// device list and the caller's optional UDID/name preference.
///
/// Order: an explicit `want_udid` always wins (attach, trusting the caller). Otherwise
/// prefer a device that is already `Booted` (an iPhone over any other booted device).
/// Otherwise boot the newest available iPhone, or one matching `want_name` if given.
/// If nothing qualifies, return an actionable error.
///
/// `max_by_key` keeps the *last* element on a tie, so both selections below iterate in
/// reverse to make the *first*-listed device win a tie — one consistent policy across the
/// attach and boot branches.
pub fn resolve(devices: &[SimDevice], want_udid: Option<&str>, want_name: Option<&str>) -> Resolve {
    if let Some(u) = want_udid {
        return Resolve::Attach(u.to_string());
    }
    // Tie-break only bites when several iPhones are booted at once: the first-listed wins.
    if let Some(d) = devices
        .iter()
        .rev()
        .filter(|d| d.state == "Booted")
        .max_by_key(|d| (d.name.starts_with("iPhone"), runtime_rank(&d.runtime)))
    {
        return Resolve::Attach(d.udid.clone());
    }
    // A tie between two newest-runtime iPhones resolves to whichever came first in the listing.
    let candidate = devices
        .iter()
        .rev()
        .filter(|d| d.is_available && d.name.starts_with("iPhone"))
        .filter(|d| want_name.is_none_or(|n| d.name == n))
        .max_by_key(|d| runtime_rank(&d.runtime));
    match candidate {
        Some(d) => Resolve::Boot(d.udid.clone()),
        None => Resolve::Error(match want_name {
            Some(n) => format!(
                "no available simulator named {n:?}; run `xcrun simctl list devices available`"
            ),
            None => "no available iPhone simulator found; install one via Xcode".into(),
        }),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const SAMPLE: &str = r#"{
      "devices": {
        "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
          {"udid":"AAA","name":"iPhone 17","state":"Shutdown","isAvailable":true},
          {"udid":"BBB","name":"iPhone 17 Pro","state":"Booted","isAvailable":true}
        ],
        "com.apple.CoreSimulator.SimRuntime.iOS-18-0": [
          {"udid":"CCC","name":"iPhone 15","state":"Shutdown","isAvailable":true}
        ]
      }
    }"#;

    #[test]
    fn parses_devices_with_runtime() {
        let d = parse_devices(SAMPLE).unwrap();
        assert_eq!(d.len(), 3);
        let booted = d.iter().find(|x| x.udid == "BBB").unwrap();
        assert_eq!(booted.state, "Booted");
        assert!(booted.runtime.contains("iOS-26-5"));
    }

    #[test]
    fn explicit_udid_attaches() {
        let d = parse_devices(SAMPLE).unwrap();
        assert_eq!(
            resolve(&d, Some("AAA"), None),
            Resolve::Attach("AAA".into())
        );
    }

    #[test]
    fn prefers_already_booted_iphone() {
        let d = parse_devices(SAMPLE).unwrap();
        assert_eq!(resolve(&d, None, None), Resolve::Attach("BBB".into()));
    }

    #[test]
    fn boots_newest_iphone_when_none_booted() {
        let mut d = parse_devices(SAMPLE).unwrap();
        for x in &mut d {
            x.state = "Shutdown".into();
        }
        // Newest runtime (iOS-26-5) iPhone wins; iPhone 17 (AAA) or 17 Pro (BBB) both qualify —
        // tie broken by list order within the newest runtime, i.e. AAA.
        assert_eq!(resolve(&d, None, None), Resolve::Boot("AAA".into()));
    }

    #[test]
    fn honors_device_name() {
        let mut d = parse_devices(SAMPLE).unwrap();
        for x in &mut d {
            x.state = "Shutdown".into();
        }
        assert_eq!(
            resolve(&d, None, Some("iPhone 17 Pro")),
            Resolve::Boot("BBB".into())
        );
    }

    #[test]
    fn errors_when_no_iphone() {
        let d: Vec<SimDevice> = vec![];
        assert!(matches!(resolve(&d, None, None), Resolve::Error(_)));
    }
}
