//! `glass-mcp env`: list glass's configuration environment variables — purpose,
//! default, and current value (secrets redacted). Operator-facing config inventory,
//! distinct from `doctor` (health). The registry and rendering are pure so they are
//! unit-tested without mutating the process environment.

use glass_core::Palette;
use serde::Serialize;

/// Which part of glass a variable affects (controls grouping in the listing).
#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum EnvScope {
    All,
    X11,
    Wayland,
    Linux,
    Windows,
    Android,
    Ios,
    Macos,
    Network,
}

impl EnvScope {
    fn label(self) -> &'static str {
        match self {
            EnvScope::All => "all",
            EnvScope::X11 => "x11",
            EnvScope::Wayland => "wayland",
            EnvScope::Linux => "linux",
            EnvScope::Windows => "windows",
            EnvScope::Android => "android",
            EnvScope::Ios => "ios",
            EnvScope::Macos => "macos",
            EnvScope::Network => "network",
        }
    }
}

/// Fixed group order for output: general → display servers → OS containment → android → ios →
/// macos → network.
const SCOPE_ORDER: [EnvScope; 9] = [
    EnvScope::All,
    EnvScope::X11,
    EnvScope::Wayland,
    EnvScope::Linux,
    EnvScope::Windows,
    EnvScope::Android,
    EnvScope::Ios,
    EnvScope::Macos,
    EnvScope::Network,
];

/// Documentation for one `GLASS_*` variable.
pub(crate) struct EnvVarDoc {
    pub name: &'static str,
    pub scope: EnvScope,
    pub purpose: &'static str,
    pub default: &'static str,
    pub secret: bool,
}

/// Every `GLASS_*` configuration variable, authored in display order within each scope.
/// KEEP IN SYNC: a new GLASS_* var read anywhere in the workspace gets an entry here.
pub(crate) const GLASS_ENV: &[EnvVarDoc] = &[
    EnvVarDoc {
        name: "GLASS_BACKEND",
        scope: EnvScope::All,
        purpose: "Default backend when glass_start omits `backend`",
        default: "x11 (or windows on Windows, macos on macOS)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_SANDBOX",
        scope: EnvScope::All,
        purpose: "Default containment level (off/default/strict) when glass_start omits `sandbox`",
        default: "default",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_SANDBOX_FLOOR",
        scope: EnvScope::All,
        purpose: "Operator-enforced minimum containment level; raises an omitted request, refuses an explicit one below it",
        default: "off (no floor)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_DISPLAY",
        scope: EnvScope::X11,
        purpose: "X11 display to use; :0 drives the real desktop",
        default: "self-spawn a private headless Xvfb",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_XVFB_SCREEN",
        scope: EnvScope::X11,
        purpose: "Geometry of the self-spawned Xvfb (WxHxDepth)",
        default: "1280x800x24",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_XVFB",
        scope: EnvScope::X11,
        purpose: "Xvfb binary (path or PATH name)",
        default: "Xvfb",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_SWAY",
        scope: EnvScope::Wayland,
        purpose: "sway binary; explicit value skips discovery (fail-closed if wrong)",
        default: "auto-discover (PATH / ~/.local/share/glass/sway / next to glass-mcp)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_WAYLAND_SCREEN",
        scope: EnvScope::Wayland,
        purpose: "Headless sway output size (WxH; no depth field, unlike GLASS_XVFB_SCREEN)",
        default: "1280x800",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_BWRAP",
        scope: EnvScope::Linux,
        purpose: "bubblewrap binary (app + build containment)",
        default: "bwrap",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_SH",
        scope: EnvScope::Linux,
        purpose: "Shell used to run spec.build",
        default: "sh",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_DBUS_DAEMON",
        scope: EnvScope::Linux,
        purpose: "dbus-daemon binary for the private AT-SPI bus (a11y: true launches)",
        default: "dbus-daemon (on PATH)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ATSPI_LAUNCHER",
        scope: EnvScope::Linux,
        purpose: "at-spi-bus-launcher binary; explicit value skips discovery (fail-closed if wrong) \
                  for launches and for what glass doctor / glass_capabilities report",
        default: "auto-discovered (well-known install paths)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_WIN_SANDBOX_PROVIDER",
        scope: EnvScope::Windows,
        purpose: "In-OS containment provider (auto/sandboxie/none)",
        default: "auto",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_SANDBOXIE_DIR",
        scope: EnvScope::Windows,
        purpose: "Sandboxie install directory",
        default: "%ProgramFiles%\\Sandboxie (auto-detected)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_CLIP_SHIM_DLL",
        scope: EnvScope::Windows,
        purpose: "Path to the private-clipboard shim DLL (glass_clip_shim_windows.dll) injected into a Sandboxie-boxed app",
        default: "next to glass-mcp, else Layer-2 (DLL) clipboard isolation is unavailable",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_CLIP_HOOK_DLL",
        scope: EnvScope::Windows,
        purpose: "Deprecated spelling of GLASS_CLIP_SHIM_DLL, still honoured; set the latter instead",
        default: "unset — used only when GLASS_CLIP_SHIM_DLL is unset",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_TYPE_DWELL_MS",
        scope: EnvScope::Windows,
        purpose: "Inter-character typing dwell (ms); raise if rapid Unicode injection corrupts, lower for speed",
        default: "60",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ADB",
        scope: EnvScope::Android,
        purpose: "adb binary used to drive an Android emulator/device",
        default: "adb (on PATH)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ANDROID_SERIAL",
        scope: EnvScope::Android,
        purpose: "adb serial of the device to attach to when several are online",
        default: "the sole online device",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ANDROID_LIFECYCLE",
        scope: EnvScope::Android,
        purpose: "`auto` (attach to a running emulator, else boot one) or `attach` (never auto-boot)",
        default: "auto",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_EMULATOR",
        scope: EnvScope::Android,
        purpose: "emulator binary; overrides $ANDROID_SDK_ROOT/emulator/emulator",
        default: "resolved from ANDROID_SDK_ROOT/ANDROID_HOME, else emulator on PATH",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_AVD",
        scope: EnvScope::Android,
        purpose: "which AVD to boot when none is running",
        default: "the sole AVD",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_EMULATOR_ARGS",
        scope: EnvScope::Android,
        purpose: "extra flags appended to the headless emulator launch",
        default: "(none)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_EMULATOR_BOOT_TIMEOUT_MS",
        scope: EnvScope::Android,
        purpose: "max wait for the booting emulator to reach sys.boot_completed",
        default: "120000",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_EMULATOR_KEEP",
        scope: EnvScope::Android,
        purpose: "leave a glass-booted emulator running at shutdown instead of stopping it",
        default: "stop it",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ANDROID_AGENT_JAR",
        scope: EnvScope::Android,
        purpose: "override path to glass-agent.jar (on-device agent: clipboard + high-fidelity input); auto-discovered next to glass-mcp or in the data dir otherwise",
        default: "auto-discovered, else pure-adb paths",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ANDROID_AGENT",
        scope: EnvScope::Android,
        purpose: "auto|off; default auto when the jar resolves; off forces the pure-adb paths",
        default: "auto",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ANDROID_A11Y_APK",
        scope: EnvScope::Android,
        purpose: "override path to glass-a11y.apk (on-device AccessibilityService reader: Compose-rich tree + high-fidelity set_value); auto-discovered next to glass-mcp or in the data dir otherwise",
        default: "auto-discovered, else uiautomator",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_ANDROID_A11Y",
        scope: EnvScope::Android,
        purpose: "auto|off \u{2014} disable the a11y service even when the APK is set",
        default: "auto",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_IOS_UDID",
        scope: EnvScope::Ios,
        purpose: "exact iOS Simulator UDID to drive when several are available",
        default: "the newest booted/available iPhone simulator",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_IOS_DEVICE",
        scope: EnvScope::Ios,
        purpose: "device name to boot when none is running, e.g. \"iPhone 17\" or \"iPad Pro 13-inch\" (ignored if GLASS_IOS_UDID is set)",
        default: "the newest available iPhone simulator",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_SIMULATOR_KEEP",
        scope: EnvScope::Ios,
        purpose: "leave a glass-booted iOS Simulator running at shutdown instead of stopping it",
        default: "stop it",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_IDB_COMPANION",
        scope: EnvScope::Ios,
        purpose: "path to the idb_companion binary (input + accessibility for the iOS Simulator backend)",
        default: "idb_companion (found on PATH)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_CLIP_SHIM_DYLIB",
        scope: EnvScope::Macos,
        purpose: "Override discovery of the injected clipboard-isolation shim (libglass_clip_shim_macos.dylib)",
        default: "auto-discovered (bundled Frameworks/, next to glass-mcp, or the build's target dir)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_TOKEN",
        scope: EnvScope::Network,
        purpose: "Bearer token for the serve --http transport",
        default: "(none)",
        secret: true,
    },
    EnvVarDoc {
        name: "GLASS_AUDIT_LOG",
        scope: EnvScope::All,
        purpose: "Append a JSONL audit log of every actuation to this path (opt-in)",
        default: "(off)",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_AUDIT_CONTENT",
        scope: EnvScope::All,
        purpose: "Audit content detail: none | redacted | full",
        default: "redacted",
        secret: false,
    },
    EnvVarDoc {
        name: "GLASS_AUDIT_PREFIX_LEN",
        scope: EnvScope::All,
        purpose: "Chars of plaintext prefix kept in redacted audit content (0 = none)",
        default: "8",
        secret: false,
    },
];

/// `GLASS_*`-prefixed names read (or, for `GLASS_CLIP`, merely spelled) somewhere in the
/// workspace that are deliberately **not** part of the user-facing override surface, so the
/// `code_reads_match_registry_or_internal_allowlist` guard test below doesn't require them in
/// [`GLASS_ENV`]. Each is a var glass **sets** for its own child/shim process (IPC plumbing),
/// reads only to force a code path in a test, or has `build.rs` emit as a `cargo:rustc-env` for
/// the crate to read back with `env!` — never an operator override. Keep this list
/// exact: an addition here should be accompanied by a one-line reason, same as the entries
/// below.
///
/// Only consumed by the guard test (`#[cfg(test)]`): it documents the internal surface but has
/// no production reader, so it would otherwise be flagged dead code in a non-test build.
#[cfg(test)]
pub(crate) const INTERNAL_ENV: &[&str] = &[
    // Not actually an environment variable: the X11 CLIPBOARD-selection transfer atom name
    // interned in glass-x11/src/clipboard.rs (`b"GLASS_CLIP"`). It shares the `GLASS_` prefix
    // the guard test scans for, so it needs an entry here even though `std::env::var` never
    // touches it.
    "GLASS_CLIP",
    // The three the glass-wayland test harness passes to the app it launches
    // (glass-wayland/src/testw.rs): which windows to map, whether to ignore a close request, and
    // whether to be an X11 client. Read only by that fixture, never by glass.
    "GLASS_TESTW_WINDOWS",
    "GLASS_TESTW_IGNORES_CLOSE",
    "GLASS_TESTW_X11",
    // Per-launch named-pasteboard name: glass sets this on the injected app's env
    // (glass-macos/src/process.rs) for the clip shim (glass-clip-shim-macos) to read; not an
    // operator-facing override.
    "GLASS_CLIP_PASTEBOARD",
    // Clipboard-hook pipe name: glass sets this in the generated Sandboxie launch.cmd
    // (glass-windows) for glass-clip-shim-windows's injected DLL to read; not an operator override.
    "GLASS_CLIP_PIPE",
    // Forces a test-only AX-geometry fallback path (glass-macos/src/axwindow.rs); read only to
    // exercise that path in an integration test, not a supported way to configure glass.
    "GLASS_MACOS_FORCE_AX_GEOMETRY_FALLBACK",
    // Makes the macOS test fixture refuse a quit request, standing in for an app with an
    // unsaved-changes sheet. glass only ever *sets* it, on a fixture it spawns in one of its own
    // tests (glass-macos/src/process.rs); nothing in glass reads it.
    "GLASS_FIXTURE_VETO_QUIT",
    // Build-time only: `build.rs` computes the release version and emits it as a
    // `cargo:rustc-env=GLASS_VERSION`, read via `env!` (see `crate::VERSION`). Not read from the
    // process environment and never an operator override.
    "GLASS_VERSION",
    // Build-time only: `build.rs` reads them out of `server.json` and emits them as
    // `cargo:rustc-env`, read via `env!` in the MCP `initialize` handshake. Keep the `BUILD`
    // infix: without it they read as operator overrides that silently do nothing.
    "GLASS_BUILD_TITLE",
    "GLASS_BUILD_DESCRIPTION",
    "GLASS_BUILD_WEBSITE_URL",
];

/// Standard (non-`GLASS_*`) env glass reads at runtime — reference only.
pub(crate) const STD_ENV: &[(&str, &str)] = &[
    (
        "PATH",
        "Resolve bare external-tool names (bwrap/Xvfb/sway/sh)",
    ),
    ("HOME", "Sandbox ephemeral-HOME base; sway data-dir lookup"),
    (
        "XDG_DATA_HOME",
        "sway bundle discovery ($XDG_DATA_HOME/glass/sway)",
    ),
    (
        "DBUS_SESSION_BUS_ADDRESS",
        "Linux accessibility (AT-SPI) bus",
    ),
    ("WINDIR", "Windows system directory"),
    (
        "NO_COLOR",
        "Suppress color in doctor/env human output when set to a non-empty value (no-color.org)",
    ),
    (
        "TERM",
        "Terminal type; `dumb` suppresses color in doctor/env human output",
    ),
];

const DISPLAY_NOTE: &str =
    "note: the X11 backend ignores ambient DISPLAY; set GLASS_DISPLAY instead.";

/// Current value of `name`: `Some(value)` when set and non-empty, else `None`.
pub(crate) fn current_from_env(name: &str) -> Option<String> {
    std::env::var(name).ok().filter(|v| !v.is_empty())
}

/// The human-readable "current" cell for a var, given its current value.
fn current_cell(doc: &EnvVarDoc, current: Option<&str>) -> String {
    match (doc.secret, current) {
        (true, Some(_)) => "set".to_string(),
        (true, None) => "(unset)".to_string(),
        (false, Some(v)) => format!("{v} (override)"),
        (false, None) => format!("(unset \u{2192} {})", doc.default),
    }
}

/// Render the listing with `palette`; [`Palette::PLAIN`] gives the uncolored text.
///
/// Pad names *before* painting them: `{:<26}` counts an escape's characters toward the width, so
/// painting first pulls the purpose column left by the width of the escapes.
pub(crate) fn render_styled(current: &dyn Fn(&str) -> Option<String>, palette: &Palette) -> String {
    /// Width of the name column, matching the `{:<26}` this replaced.
    const NAME_COL: usize = 26;
    let pad_to_col = |name: &str| " ".repeat(NAME_COL.saturating_sub(name.chars().count()));

    let mut out = format!("{}\n", palette.paint(palette.bold, "glass environment"));
    for scope in SCOPE_ORDER {
        let group: Vec<&EnvVarDoc> = GLASS_ENV.iter().filter(|d| d.scope == scope).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!(
            "\n{}\n",
            palette.paint(palette.heading, &format!("[{}]", scope.label()))
        ));
        for d in group {
            let cur = current(d.name);
            out.push_str(&format!(
                "  {}{} {}\n",
                palette.paint(palette.bold, d.name),
                pad_to_col(d.name),
                d.purpose
            ));
            let cur_attr = if cur.is_some() {
                palette.ok
            } else {
                palette.dim
            };
            out.push_str(&format!(
                "  {:<width$} {} | {}\n",
                "",
                palette.paint(palette.dim, &format!("default: {}", d.default)),
                palette.paint(
                    cur_attr,
                    &format!("current: {}", current_cell(d, cur.as_deref()))
                ),
                width = NAME_COL,
            ));
        }
    }
    out.push_str(&format!(
        "\n{}\n",
        palette.paint(palette.heading, "standard env (read, not glass-specific)")
    ));
    for (name, purpose) in STD_ENV {
        let set = current(name).is_some();
        let cur = if set { "set" } else { "(unset)" };
        let attr = if set { palette.ok } else { palette.dim };
        out.push_str(&format!(
            "  {}{} {purpose} | {}\n",
            palette.paint(palette.bold, name),
            pad_to_col(name),
            palette.paint(attr, &format!("current: {cur}")),
        ));
    }
    out.push_str(&format!("\n{}\n", palette.paint(palette.dim, DISPLAY_NOTE)));
    out
}

#[derive(Serialize)]
struct GlassVarView {
    name: &'static str,
    scope: &'static str,
    purpose: &'static str,
    default: &'static str,
    secret: bool,
    is_set: bool,
    /// Omitted entirely for secrets (only `is_set` conveys presence).
    #[serde(skip_serializing_if = "Option::is_none")]
    current: Option<String>,
}

#[derive(Serialize)]
struct StdVarView {
    name: &'static str,
    purpose: &'static str,
    is_set: bool,
}

#[derive(Serialize)]
struct EnvJson {
    glass: Vec<GlassVarView>,
    standard: Vec<StdVarView>,
    notes: Vec<&'static str>,
}

/// Render the JSON listing (same overall order as the text form).
pub(crate) fn render_json(current: &dyn Fn(&str) -> Option<String>) -> String {
    let mut glass = Vec::new();
    for scope in SCOPE_ORDER {
        for d in GLASS_ENV.iter().filter(|d| d.scope == scope) {
            let cur = current(d.name);
            glass.push(GlassVarView {
                name: d.name,
                scope: d.scope.label(),
                purpose: d.purpose,
                default: d.default,
                secret: d.secret,
                is_set: cur.is_some(),
                current: if d.secret { None } else { cur },
            });
        }
    }
    let standard = STD_ENV
        .iter()
        .map(|(name, purpose)| StdVarView {
            name,
            purpose,
            is_set: current(name).is_some(),
        })
        .collect();
    let doc = EnvJson {
        glass,
        standard,
        notes: vec![DISPLAY_NOTE],
    };
    serde_json::to_string_pretty(&doc).expect("serialize env")
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stub "current values" map for deterministic, env-free tests.
    fn stub(name: &str) -> Option<String> {
        match name {
            "GLASS_SANDBOX" => Some("strict".to_string()),
            "GLASS_TOKEN" => Some("supersecret".to_string()),
            "PATH" => Some("/usr/bin".to_string()),
            _ => None,
        }
    }

    #[test]
    fn registry_has_all_known_vars_once() {
        let expected = [
            "GLASS_BACKEND",
            "GLASS_SANDBOX",
            "GLASS_SANDBOX_FLOOR",
            "GLASS_DISPLAY",
            "GLASS_XVFB_SCREEN",
            "GLASS_XVFB",
            "GLASS_SWAY",
            "GLASS_WAYLAND_SCREEN",
            "GLASS_BWRAP",
            "GLASS_SH",
            "GLASS_DBUS_DAEMON",
            "GLASS_ATSPI_LAUNCHER",
            "GLASS_WIN_SANDBOX_PROVIDER",
            "GLASS_SANDBOXIE_DIR",
            "GLASS_CLIP_SHIM_DLL",
            "GLASS_CLIP_HOOK_DLL",
            "GLASS_TYPE_DWELL_MS",
            "GLASS_ADB",
            "GLASS_ANDROID_SERIAL",
            "GLASS_ANDROID_LIFECYCLE",
            "GLASS_EMULATOR",
            "GLASS_AVD",
            "GLASS_EMULATOR_ARGS",
            "GLASS_EMULATOR_BOOT_TIMEOUT_MS",
            "GLASS_EMULATOR_KEEP",
            "GLASS_ANDROID_AGENT_JAR",
            "GLASS_ANDROID_AGENT",
            "GLASS_ANDROID_A11Y_APK",
            "GLASS_ANDROID_A11Y",
            "GLASS_IOS_UDID",
            "GLASS_IOS_DEVICE",
            "GLASS_SIMULATOR_KEEP",
            "GLASS_IDB_COMPANION",
            "GLASS_CLIP_SHIM_DYLIB",
            "GLASS_TOKEN",
            "GLASS_AUDIT_LOG",
            "GLASS_AUDIT_CONTENT",
            "GLASS_AUDIT_PREFIX_LEN",
        ];
        for name in expected {
            let n = GLASS_ENV.iter().filter(|d| d.name == name).count();
            assert_eq!(
                n, 1,
                "{name} must appear exactly once in GLASS_ENV (found {n})"
            );
        }
        assert_eq!(
            GLASS_ENV.len(),
            expected.len(),
            "GLASS_ENV has an undocumented entry"
        );
    }

    #[test]
    fn text_shows_default_override_and_unset_markers() {
        let out = render_styled(&stub, &Palette::PLAIN);
        // a set non-secret shows value + (override)
        assert!(out.contains("current: strict (override)"), "{out}");
        // an unset non-secret shows (unset → default)
        assert!(out.contains("GLASS_BACKEND"), "{out}");
        assert!(
            out.contains("(unset \u{2192} x11 (or windows on Windows, macos on macOS))"),
            "{out}"
        );
        // standard-env + ignored-DISPLAY note present
        assert!(
            out.contains("standard env (read, not glass-specific)"),
            "{out}"
        );
        assert!(out.contains("ignores ambient DISPLAY"), "{out}");
    }

    #[test]
    fn text_groups_are_in_fixed_scope_order() {
        let out = render_styled(&stub, &Palette::PLAIN);
        let idx = |s: &str| {
            out.find(s)
                .unwrap_or_else(|| panic!("missing {s} in:\n{out}"))
        };
        // group order: all < x11 < wayland < linux < windows < android < ios < macos < network
        assert!(idx("GLASS_BACKEND") < idx("GLASS_DISPLAY"));
        assert!(idx("GLASS_DISPLAY") < idx("GLASS_SWAY"));
        assert!(idx("GLASS_SWAY") < idx("GLASS_BWRAP"));
        assert!(idx("GLASS_BWRAP") < idx("GLASS_DBUS_DAEMON"));
        assert!(idx("GLASS_DBUS_DAEMON") < idx("GLASS_ATSPI_LAUNCHER"));
        assert!(idx("GLASS_ATSPI_LAUNCHER") < idx("GLASS_WIN_SANDBOX_PROVIDER"));
        assert!(idx("GLASS_WIN_SANDBOX_PROVIDER") < idx("GLASS_ANDROID_AGENT_JAR"));
        assert!(idx("GLASS_ANDROID_AGENT_JAR") < idx("GLASS_ANDROID_A11Y_APK"));
        // Use unique purpose snippets to distinguish A11Y_APK from A11Y (prefix-match hazard).
        assert!(idx("glass-a11y.apk") < idx("disable the a11y service"));
        assert!(idx("disable the a11y service") < idx("GLASS_IOS_UDID"));
        assert!(idx("GLASS_IOS_UDID") < idx("GLASS_SIMULATOR_KEEP"));
        assert!(idx("GLASS_SIMULATOR_KEEP") < idx("GLASS_CLIP_SHIM_DYLIB"));
        assert!(idx("GLASS_CLIP_SHIM_DYLIB") < idx("GLASS_TOKEN"));
        // adjacency within the windows group
        assert!(idx("GLASS_WIN_SANDBOX_PROVIDER") < idx("GLASS_SANDBOXIE_DIR"));
        assert!(idx("GLASS_SANDBOXIE_DIR") < idx("GLASS_CLIP_SHIM_DLL"));
        // the deprecated spelling is listed, and directly after the one that supersedes it
        assert!(idx("GLASS_CLIP_SHIM_DLL") < idx("Deprecated spelling of GLASS_CLIP_SHIM_DLL"));
    }

    #[test]
    fn secret_value_is_never_emitted() {
        let text = render_styled(&stub, &Palette::PLAIN);
        let json = render_json(&stub);
        assert!(
            !text.contains("supersecret"),
            "secret leaked in text:\n{text}"
        );
        assert!(
            !json.contains("supersecret"),
            "secret leaked in json:\n{json}"
        );
        // but presence is shown
        assert!(text.contains("GLASS_TOKEN"));
        // GLASS_TOKEN line shows `set`
        let token_line = text
            .lines()
            .zip(text.lines().skip(1))
            .find(|(a, _)| a.contains("GLASS_TOKEN"))
            .map(|(_, b)| b)
            .unwrap();
        assert!(token_line.contains("current: set"), "{token_line}");
    }

    /// Drop every `\x1b[…m` SGR sequence, leaving the text bytes behind.
    ///
    /// Deliberately duplicated from glass-core's `doctor` tests rather than shared: exporting a
    /// test-only helper from glass-core's public API to serve one consumer is the worse trade.
    /// Leave both copies alone.
    fn strip_ansi(s: &str) -> String {
        let mut out = String::with_capacity(s.len());
        let mut chars = s.chars();
        while let Some(c) = chars.next() {
            if c == '\x1b' {
                for c in chars.by_ref() {
                    if c == 'm' {
                        break;
                    }
                }
            } else {
                out.push(c);
            }
        }
        out
    }

    #[test]
    fn strip_ansi_removes_escapes_and_is_not_the_identity() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[1mGLASS_BACKEND\x1b[0m"), "GLASS_BACKEND");
        let colored = Palette::ANSI.paint(Palette::ANSI.bold, "x");
        assert!(colored.len() > "x".len(), "expected escapes: {colored:?}");
    }

    #[test]
    fn color_adds_escapes_and_changes_nothing_else() {
        // Also the column guard: painting a name before padding it would pull the purpose column
        // left by the width of the escapes, which shows up here as a byte difference.
        assert_eq!(
            strip_ansi(&render_styled(&stub, &Palette::ANSI)),
            render_styled(&stub, &Palette::PLAIN)
        );
    }

    #[test]
    fn a_set_variable_is_green_and_an_unset_one_is_dim() {
        let p = Palette::ANSI;
        let out = render_styled(&stub, &p);
        // GLASS_SANDBOX is set to "strict" by `stub`.
        assert!(
            out.contains(&format!("{}current: strict (override){}", p.ok, p.reset)),
            "{out:?}"
        );
        // GLASS_BACKEND is unset — the whole cell plus the reset, so this half is no weaker
        // than the set half above.
        assert!(
            out.contains(&format!(
                "{}current: (unset \u{2192} x11 (or windows on Windows, macos on macOS)){}",
                p.dim, p.reset
            )),
            "{out:?}"
        );
    }

    #[test]
    fn the_purpose_column_starts_at_a_fixed_offset() {
        // The byte-identity test pads both sides with the same constant, so it is blind to the
        // constant itself being wrong. The offset here is a literal: deriving it from NAME_COL
        // would inherit exactly the error guarded against.
        let out = render_styled(&stub, &Palette::PLAIN);
        let line = |starts: &str| {
            out.lines()
                .find(|l| l.starts_with(starts))
                .unwrap_or_else(|| panic!("no {starts:?} line in:\n{out}"))
                .to_string()
        };
        // 29 = the two-space indent + the 26-column name field + one separating space.
        let name_line = line("  GLASS_AVD");
        assert_eq!(
            name_line.find("which AVD to boot when none is running"),
            Some(29),
            "{name_line:?}"
        );
        // The second line of the pair puts `default:` in that same column.
        let default_line = out
            .lines()
            .find(|l| l.contains("default: the sole AVD"))
            .unwrap_or_else(|| panic!("no GLASS_AVD default line in:\n{out}"));
        assert_eq!(default_line.find("default: "), Some(29), "{default_line:?}");
    }

    #[test]
    fn a_name_wider_than_the_column_keeps_exactly_one_space() {
        // GLASS_EMULATOR_BOOT_TIMEOUT_MS is 30 chars, the only name that reaches the
        // saturating_sub clamp.
        let out = render_styled(&stub, &Palette::PLAIN);
        assert!(
            out.contains(
                "\n  GLASS_EMULATOR_BOOT_TIMEOUT_MS max wait for the booting emulator to reach \
                 sys.boot_completed\n"
            ),
            "{out}"
        );
    }

    #[test]
    fn every_span_of_the_colored_listing_is_painted_as_written_here() {
        // Same hole as glass-core's golden: stripping escapes cannot see a *missing* one, so the
        // byte-identity test above passes with any span left unpainted. Escape literals on
        // purpose — an expectation built from `palette.*` would pass against any palette.
        let out = render_styled(&stub, &Palette::ANSI);
        assert!(
            out.starts_with("\x1b[1mglass environment\x1b[0m\n\n\x1b[1;36m[all]\x1b[0m\n"),
            "{out:?}"
        );
        assert!(
            out.contains(concat!(
                "  \x1b[1mGLASS_AVD\x1b[0m                  which AVD to boot when none is running\n",
                "                             \x1b[2mdefault: the sole AVD\x1b[0m | \x1b[2mcurrent: (unset \u{2192} the sole AVD)\x1b[0m\n",
            )),
            "{out:?}"
        );
        assert!(
            out.contains("\n\x1b[1;36mstandard env (read, not glass-specific)\x1b[0m\n"),
            "{out:?}"
        );
        assert!(
            out.ends_with(
                "\n\x1b[2mnote: the X11 backend ignores ambient DISPLAY; set GLASS_DISPLAY \
                 instead.\x1b[0m\n"
            ),
            "{out:?}"
        );
    }

    #[test]
    fn a_standard_variable_that_is_set_is_green_and_an_unset_one_is_dim() {
        // The GLASS_* loop and the STD_ENV loop each decide this for themselves; the test above
        // only exercises the first. `stub` sets PATH and nothing else in STD_ENV.
        let out = render_styled(&stub, &Palette::ANSI);
        let line = |name: &str| {
            let head = format!("  \x1b[1m{name}\x1b[0m");
            out.lines()
                .find(|l| l.starts_with(&head))
                .unwrap_or_else(|| panic!("no {name} line in:\n{out:?}"))
                .to_string()
        };
        let path = line("PATH");
        assert!(path.ends_with("\x1b[32mcurrent: set\x1b[0m"), "{path:?}");
        let windir = line("WINDIR");
        assert!(
            windir.ends_with("\x1b[2mcurrent: (unset)\x1b[0m"),
            "{windir:?}"
        );
    }

    #[test]
    fn a_secret_value_never_leaks_through_the_styled_rendering_either() {
        // A coloring bug that painted the raw value would be a leak, and the existing
        // `secret_value_is_never_emitted` only looks at the plain and JSON forms.
        let out = render_styled(&stub, &Palette::ANSI);
        assert!(
            !out.contains("supersecret"),
            "secret leaked in styled:\n{out}"
        );
        let p = Palette::ANSI;
        assert!(
            out.contains(&format!("{}current: set{}", p.ok, p.reset)),
            "{out:?}"
        );
    }

    #[test]
    fn the_standard_env_list_documents_the_color_variables() {
        // `color::palette` reads NO_COLOR and TERM, so `glass-mcp env` must say so — STD_ENV is
        // the inventory of non-GLASS_* env glass reads.
        let out = render_styled(&stub, &Palette::PLAIN);
        assert!(out.contains("NO_COLOR"), "{out}");
        assert!(out.contains("TERM"), "{out}");
    }

    #[test]
    fn json_parses_and_redacts_secret() {
        let json = render_json(&stub);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let glass = v["glass"].as_array().expect("glass array");
        assert_eq!(glass.len(), GLASS_ENV.len());
        let token = glass.iter().find(|e| e["name"] == "GLASS_TOKEN").unwrap();
        assert_eq!(token["is_set"], true);
        assert!(
            token.get("current").is_none(),
            "secret `current` must be absent from JSON, not null: {token}"
        );
        // a non-secret set var carries its value
        let sandbox = glass.iter().find(|e| e["name"] == "GLASS_SANDBOX").unwrap();
        assert_eq!(sandbox["current"], "strict");
        assert_eq!(sandbox["scope"], "all");
    }

    /// Every `GLASS_[A-Z0-9_]+` name that appears as a **quoted** literal in `text` — e.g.
    /// `"GLASS_BACKEND"` or the byte-string `b"GLASS_CLIP"` — but not a bare identifier like
    /// `GLASS_ENV` (no surrounding quotes), and not a `GLASS_`-prefixed run that continues past
    /// the closing quote (e.g. an interpolated format string) — not `env::var`-shaped, so
    /// deliberately not caught here; see the unit test below for the exact edge cases.
    fn quoted_glass_var_names(text: &str) -> Vec<&str> {
        // Built at runtime rather than written as the literal `"GLASS_` here, so this
        // function's own source line doesn't self-match when `code_reads_match_registry_or_
        // internal_allowlist` (below) scans this very file.
        let needle = format!("{}GLASS_", '"');
        let mut found = Vec::new();
        let mut pos = 0;
        while let Some(rel) = text[pos..].find(needle.as_str()) {
            let name_start = pos + rel + 1; // skip the opening quote, land on 'G'
            let rest = &text[name_start..];
            let name_len = rest
                .find(|c: char| !(c.is_ascii_uppercase() || c.is_ascii_digit() || c == '_'))
                .unwrap_or(rest.len());
            if rest.as_bytes().get(name_len) == Some(&b'"') {
                found.push(&rest[..name_len]);
            }
            pos = name_start + name_len;
        }
        found
    }

    /// Every `.rs` file under `dir`, recursively.
    fn rust_files_under(dir: &std::path::Path, out: &mut Vec<std::path::PathBuf>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                rust_files_under(&path, out);
            } else if path.extension().is_some_and(|ext| ext == "rs") {
                out.push(path);
            }
        }
    }

    /// The freeze mechanism: every `GLASS_*` name read anywhere in the workspace must be either
    /// documented in [`GLASS_ENV`] (the user-facing override surface) or explicitly listed in
    /// [`INTERNAL_ENV`] (plumbing/test-only) — never silently undocumented, so a new var can't
    /// drift out of the guarantee the way `GLASS_DBUS_DAEMON`/`GLASS_ATSPI_LAUNCHER` did before
    /// this test existed. Scans every crate's `*/src/**/*.rs`; `tests/`/`fixture/` directories
    /// sit beside, not under, a crate's `src/`, so they're excluded simply by starting each
    /// crate's walk at `src/` rather than by name-matching directories.
    #[test]
    fn code_reads_match_registry_or_internal_allowlist() {
        let workspace_root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
            .parent() // crates/
            .and_then(|p| p.parent()) // workspace root
            .expect("crates/glass-mcp is two levels under the workspace root");
        let crates_dir = workspace_root.join("crates");

        let mut offenders = Vec::new();
        for crate_entry in std::fs::read_dir(&crates_dir)
            .unwrap_or_else(|e| panic!("read {}: {e}", crates_dir.display()))
            .flatten()
        {
            let src_dir = crate_entry.path().join("src");
            if !src_dir.is_dir() {
                continue;
            }
            let mut files = Vec::new();
            rust_files_under(&src_dir, &mut files);
            for file in files {
                let text = std::fs::read_to_string(&file)
                    .unwrap_or_else(|e| panic!("read {}: {e}", file.display()));
                for var in quoted_glass_var_names(&text) {
                    let registered = GLASS_ENV.iter().any(|d| d.name == var);
                    let internal = INTERNAL_ENV.contains(&var);
                    if !registered && !internal {
                        offenders.push(format!(
                            "{var} is read in {} but not in the GLASS_ENV registry or \
                             INTERNAL_ENV allowlist — add it to one",
                            file.display()
                        ));
                    }
                }
            }
        }
        assert!(offenders.is_empty(), "{}", offenders.join("\n"));
    }

    #[test]
    fn quoted_glass_var_names_ignores_bare_identifiers_and_partial_matches() {
        // A bare identifier (GLASS_ENV, unquoted) must not match...
        assert_eq!(
            quoted_glass_var_names("GLASS_ENV.iter()"),
            Vec::<&str>::new()
        );
        // ...but a real quoted literal must. (Using a registered name here, rather than a made-up
        // one, so this fixture doesn't itself need an INTERNAL_ENV entry to satisfy the guard
        // test below when it scans this file's own source.)
        assert_eq!(
            quoted_glass_var_names(r#"std::env::var("GLASS_BACKEND")"#),
            vec!["GLASS_BACKEND"]
        );
        // A byte-string literal counts too (this is how the X11 atom name GLASS_CLIP is caught).
        assert_eq!(
            quoted_glass_var_names(r#"b"GLASS_CLIP""#),
            vec!["GLASS_CLIP"]
        );
        // Text that continues past the GLASS_-prefixed run before the closing quote (e.g. an
        // interpolated format string) is not treated as an env-var-shaped literal.
        assert_eq!(
            quoted_glass_var_names(r#""GLASS_CLIP_PIPE={pipe}""#),
            Vec::<&str>::new()
        );
    }
}
