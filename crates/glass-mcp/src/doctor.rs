//! `glass doctor` aggregation: combine the backends' environment checks into one
//! [`Diagnosis`], shared by the `doctor` CLI subcommand and the `glass_doctor` MCP
//! tool. `--deep`/`deep` probes only the *default* backend's display (the one you'd
//! actually use), not every backend.

use glass_core::{Check, CheckStatus, Diagnosis, Section};

/// Build the full environment report. `deep` spawns and tears down the default
/// backend's headless display to verify it actually starts.
pub fn diagnose(deep: bool) -> Diagnosis {
    diagnose_inner(deep, None)
}

/// Like `diagnose`, plus an "audit" section describing the audit-log posture.
/// Used by the `doctor` CLI subcommand and the `glass_doctor` MCP tool.
pub fn diagnose_with_audit(deep: bool, report: &crate::audit::AuditReport) -> Diagnosis {
    diagnose_inner(deep, Some(report))
}

fn diagnose_inner(deep: bool, audit: Option<&crate::audit::AuditReport>) -> Diagnosis {
    let backend = crate::default_backend(std::env::var("GLASS_BACKEND").ok().as_deref());

    let backend_detail = match std::env::var("GLASS_BACKEND") {
        Ok(v) => format!("{backend} (GLASS_BACKEND = {v})"),
        Err(_) => format!("{backend} (GLASS_BACKEND unset)"),
    };
    let general = Section::new(
        "general",
        None,
        vec![
            Check::new("default backend", CheckStatus::Ok, backend_detail),
            Check::new("glass", CheckStatus::Ok, env!("CARGO_PKG_VERSION")),
        ],
    );

    #[cfg(feature = "network")]
    let network = Section::new(
        "network",
        None,
        vec![Check::new(
            "http transport",
            CheckStatus::Ok,
            "available — run `glass-mcp serve --http --addr <addr>` (token via --token-file/GLASS_TOKEN)",
        )],
    );
    #[cfg(not(feature = "network"))]
    let network = Section::new(
        "network",
        None,
        vec![Check::new("http transport", CheckStatus::Skip, "not built into this binary")],
    );

    // Only show sections for backends actually compiled into THIS binary — absent
    // backends (e.g. windows on a Linux build, or macos anywhere today) are omitted
    // rather than listed as "not built into this binary" placeholders. Accessibility is
    // per-OS (AT-SPI on Linux, UIA on Windows), so it ships with whichever OS is built.
    let mut sections = vec![general, network];

    #[cfg(target_os = "linux")]
    {
        sections.push(Section::new(
            "x11",
            Some("x11".into()),
            glass_x11::doctor::checks(deep && backend == "x11"),
        ));
        sections.push(Section::new(
            "wayland",
            Some("wayland".into()),
            glass_wayland::doctor::checks(deep && backend == "wayland"),
        ));
        // Sandbox is a host-level concern shared by both Linux backends; emit it
        // once here rather than once per backend.
        sections.push(Section::new("sandbox", None, glass_sandbox_linux::checks()));
        sections.push(Section::new("accessibility (linux)", None, glass_a11y_linux::doctor::checks()));
    }
    #[cfg(windows)]
    {
        sections.push(Section::new(
            "windows",
            Some("windows".into()),
            glass_windows::doctor::checks(deep && backend == "windows"),
        ));
        // In-OS containment posture (Sandboxie Classic) + VM-tier pointer.
        // Separate section, mirroring the Linux `sandbox` section.
        sections.push(Section::new("sandbox", None, glass_windows::doctor::sandbox_checks()));
        sections.push(Section::new(
            "accessibility (windows)",
            None,
            glass_a11y_windows::doctor::checks(deep),
        ));
    }

    if let Some(report) = audit {
        sections.push(audit_section(report));
    }

    Diagnosis::new(sections)
}

fn audit_section(report: &crate::audit::AuditReport) -> Section {
    let (status, detail) = if report.enabled {
        let mode = match report.content {
            crate::audit::ContentMode::None => "none",
            crate::audit::ContentMode::Redacted => "redacted",
            crate::audit::ContentMode::Full => "full",
        };
        // Invariant (set by audit::resolve/report_from_config): enabled ⇒ path is Some.
        let path = report.path.as_deref().expect("AuditReport: enabled implies a path");
        (CheckStatus::Ok, format!("on → {path} (content: {mode})"))
    } else {
        (CheckStatus::Skip, "off (set --audit-log/GLASS_AUDIT_LOG to enable)".to_string())
    };
    Section::new("audit", None, vec![Check::new("audit log", status, detail)])
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn diagnose_lists_only_compiled_in_backends() {
        // Passive (deep=false) — runs the real passive probes but only the structure
        // is asserted, so it's deterministic regardless of host.
        let d = diagnose(false);
        let titles: Vec<&str> = d.sections.iter().map(|s| s.title.as_str()).collect();
        // Only backends compiled into THIS binary get a section — no "not built into
        // this binary" placeholders. Accessibility is per-OS (AT-SPI on Linux, UIA on
        // Windows). macos has no backend yet, so it never appears.
        #[cfg(target_os = "linux")]
        assert_eq!(titles, ["general", "network", "x11", "wayland", "sandbox", "accessibility (linux)"]);
        #[cfg(windows)]
        assert_eq!(titles, ["general", "network", "windows", "sandbox", "accessibility (windows)"]);
        // The `network` section is always present (Ok when compiled in, else Skip).
        let net = d.sections.iter().find(|s| s.title == "network").expect("network section");
        assert_eq!(net.checks.len(), 1);
        // No section is a "not built into this binary" placeholder, and absent backends
        // are omitted entirely.
        assert!(!titles.contains(&"macos"));
        #[cfg(target_os = "linux")]
        assert!(!titles.contains(&"windows"));
        #[cfg(windows)]
        {
            assert!(!titles.contains(&"x11"));
            assert!(!titles.contains(&"wayland"));
        }
        let placeholder = d
            .sections
            .iter()
            .flat_map(|s| &s.checks)
            .any(|c| c.detail == "not built into this binary" && c.status == CheckStatus::Skip);
        // The only allowed "not built" placeholder is the network transport in a
        // stdio-only (no `network` feature) build — never a backend.
        #[cfg(feature = "network")]
        assert!(!placeholder, "no 'not built into this binary' placeholders when network is compiled in");
        #[cfg(not(feature = "network"))]
        let _ = placeholder; // network shows its own Skip line in the stdio-only build
    }

    #[test]
    fn diagnose_with_audit_reports_posture() {
        let on = crate::audit::AuditReport { enabled: true, path: Some("/v/g.jsonl".into()), content: crate::audit::ContentMode::Redacted, prefix_len: 8 };
        let t = diagnose_with_audit(false, &on).render_text("x11");
        assert!(t.contains("audit") && t.contains("/v/g.jsonl") && t.contains("redacted"), "{t}");
        let off = crate::audit::AuditReport { enabled: false, path: None, content: crate::audit::ContentMode::Redacted, prefix_len: 8 };
        let t = diagnose_with_audit(false, &off).render_text("x11").to_lowercase();
        assert!(t.contains("audit") && t.contains("off"), "{t}");
    }
}
