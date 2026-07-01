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
    // backends (e.g. windows on a Linux build, or macos on a non-macOS build) are
    // omitted rather than listed as "not built into this binary" placeholders.
    // Accessibility is per-OS (AT-SPI on Linux, UIA on Windows); macOS's grants live in
    // its own "macos" section instead. Android is the exception: its crate is
    // host-OS-agnostic and always compiled in, so its section is always emitted, gated
    // at runtime (see below) rather than by cfg.
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
    #[cfg(target_os = "macos")]
    {
        sections.push(Section::new("macos", Some("macos".into()), macos_checks(backend)));
    }

    // Android is host-OS-agnostic (drives an AVD over adb), so the crate is always compiled
    // in. Run its basic presence checks unconditionally — like the desktop backends — so the
    // doctor gives android pre-flight regardless of the (launch-frozen) GLASS_BACKEND. Only
    // the expensive/mutating deep probes (boot AVD, install agent) stay gated to the selected
    // backend. When android isn't active, soften any Fail to Warn so an irrelevant missing
    // adb/emulator doesn't fail the overall verdict for a desktop user.
    let android_selected = backend == "android";
    let mut android_checks = glass_android::doctor::checks(deep && android_selected);
    if !android_selected {
        soften_inactive_android(&mut android_checks);
    }
    sections.push(Section::new("android", Some("android".into()), android_checks));

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

/// When android isn't the active backend, its presence checks are advisory: downgrade any
/// `Fail` to `Warn` (noting why) so a missing adb/emulator — irrelevant to the current
/// backend — doesn't fail the overall diagnosis. The actual status is still reported.
fn soften_inactive_android(checks: &mut [Check]) {
    for c in checks {
        if c.status == CheckStatus::Fail {
            c.status = CheckStatus::Warn;
            c.detail = format!("{} (only required when the android backend is selected)", c.detail);
        }
    }
}

/// macOS checks: the two TCC grants (Screen Recording, Accessibility), the console
/// session's lock/display-sleep state, and the resolved backend. Pure — takes
/// already-gathered facts, makes no OS calls itself — so it's unit-tested without
/// needing real grants or a locked/awake session; [`macos_checks`] gathers the real
/// facts via `glass_macos`. An asleep/locked session is a `Warn`, not a `Fail`: it's
/// recoverable in-place (`caffeinate -d`) and shouldn't fail the whole doctor run the
/// way a genuinely missing grant does.
#[cfg(target_os = "macos")]
fn macos_checks_from(
    resolved_backend: &str,
    screen_recording: bool,
    accessibility: bool,
    session_locked: bool,
) -> Vec<Check> {
    let mut v = Vec::new();
    v.push(if screen_recording {
        Check::new("Screen Recording", CheckStatus::Ok, "granted")
    } else {
        Check::new(
            "Screen Recording",
            CheckStatus::Fail,
            "not granted — capture will fail with a permission error",
        )
        .with_remedy(glass_macos::screen_recording_remedy())
    });
    v.push(if accessibility {
        Check::new("Accessibility", CheckStatus::Ok, "granted")
    } else {
        Check::new(
            "Accessibility",
            CheckStatus::Fail,
            "not granted — window management and input injection will fail",
        )
        .with_remedy(glass_macos::accessibility_remedy())
    });
    v.push(if session_locked {
        Check::new(
            "display awake",
            CheckStatus::Warn,
            "console session is locked/asleep — capture and input are silently suppressed while it is",
        )
        .with_remedy("run `caffeinate -d` in the console session to keep the display awake (no sudo needed)")
    } else {
        Check::new("display awake", CheckStatus::Ok, "session unlocked")
    });
    v.push(if resolved_backend == "macos" {
        Check::new("backend", CheckStatus::Ok, "resolved to macos")
    } else {
        Check::new(
            "backend",
            CheckStatus::Warn,
            format!("resolved backend is {resolved_backend}, not macos (GLASS_BACKEND override?)"),
        )
    });
    v
}

/// Gather the real macOS facts (TCC grants, session lock state) and map them via
/// [`macos_checks_from`].
#[cfg(target_os = "macos")]
fn macos_checks(resolved_backend: &str) -> Vec<Check> {
    macos_checks_from(
        resolved_backend,
        glass_macos::screen_recording_granted(),
        glass_macos::accessibility_granted(),
        glass_macos::session_locked(),
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn inactive_android_fails_soften_to_warn() {
        let mut checks = vec![
            Check::new("adb", CheckStatus::Fail, "`adb` not found").with_remedy("install platform-tools"),
            Check::new("emulator", CheckStatus::Warn, "no AVDs listed"),
            Check::new("agent", CheckStatus::Skip, "not configured"),
            Check::new("device", CheckStatus::Ok, "1 online"),
        ];
        soften_inactive_android(&mut checks);
        assert_eq!(checks[0].status, CheckStatus::Warn); // Fail → Warn
        assert!(checks[0].detail.contains("only required when the android backend is selected"));
        assert_eq!(checks[0].remedy.as_deref(), Some("install platform-tools")); // remedy preserved
        assert_eq!(checks[1].status, CheckStatus::Warn); // Warn untouched
        assert_eq!(checks[2].status, CheckStatus::Skip); // Skip untouched
        assert_eq!(checks[3].status, CheckStatus::Ok); // Ok untouched
    }

    #[test]
    fn diagnose_lists_only_compiled_in_backends() {
        // Passive (deep=false) — runs the real passive probes but only the structure
        // is asserted, so it's deterministic regardless of host.
        let d = diagnose(false);
        let titles: Vec<&str> = d.sections.iter().map(|s| s.title.as_str()).collect();
        // Platform-gated backends compiled into THIS binary get a section; android is
        // always present (host-OS-agnostic crate) via a runtime gate. No "not built into
        // this binary" placeholders. Accessibility is per-OS (AT-SPI on Linux, UIA on
        // Windows); macOS's grants (Screen Recording, Accessibility) live inside its own
        // "macos" section rather than a separate accessibility section.
        #[cfg(target_os = "linux")]
        assert_eq!(titles, ["general", "network", "x11", "wayland", "sandbox", "accessibility (linux)", "android"]);
        #[cfg(windows)]
        assert_eq!(titles, ["general", "network", "windows", "sandbox", "accessibility (windows)", "android"]);
        #[cfg(target_os = "macos")]
        assert_eq!(titles, ["general", "network", "macos", "android"]);
        // Android's section is always present and non-empty — its basic presence checks now
        // run unconditionally (deep probes gated to the selected backend; Fails softened to
        // Warn when android isn't active). Asserting non-empty catches accidental removal of
        // the section, without depending on which backend the ambient env resolves to.
        let android = d.sections.iter().find(|s| s.title == "android").expect("android section");
        assert_eq!(android.backend.as_deref(), Some("android"));
        assert!(!android.checks.is_empty());
        // The `network` section is always present (Ok when compiled in, else Skip).
        let net = d.sections.iter().find(|s| s.title == "network").expect("network section");
        assert_eq!(net.checks.len(), 1);
        // No section is a "not built into this binary" placeholder, and absent backends
        // are omitted entirely.
        #[cfg(not(target_os = "macos"))]
        assert!(!titles.contains(&"macos"));
        #[cfg(target_os = "linux")]
        assert!(!titles.contains(&"windows"));
        #[cfg(windows)]
        {
            assert!(!titles.contains(&"x11"));
            assert!(!titles.contains(&"wayland"));
        }
        #[cfg(target_os = "macos")]
        {
            assert!(!titles.contains(&"x11"));
            assert!(!titles.contains(&"wayland"));
            assert!(!titles.contains(&"windows"));
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

    #[cfg(target_os = "macos")]
    mod macos {
        use super::*;

        #[test]
        fn all_granted_awake_and_resolved_is_all_ok() {
            let checks = macos_checks_from("macos", true, true, false);
            assert!(checks.iter().all(|c| c.status == CheckStatus::Ok), "{checks:?}");
        }

        #[test]
        fn missing_screen_recording_fails_with_the_shared_remedy() {
            let checks = macos_checks_from("macos", false, true, false);
            let c = checks.iter().find(|c| c.name == "Screen Recording").unwrap();
            assert_eq!(c.status, CheckStatus::Fail);
            // Same wording `preflight`'s `PermissionDenied` error uses — no separate,
            // driftable copy in glass-mcp.
            assert_eq!(c.remedy.as_deref(), Some(glass_macos::screen_recording_remedy()));
        }

        #[test]
        fn missing_accessibility_fails_with_the_shared_remedy() {
            let checks = macos_checks_from("macos", true, false, false);
            let c = checks.iter().find(|c| c.name == "Accessibility").unwrap();
            assert_eq!(c.status, CheckStatus::Fail);
            assert_eq!(c.remedy.as_deref(), Some(glass_macos::accessibility_remedy()));
        }

        #[test]
        fn locked_session_warns_and_names_caffeinate() {
            let checks = macos_checks_from("macos", true, true, true);
            let c = checks.iter().find(|c| c.name == "display awake").unwrap();
            assert_eq!(c.status, CheckStatus::Warn);
            assert!(c.remedy.as_deref().unwrap().contains("caffeinate -d"), "{c:?}");
        }

        #[test]
        fn locked_session_does_not_fail_the_whole_doctor_run() {
            // The display-awake WARN must not escalate `Diagnosis::overall` to FAIL —
            // it's recoverable in place, unlike a genuinely missing grant.
            let checks = macos_checks_from("macos", true, true, true);
            let d = Diagnosis::new(vec![Section::new("macos", Some("macos".into()), checks)]);
            assert_eq!(d.overall("macos"), CheckStatus::Warn);
            assert_eq!(d.exit_code("macos"), 0);
        }

        #[test]
        fn missing_grant_fails_the_whole_doctor_run_when_macos_is_the_default_backend() {
            let checks = macos_checks_from("macos", false, true, false);
            let d = Diagnosis::new(vec![Section::new("macos", Some("macos".into()), checks)]);
            assert_eq!(d.overall("macos"), CheckStatus::Fail);
            assert_eq!(d.exit_code("macos"), 1);
        }

        #[test]
        fn backend_mismatch_warns_without_naming_it_a_failure() {
            let checks = macos_checks_from("android", true, true, false);
            let c = checks.iter().find(|c| c.name == "backend").unwrap();
            assert_eq!(c.status, CheckStatus::Warn);
        }
    }
}
