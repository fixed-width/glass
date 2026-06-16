//! `glass-mcp env`: list glass's configuration environment variables — purpose,
//! default, and current value (secrets redacted). Operator-facing config inventory,
//! distinct from `doctor` (health). The registry and rendering are pure so they are
//! unit-tested without mutating the process environment.

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
            EnvScope::Network => "network",
        }
    }
}

/// Fixed group order for output: general → display servers → OS containment → android → network.
const SCOPE_ORDER: [EnvScope; 7] = [
    EnvScope::All,
    EnvScope::X11,
    EnvScope::Wayland,
    EnvScope::Linux,
    EnvScope::Windows,
    EnvScope::Android,
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
    EnvVarDoc { name: "GLASS_BACKEND", scope: EnvScope::All,
        purpose: "Default backend when glass_start omits `backend`",
        default: "x11 (or windows on a Windows host)", secret: false },
    EnvVarDoc { name: "GLASS_SANDBOX", scope: EnvScope::All,
        purpose: "Default containment level (off/default/strict)",
        default: "default", secret: false },
    EnvVarDoc { name: "GLASS_DISPLAY", scope: EnvScope::X11,
        purpose: "X11 display to use; :0 drives the real desktop",
        default: "self-spawn a private headless Xvfb", secret: false },
    EnvVarDoc { name: "GLASS_XVFB_SCREEN", scope: EnvScope::X11,
        purpose: "Geometry of the self-spawned Xvfb (WxHxDepth)",
        default: "1280x800x24", secret: false },
    EnvVarDoc { name: "GLASS_XVFB", scope: EnvScope::X11,
        purpose: "Xvfb binary (path or PATH name)",
        default: "Xvfb", secret: false },
    EnvVarDoc { name: "GLASS_SWAY", scope: EnvScope::Wayland,
        purpose: "sway binary; explicit value skips discovery (fail-closed if wrong)",
        default: "auto-discover (PATH / ~/.local/share/glass/sway / next to glass-mcp)", secret: false },
    EnvVarDoc { name: "GLASS_WAYLAND_SCREEN", scope: EnvScope::Wayland,
        purpose: "Headless sway output size (WxH; no depth field, unlike GLASS_XVFB_SCREEN)",
        default: "1280x800", secret: false },
    EnvVarDoc { name: "GLASS_BWRAP", scope: EnvScope::Linux,
        purpose: "bubblewrap binary (app + build containment)",
        default: "bwrap", secret: false },
    EnvVarDoc { name: "GLASS_SH", scope: EnvScope::Linux,
        purpose: "Shell used to run spec.build",
        default: "sh", secret: false },
    EnvVarDoc { name: "GLASS_WIN_SANDBOX_PROVIDER", scope: EnvScope::Windows,
        purpose: "In-OS containment provider (auto/sandboxie/none)",
        default: "auto", secret: false },
    EnvVarDoc { name: "GLASS_SANDBOXIE_DIR", scope: EnvScope::Windows,
        purpose: "Sandboxie install directory",
        default: "%ProgramFiles%\\Sandboxie (auto-detected)", secret: false },
    EnvVarDoc { name: "GLASS_TYPE_DWELL_MS", scope: EnvScope::Windows,
        purpose: "Inter-character typing dwell (ms); raise if rapid Unicode injection corrupts, lower for speed",
        default: "60", secret: false },
    EnvVarDoc { name: "GLASS_ADB", scope: EnvScope::Android,
        purpose: "adb binary used to drive an Android emulator/device",
        default: "adb (on PATH)", secret: false },
    EnvVarDoc { name: "GLASS_ANDROID_SERIAL", scope: EnvScope::Android,
        purpose: "adb serial of the device to attach to when several are online",
        default: "the sole online device", secret: false },
    EnvVarDoc { name: "GLASS_ANDROID_LIFECYCLE", scope: EnvScope::Android,
        purpose: "`auto` (attach to a running emulator, else boot one) or `attach` (never auto-boot)",
        default: "auto", secret: false },
    EnvVarDoc { name: "GLASS_EMULATOR", scope: EnvScope::Android,
        purpose: "emulator binary; overrides $ANDROID_SDK_ROOT/emulator/emulator",
        default: "resolved from ANDROID_SDK_ROOT/ANDROID_HOME, else emulator on PATH", secret: false },
    EnvVarDoc { name: "GLASS_AVD", scope: EnvScope::Android,
        purpose: "which AVD to boot when none is running",
        default: "the sole AVD", secret: false },
    EnvVarDoc { name: "GLASS_EMULATOR_ARGS", scope: EnvScope::Android,
        purpose: "extra flags appended to the headless emulator launch",
        default: "(none)", secret: false },
    EnvVarDoc { name: "GLASS_EMULATOR_BOOT_TIMEOUT_MS", scope: EnvScope::Android,
        purpose: "max wait for the booting emulator to reach sys.boot_completed",
        default: "120000", secret: false },
    EnvVarDoc { name: "GLASS_EMULATOR_KEEP", scope: EnvScope::Android,
        purpose: "leave a glass-booted emulator running at shutdown instead of stopping it",
        default: "stop it", secret: false },
    EnvVarDoc { name: "GLASS_ANDROID_AGENT_JAR", scope: EnvScope::Android,
        purpose: "path to glass-agent.jar; enables the on-device agent (clipboard + high-fidelity input)",
        default: "(none; pure-adb paths used)", secret: false },
    EnvVarDoc { name: "GLASS_ANDROID_AGENT", scope: EnvScope::Android,
        purpose: "auto|off; default auto when the jar resolves; off forces the pure-adb paths",
        default: "auto", secret: false },
    EnvVarDoc { name: "GLASS_TOKEN", scope: EnvScope::Network,
        purpose: "Bearer token for the serve --http transport",
        default: "(none)", secret: true },
    EnvVarDoc { name: "GLASS_AUDIT_LOG", scope: EnvScope::All,
        purpose: "Append a JSONL audit log of every actuation to this path (opt-in)",
        default: "(off)", secret: false },
    EnvVarDoc { name: "GLASS_AUDIT_CONTENT", scope: EnvScope::All,
        purpose: "Audit content detail: none | redacted | full",
        default: "redacted", secret: false },
    EnvVarDoc { name: "GLASS_AUDIT_PREFIX_LEN", scope: EnvScope::All,
        purpose: "Chars of plaintext prefix kept in redacted audit content (0 = none)",
        default: "8", secret: false },
];

/// Standard (non-`GLASS_*`) env glass reads at runtime — reference only.
pub(crate) const STD_ENV: &[(&str, &str)] = &[
    ("PATH", "Resolve bare external-tool names (bwrap/Xvfb/sway/sh)"),
    ("HOME", "Sandbox ephemeral-HOME base; sway data-dir lookup"),
    ("XDG_DATA_HOME", "sway bundle discovery ($XDG_DATA_HOME/glass/sway)"),
    ("DBUS_SESSION_BUS_ADDRESS", "Linux accessibility (AT-SPI) bus"),
    ("WINDIR", "Windows system directory"),
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

/// Render the text listing. `current` returns the live value of a var (`None` = unset).
pub(crate) fn render_text(current: &dyn Fn(&str) -> Option<String>) -> String {
    let mut out = String::from("glass environment\n");
    for scope in SCOPE_ORDER {
        let group: Vec<&EnvVarDoc> = GLASS_ENV.iter().filter(|d| d.scope == scope).collect();
        if group.is_empty() {
            continue;
        }
        out.push_str(&format!("\n[{}]\n", scope.label()));
        for d in group {
            let cur = current(d.name);
            out.push_str(&format!("  {:<26} {}\n", d.name, d.purpose));
            out.push_str(&format!(
                "  {:<26} default: {} | current: {}\n",
                "",
                d.default,
                current_cell(d, cur.as_deref()),
            ));
        }
    }
    out.push_str("\nstandard env (read, not glass-specific)\n");
    for (name, purpose) in STD_ENV {
        let cur = if current(name).is_some() { "set" } else { "(unset)" };
        out.push_str(&format!("  {name:<26} {purpose} | current: {cur}\n"));
    }
    out.push_str(&format!("\n{DISPLAY_NOTE}\n"));
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
        .map(|(name, purpose)| StdVarView { name, purpose, is_set: current(name).is_some() })
        .collect();
    let doc = EnvJson { glass, standard, notes: vec![DISPLAY_NOTE] };
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
            "GLASS_BACKEND", "GLASS_SANDBOX", "GLASS_DISPLAY", "GLASS_XVFB_SCREEN",
            "GLASS_XVFB", "GLASS_SWAY", "GLASS_WAYLAND_SCREEN", "GLASS_BWRAP", "GLASS_SH",
            "GLASS_WIN_SANDBOX_PROVIDER", "GLASS_SANDBOXIE_DIR", "GLASS_TYPE_DWELL_MS",
            "GLASS_ADB", "GLASS_ANDROID_SERIAL", "GLASS_ANDROID_LIFECYCLE",
            "GLASS_EMULATOR", "GLASS_AVD", "GLASS_EMULATOR_ARGS",
            "GLASS_EMULATOR_BOOT_TIMEOUT_MS", "GLASS_EMULATOR_KEEP",
            "GLASS_ANDROID_AGENT_JAR", "GLASS_ANDROID_AGENT",
            "GLASS_TOKEN",
            "GLASS_AUDIT_LOG", "GLASS_AUDIT_CONTENT", "GLASS_AUDIT_PREFIX_LEN",
        ];
        for name in expected {
            let n = GLASS_ENV.iter().filter(|d| d.name == name).count();
            assert_eq!(n, 1, "{name} must appear exactly once in GLASS_ENV (found {n})");
        }
        assert_eq!(GLASS_ENV.len(), expected.len(), "GLASS_ENV has an undocumented entry");
    }

    #[test]
    fn text_shows_default_override_and_unset_markers() {
        let out = render_text(&stub);
        // a set non-secret shows value + (override)
        assert!(out.contains("current: strict (override)"), "{out}");
        // an unset non-secret shows (unset → default)
        assert!(out.contains("GLASS_BACKEND"), "{out}");
        assert!(out.contains("(unset \u{2192} x11 (or windows on a Windows host))"), "{out}");
        // standard-env + ignored-DISPLAY note present
        assert!(out.contains("standard env (read, not glass-specific)"), "{out}");
        assert!(out.contains("ignores ambient DISPLAY"), "{out}");
    }

    #[test]
    fn text_groups_are_in_fixed_scope_order() {
        let out = render_text(&stub);
        let idx = |s: &str| out.find(s).unwrap_or_else(|| panic!("missing {s} in:\n{out}"));
        // group order: all < x11 < wayland < linux < windows < network
        assert!(idx("GLASS_BACKEND") < idx("GLASS_DISPLAY"));
        assert!(idx("GLASS_DISPLAY") < idx("GLASS_SWAY"));
        assert!(idx("GLASS_SWAY") < idx("GLASS_BWRAP"));
        assert!(idx("GLASS_BWRAP") < idx("GLASS_WIN_SANDBOX_PROVIDER"));
        assert!(idx("GLASS_WIN_SANDBOX_PROVIDER") < idx("GLASS_TOKEN"));
        // adjacency within the windows group
        assert!(idx("GLASS_WIN_SANDBOX_PROVIDER") < idx("GLASS_SANDBOXIE_DIR"));
    }

    #[test]
    fn secret_value_is_never_emitted() {
        let text = render_text(&stub);
        let json = render_json(&stub);
        assert!(!text.contains("supersecret"), "secret leaked in text:\n{text}");
        assert!(!json.contains("supersecret"), "secret leaked in json:\n{json}");
        // but presence is shown
        assert!(text.contains("GLASS_TOKEN"));
        // GLASS_TOKEN line shows `set`
        let token_line = text.lines().zip(text.lines().skip(1))
            .find(|(a, _)| a.contains("GLASS_TOKEN")).map(|(_, b)| b).unwrap();
        assert!(token_line.contains("current: set"), "{token_line}");
    }

    #[test]
    fn json_parses_and_redacts_secret() {
        let json = render_json(&stub);
        let v: serde_json::Value = serde_json::from_str(&json).expect("valid json");
        let glass = v["glass"].as_array().expect("glass array");
        assert_eq!(glass.len(), GLASS_ENV.len());
        let token = glass.iter().find(|e| e["name"] == "GLASS_TOKEN").unwrap();
        assert_eq!(token["is_set"], true);
        assert!(token.get("current").is_none(),
            "secret `current` must be absent from JSON, not null: {token}");
        // a non-secret set var carries its value
        let sandbox = glass.iter().find(|e| e["name"] == "GLASS_SANDBOX").unwrap();
        assert_eq!(sandbox["current"], "strict");
        assert_eq!(sandbox["scope"], "all");
    }
}
