//! Backend capability reporting for the `glass_capabilities` MCP tool.
//!
//! A projection over existing environment signals, not a new prober: each compiled-in
//! backend crate's `capabilities()` supplies its live map; a valid backend name not
//! compiled into this binary is reported `NotOnThisHost`. Dispatch is cfg-gated exactly
//! like `make_platform`/`doctor` (android is always compiled; the rest per host OS).

use glass_core::capability::CapabilityMap;

/// A backend's capability report on this host.
pub enum CapabilityReport {
    /// Compiled into this binary — here is its live capability map.
    Available(CapabilityMap),
    /// A valid backend name, but not compiled into this binary (cannot be probed here).
    NotOnThisHost,
}

/// Dispatch to the compiled-in backend's `capabilities()`.
///
/// `None` ⇒ `backend` is not a known [`crate::BACKENDS`] name. `Some(NotOnThisHost)` ⇒ a
/// known backend name that isn't compiled into this binary. `Some(Available(_))` ⇒
/// compiled into this binary, with its live capability map.
pub fn capabilities_for(backend: &str) -> Option<CapabilityReport> {
    if !crate::BACKENDS.contains(&backend) {
        return None;
    }
    // android is always compiled in (it shells out to adb; host-OS-agnostic).
    if backend == "android" {
        return Some(CapabilityReport::Available(glass_android::capabilities()));
    }
    #[cfg(target_os = "linux")]
    {
        match backend {
            "x11" => return Some(CapabilityReport::Available(glass_x11::capabilities())),
            "wayland" => return Some(CapabilityReport::Available(glass_wayland::capabilities())),
            _ => {}
        }
    }
    #[cfg(windows)]
    {
        if backend == "windows" {
            return Some(CapabilityReport::Available(glass_windows::capabilities()));
        }
    }
    #[cfg(target_os = "macos")]
    {
        match backend {
            "macos" => return Some(CapabilityReport::Available(glass_macos::capabilities())),
            "ios" => return Some(CapabilityReport::Available(glass_ios::capabilities())),
            _ => {}
        }
    }
    Some(CapabilityReport::NotOnThisHost)
}

/// Resolve `backend` (None => the default backend) and render the report as JSON text.
/// `Err` names the valid backends when `backend` is an unrecognized value.
pub fn render_json(backend: Option<&str>) -> Result<String, String> {
    let name: &'static str = match backend {
        Some(v) => crate::BACKENDS
            .iter()
            .find(|b| v.eq_ignore_ascii_case(b))
            .copied()
            .ok_or_else(|| {
                format!(
                    "unknown backend {v:?}; use one of: {}",
                    crate::BACKENDS.join(", ")
                )
            })?,
        None => crate::default_backend(std::env::var("GLASS_BACKEND").ok().as_deref()),
    };
    let report =
        capabilities_for(name).expect("render_json resolved name to a canonical BACKENDS entry");
    let json = match report {
        CapabilityReport::Available(map) => serde_json::json!({
            "backend": name,
            "available": true,
            "capabilities": map,
        }),
        CapabilityReport::NotOnThisHost => serde_json::json!({
            "backend": name,
            "available": false,
            "reason": format!("not in this glass build (host: {})", std::env::consts::OS),
        }),
    };
    Ok(serde_json::to_string(&json).expect("capability report serializes"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn available_only_for_compiled_in_backends() {
        // android is always compiled in (host-OS-agnostic).
        assert!(matches!(
            capabilities_for("android"),
            Some(CapabilityReport::Available(_))
        ));

        #[cfg(target_os = "linux")]
        {
            assert!(matches!(
                capabilities_for("x11"),
                Some(CapabilityReport::Available(_))
            ));
            assert!(matches!(
                capabilities_for("wayland"),
                Some(CapabilityReport::Available(_))
            ));
            for b in ["windows", "macos", "ios"] {
                assert!(
                    matches!(capabilities_for(b), Some(CapabilityReport::NotOnThisHost)),
                    "{b}"
                );
            }
        }
        #[cfg(windows)]
        {
            assert!(matches!(
                capabilities_for("windows"),
                Some(CapabilityReport::Available(_))
            ));
            for b in ["x11", "wayland", "macos", "ios"] {
                assert!(
                    matches!(capabilities_for(b), Some(CapabilityReport::NotOnThisHost)),
                    "{b}"
                );
            }
        }
        #[cfg(target_os = "macos")]
        {
            for b in ["macos", "ios"] {
                assert!(
                    matches!(capabilities_for(b), Some(CapabilityReport::Available(_))),
                    "{b}"
                );
            }
            for b in ["x11", "wayland", "windows"] {
                assert!(
                    matches!(capabilities_for(b), Some(CapabilityReport::NotOnThisHost)),
                    "{b}"
                );
            }
        }

        assert!(capabilities_for("nope").is_none());

        // Every canonical name resolves without panicking.
        for b in crate::BACKENDS {
            let _ = capabilities_for(b);
        }
    }

    #[test]
    fn render_json_shapes_available_and_canonicalizes() {
        let v: serde_json::Value =
            serde_json::from_str(&render_json(Some("ANDROID")).unwrap()).unwrap();
        assert_eq!(v["backend"], "android"); // canonicalized, case-insensitive input
        assert_eq!(v["available"], true);
        assert!(v["capabilities"]["multi_touch"]["status"].is_string());
        assert!(v.get("reason").is_none());
    }

    #[test]
    fn render_json_errors_on_unknown_backend() {
        let e = render_json(Some("nope")).unwrap_err();
        assert!(e.contains("nope"));
        assert!(e.contains("x11")); // lists the valid backends
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn render_json_not_on_this_host_shape() {
        let v: serde_json::Value =
            serde_json::from_str(&render_json(Some("ios")).unwrap()).unwrap();
        assert_eq!(v["backend"], "ios");
        assert_eq!(v["available"], false);
        assert!(v["reason"].as_str().unwrap().contains("host: linux"));
        assert!(v.get("capabilities").is_none());
    }

    #[test]
    fn render_json_none_resolves_to_the_default_backend() {
        let default = crate::default_backend(std::env::var("GLASS_BACKEND").ok().as_deref());
        // Omitting `backend` is identical to naming the resolved default.
        assert_eq!(
            render_json(None).unwrap(),
            render_json(Some(default)).unwrap()
        );
        let v: serde_json::Value = serde_json::from_str(&render_json(None).unwrap()).unwrap();
        assert_eq!(v["backend"], default);
    }
}
