//! Backend capability reporting for the `glass_capabilities` MCP tool.
//!
//! A projection over existing environment signals, not a new prober: each compiled-in
//! backend crate's `capabilities()` supplies its live map; a valid backend name not
//! compiled into this binary is reported `NotOnThisHost`. Dispatch is cfg-gated exactly
//! like `make_platform`/`doctor` (android is always compiled; the rest per host OS).

use glass_core::capability::{CapabilityMap, CapabilityStatus};

/// Operation → the registered MCP tools it gates. One source; the `server` test
/// `every_mapped_tool_is_a_registered_tool` pins it to the registry (that test lives in
/// `server.rs`, the only module that can reach `tool_router()`).
pub(crate) const OPERATION_TOOLS: &[(&str, &[&str])] = &[
    (
        "input",
        &[
            "glass_type",
            "glass_click",
            "glass_key",
            "glass_drag",
            "glass_scroll",
            "glass_move",
            "glass_do",
        ],
    ),
    ("multi_touch", &["glass_gesture"]),
    ("clipboard", &["glass_clipboard_get", "glass_clipboard_set"]),
    (
        "accessibility",
        &[
            "glass_a11y_snapshot",
            "glass_a11y_marks",
            "glass_click_element",
            "glass_set_value",
            "glass_wait_for_element",
            "glass_scroll_to_element",
        ],
    ),
    ("window_move_resize", &["glass_window"]),
];

fn tools_for(op: &str) -> &'static [&'static str] {
    OPERATION_TOOLS
        .iter()
        .find(|(name, _)| *name == op)
        .map(|(_, t)| *t)
        .unwrap_or_else(|| panic!("no OPERATION_TOOLS entry for operation {op:?}"))
}

/// One rendered operation entry: the live status/note (via `CapabilityStatus`'s own
/// serialization — single source for the `note`-omit policy) + the tools it gates.
fn entry(op: &str, st: &CapabilityStatus) -> serde_json::Value {
    let mut v = serde_json::to_value(st).expect("CapabilityStatus serializes");
    v.as_object_mut()
        .expect("CapabilityStatus serializes to a JSON object")
        .insert(
            "tools".into(),
            serde_json::to_value(tools_for(op)).expect("tool list serializes"),
        );
    v
}

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
    render_json_resolved(backend, std::env::var("GLASS_BACKEND").ok().as_deref())
}

/// Pure core of [`render_json`], with the `GLASS_BACKEND` value passed in (`env`) so the
/// default-resolution branch is testable without mutating the process environment. `env` is
/// consulted only when `backend` is `None`.
fn render_json_resolved(backend: Option<&str>, env: Option<&str>) -> Result<String, String> {
    // A `None` caller with a set-but-unrecognized GLASS_BACKEND resolves to the host default;
    // surface that in the report the way `boot`/`doctor` do, rather than reporting the wrong
    // backend's capabilities with no indication the requested one was dropped.
    let mut warning: Option<String> = None;
    let name: &'static str = match backend {
        Some(v) => crate::recognized_backend(v).ok_or_else(|| {
            format!(
                "unknown backend {v:?}; use one of: {}",
                crate::BACKENDS.join(", ")
            )
        })?,
        None => {
            if crate::backend_env_unrecognized(env) {
                warning = Some(format!(
                    "GLASS_BACKEND={:?} is not a recognized backend; reporting {} instead",
                    env.unwrap_or_default(),
                    crate::default_backend(env),
                ));
            }
            crate::default_backend(env)
        }
    };
    let report =
        capabilities_for(name).expect("render_json resolved name to a canonical BACKENDS entry");
    let mut json = match report {
        CapabilityReport::Available(map) => {
            let caps: serde_json::Map<String, serde_json::Value> = map
                .entries()
                .iter()
                .map(|(op, st)| ((*op).to_string(), entry(op, st)))
                .collect();
            serde_json::json!({
                "backend": name,
                "available": true,
                "capabilities": caps,
            })
        }
        CapabilityReport::NotOnThisHost => serde_json::json!({
            "backend": name,
            "available": false,
            "reason": format!("not in this glass build (host: {})", std::env::consts::OS),
        }),
    };
    if let Some(w) = warning {
        json.as_object_mut()
            .expect("capability report is a JSON object")
            .insert("warning".to_string(), serde_json::Value::String(w));
    }
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
        assert!(v["capabilities"]["input"]["status"].is_string());
        assert!(v["capabilities"]["input"]["tools"][0].is_string());
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
    fn render_json_resolved_warns_on_unrecognized_env_backend() {
        // No explicit backend + a typo'd GLASS_BACKEND: report the host default's caps, but
        // attach a `warning` naming the dropped value so the fallback isn't silent (#148).
        let host_default = crate::default_backend(None);
        let v: serde_json::Value =
            serde_json::from_str(&render_json_resolved(None, Some("andriod")).unwrap()).unwrap();
        assert_eq!(v["backend"], host_default);
        let warn = v["warning"].as_str().expect("warning field present");
        assert!(warn.contains("andriod"), "warning: {warn}");

        // A recognized value (or unset) attaches no warning — normal output is unchanged.
        for env in [Some("android"), None] {
            let v: serde_json::Value =
                serde_json::from_str(&render_json_resolved(None, env).unwrap()).unwrap();
            assert!(v.get("warning").is_none(), "unexpected warning for {env:?}");
        }
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

    // android/x11 are both compiled in on Linux (android is host-OS-agnostic; x11 is a
    // Linux-only dependency — see Cargo.toml). Gated like `render_json_not_on_this_host_shape`
    // so it compiles out (not fails) on the macOS/Windows CI legs that also run this suite.
    #[cfg(target_os = "linux")]
    #[test]
    fn render_attaches_notes_and_tools_on_the_shipped_path() {
        // degraded/requires_setup carry a note; the note rides the rendered JSON.
        let v: serde_json::Value =
            serde_json::from_str(&render_json(Some("android")).unwrap()).unwrap();
        assert_eq!(v["capabilities"]["input"]["status"], "degraded");
        assert!(v["capabilities"]["input"]["note"]
            .as_str()
            .unwrap()
            .contains("GLASS_ANDROID_AGENT_JAR"));
        assert!(v["capabilities"]["input"]["tools"]
            .as_array()
            .unwrap()
            .iter()
            .any(|t| t == "glass_type"));
        // a plain `supported` op omits `note`.
        let v: serde_json::Value =
            serde_json::from_str(&render_json(Some("x11")).unwrap()).unwrap();
        assert_eq!(v["capabilities"]["input"]["status"], "supported");
        assert!(v["capabilities"]["input"].get("note").is_none());
    }

    /// Ties each compiled-in backend's `glass_<x>::BACKEND` const to this crate's own
    /// registry ([`crate::BACKENDS`]) and dispatch ([`capabilities_for`]) — so a backend
    /// crate renaming its `BACKEND` const (a drifting third copy of the backend name,
    /// alongside `BACKENDS` and the `capabilities_for` match arms) fails here instead of
    /// silently reporting `NotOnThisHost` at runtime. Gated exactly like `capabilities_for`
    /// itself: android is always compiled in; the rest per host OS.
    mod backend_const_matches_registry {
        use super::*;

        #[test]
        fn android() {
            assert!(crate::BACKENDS.contains(&glass_android::BACKEND));
            assert!(matches!(
                capabilities_for(glass_android::BACKEND),
                Some(CapabilityReport::Available(_))
            ));
        }

        #[cfg(target_os = "linux")]
        #[test]
        fn x11_and_wayland() {
            assert!(crate::BACKENDS.contains(&glass_x11::BACKEND));
            assert!(matches!(
                capabilities_for(glass_x11::BACKEND),
                Some(CapabilityReport::Available(_))
            ));
            assert!(crate::BACKENDS.contains(&glass_wayland::BACKEND));
            assert!(matches!(
                capabilities_for(glass_wayland::BACKEND),
                Some(CapabilityReport::Available(_))
            ));
        }

        #[cfg(windows)]
        #[test]
        fn windows() {
            assert!(crate::BACKENDS.contains(&glass_windows::BACKEND));
            assert!(matches!(
                capabilities_for(glass_windows::BACKEND),
                Some(CapabilityReport::Available(_))
            ));
        }

        #[cfg(target_os = "macos")]
        #[test]
        fn macos_and_ios() {
            assert!(crate::BACKENDS.contains(&glass_macos::BACKEND));
            assert!(matches!(
                capabilities_for(glass_macos::BACKEND),
                Some(CapabilityReport::Available(_))
            ));
            assert!(crate::BACKENDS.contains(&glass_ios::BACKEND));
            assert!(matches!(
                capabilities_for(glass_ios::BACKEND),
                Some(CapabilityReport::Available(_))
            ));
        }
    }

    #[test]
    fn operation_tools_covers_every_rendered_operation() {
        use glass_core::capability::{CapabilityMap, CapabilityStatus};
        let dummy = CapabilityMap {
            input: CapabilityStatus::supported(),
            multi_touch: CapabilityStatus::supported(),
            clipboard: CapabilityStatus::supported(),
            accessibility: CapabilityStatus::supported(),
            window_move_resize: CapabilityStatus::supported(),
        };
        let rendered: std::collections::BTreeSet<&str> =
            dummy.entries().iter().map(|(op, _)| *op).collect();
        let mapped: std::collections::BTreeSet<&str> =
            OPERATION_TOOLS.iter().map(|(op, _)| *op).collect();
        assert_eq!(
            rendered, mapped,
            "OPERATION_TOOLS keys must match the operations render emits"
        );
    }
}
