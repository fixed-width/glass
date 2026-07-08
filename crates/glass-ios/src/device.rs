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
/// digits (iOS-26-5 > iOS-18-0). Falls back to `(0, 0)` — oldest — for an unrecognized
/// identifier, so a future change to Apple's runtime-id format ranks that device as oldest
/// rather than failing resolution outright.
fn runtime_rank(runtime: &str) -> (u32, u32) {
    let tail = runtime.rsplit("iOS-").next().unwrap_or(runtime);
    let mut it = tail.split('-').filter_map(|p| p.parse::<u32>().ok());
    (it.next().unwrap_or(0), it.next().unwrap_or(0))
}

/// Is `runtime` an iOS (as opposed to watchOS/tvOS/visionOS) simulator runtime? The runtime
/// identifier looks like `com.apple.CoreSimulator.SimRuntime.iOS-26-5`; the sibling platforms'
/// identifiers (`watchOS-...`, `tvOS-...`, `xrOS-...`) do not contain the substring `iOS`.
fn is_ios_family(runtime: &str) -> bool {
    runtime.contains("iOS")
}

/// Decide whether to attach to a running simulator or boot one, given the current
/// device list and the caller's optional UDID/name preference.
///
/// Order: an explicit `want_udid` always wins (attach, trusting the caller). Otherwise
/// prefer a device that is already `Booted`, restricted to the iOS family (so a booted
/// watchOS/tvOS/visionOS simulator is never picked up), with an iPhone preferred over any
/// other iOS-family device on a tie. Otherwise boot the newest available iOS-family device
/// (again preferring an iPhone), or one matching `want_name` if given — `want_name` accepts
/// any iOS-family device, e.g. an iPad, not just an iPhone. If nothing qualifies, return an
/// actionable error.
///
/// `max_by_key` keeps the *last* element on a tie, so both selections below iterate in
/// reverse to make the *first*-listed device win a tie — one consistent policy across the
/// attach and boot branches.
pub fn resolve(devices: &[SimDevice], want_udid: Option<&str>, want_name: Option<&str>) -> Resolve {
    if let Some(u) = want_udid {
        return Resolve::Attach(u.to_string());
    }
    // Tie-break only bites when several iOS-family devices are booted at once: the
    // first-listed wins, with an iPhone preferred over any other iOS-family device.
    if let Some(d) = devices
        .iter()
        .rev()
        .filter(|d| d.state == "Booted" && is_ios_family(&d.runtime))
        .max_by_key(|d| (d.name.starts_with("iPhone"), runtime_rank(&d.runtime)))
    {
        return Resolve::Attach(d.udid.clone());
    }
    // A tie between two newest-runtime iOS-family devices resolves to whichever came first
    // in the listing, with an iPhone preferred over any other iOS-family device.
    let candidate = devices
        .iter()
        .rev()
        .filter(|d| d.is_available && is_ios_family(&d.runtime))
        .filter(|d| want_name.is_none_or(|n| d.name == n))
        .max_by_key(|d| (d.name.starts_with("iPhone"), runtime_rank(&d.runtime)));
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

    #[test]
    fn attach_prefers_first_listed_among_equal_rank_booted_iphones() {
        let json = r#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
              {"udid":"AAA","name":"iPhone 17","state":"Booted","isAvailable":true},
              {"udid":"BBB","name":"iPhone 17 Pro","state":"Booted","isAvailable":true}
            ]
          }
        }"#;
        let d = parse_devices(json).unwrap();
        assert_eq!(resolve(&d, None, None), Resolve::Attach("AAA".into()));
    }

    #[test]
    fn booted_watch_is_not_attached_iphone_boots_instead() {
        let json = r#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.watchOS-10-4": [
              {"udid":"WWW","name":"Apple Watch Series 9","state":"Booted","isAvailable":true}
            ],
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
              {"udid":"AAA","name":"iPhone 17","state":"Shutdown","isAvailable":true}
            ]
          }
        }"#;
        let d = parse_devices(json).unwrap();
        assert_eq!(resolve(&d, None, None), Resolve::Boot("AAA".into()));
    }

    #[test]
    fn booted_watch_only_errors_when_no_iphone_available() {
        let json = r#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.watchOS-10-4": [
              {"udid":"WWW","name":"Apple Watch Series 9","state":"Booted","isAvailable":true}
            ]
          }
        }"#;
        let d = parse_devices(json).unwrap();
        assert!(matches!(resolve(&d, None, None), Resolve::Error(_)));
    }

    #[test]
    fn explicit_device_name_boots_a_named_ipad() {
        let json = r#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
              {"udid":"III","name":"iPad Pro 13-inch","state":"Shutdown","isAvailable":true}
            ]
          }
        }"#;
        let d = parse_devices(json).unwrap();
        assert_eq!(
            resolve(&d, None, Some("iPad Pro 13-inch")),
            Resolve::Boot("III".into())
        );
    }

    #[test]
    fn booted_ipad_is_attached_when_no_iphone_booted() {
        let json = r#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
              {"udid":"III","name":"iPad Pro 13-inch","state":"Booted","isAvailable":true}
            ]
          }
        }"#;
        let d = parse_devices(json).unwrap();
        assert_eq!(resolve(&d, None, None), Resolve::Attach("III".into()));
    }

    #[test]
    fn unavailable_iphone_with_nothing_else_errors() {
        let json = r#"{
          "devices": {
            "com.apple.CoreSimulator.SimRuntime.iOS-26-5": [
              {"udid":"AAA","name":"iPhone 17","state":"Shutdown","isAvailable":false}
            ]
          }
        }"#;
        let d = parse_devices(json).unwrap();
        assert!(matches!(resolve(&d, None, None), Resolve::Error(_)));
    }

    #[test]
    fn parse_devices_rejects_non_json() {
        assert!(parse_devices("not json").is_err());
    }

    #[test]
    fn parse_devices_rejects_json_missing_devices_key() {
        assert!(parse_devices(r#"{"foo":1}"#).is_err());
    }
}
