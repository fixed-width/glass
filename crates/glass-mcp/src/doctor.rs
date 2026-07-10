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
        vec![Check::new(
            "http transport",
            CheckStatus::Skip,
            "not built into this binary",
        )],
    );

    // Only show sections for backends actually compiled into THIS binary — absent
    // backends (e.g. windows on a Linux build, or macos on a non-macOS build) are
    // omitted rather than listed as "not built into this binary" placeholders.
    // Accessibility is per-OS (AT-SPI on Linux, UIA on Windows); macOS instead gets three
    // sections below — "macos" (the platform backend's own TCC posture), "sandbox" (Seatbelt
    // containment posture, mirroring the Linux/Windows "sandbox" sections), and
    // "accessibility (macos)" (the a11y-tool reader's readiness, kept separate from "macos" —
    // see the comment there for why). Android is the exception: it shells out to a separate
    // SDK's own tools (adb/emulator) rather than linking an OS framework, so it's
    // host-OS-agnostic and always compiled in — its section is always emitted, gated at
    // runtime (see below) rather than by cfg. iOS also shells out to a separate SDK's tools
    // (`xcrun simctl`), but only macOS can actually run them, and `glass-ios` pulls in an
    // `image` PNG codec chain that a non-macOS binary has no use for — so glass-mcp only
    // depends on `glass-ios` on macOS, and its section is compiled in accordingly.
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
        sections.push(Section::new(
            "accessibility (linux)",
            None,
            glass_a11y_linux::doctor::checks(),
        ));
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
        sections.push(Section::new(
            "sandbox",
            None,
            glass_windows::doctor::sandbox_checks(),
        ));
        sections.push(Section::new(
            "accessibility (windows)",
            None,
            glass_a11y_windows::doctor::checks(deep),
        ));
    }
    #[cfg(target_os = "macos")]
    {
        sections.push(Section::new(
            "macos",
            Some("macos".into()),
            macos_checks(backend),
        ));
        // Mirrors the Linux/Windows "sandbox" section: Seatbelt containment posture.
        sections.push(Section::new("sandbox", None, glass_sandbox_macos::checks()));
        // Mirrors "accessibility (linux)"/"accessibility (windows)": a dedicated section
        // for the a11y-tool reader itself (glass_a11y_snapshot/marks/click_element/
        // set_value), distinct from the "macos" section above which covers the platform
        // backend's own TCC posture (Screen Recording, session state, ...). The
        // Accessibility grant check is intentionally duplicated between the two sections
        // — here it answers "will the a11y tools work", there it answers "is this Mac
        // set up at all" — both reuse the same `glass_macos::accessibility_granted()`
        // fact and remedy string, so there's no risk of the two drifting apart.
        sections.push(Section::new(
            "accessibility (macos)",
            None,
            macos_a11y_checks(glass_macos::accessibility_granted()),
        ));
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
        soften_inactive_android(&mut android_checks, deep);
    }
    sections.push(Section::new(
        "android",
        Some("android".into()),
        android_checks,
    ));

    // iOS Simulator backend: unlike android above, `glass-ios` is only a dependency of
    // glass-mcp on macOS (the only host that can drive `xcrun simctl` — see this crate's
    // Cargo.toml), so its section only exists in a macOS build. Soften Fails to Warns when
    // ios isn't active so a Mac not currently driving iOS doesn't fail the overall doctor
    // verdict.
    #[cfg(target_os = "macos")]
    {
        let ios_selected = backend == "ios";
        let mut ios_checks = glass_ios::doctor::checks(deep && ios_selected);
        // The companion drives input + the accessibility reader; only worth reporting when
        // ios is actually the backend in play, mirroring android's gated deep probes. On
        // --deep, probe it for real (spawn against a booted sim, else a bounded self-test);
        // otherwise just report resolvable-on-PATH presence.
        if ios_selected {
            let companion = if deep {
                companion_deep_check(glass_ios::doctor::probe_companion())
            } else {
                idb_companion_check(glass_ios::doctor::companion_present())
            };
            ios_checks.push(companion);
        }
        if !ios_selected {
            soften_inactive_ios(&mut ios_checks);
        }
        sections.push(Section::new("ios", Some("ios".into()), ios_checks));
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
        let path = report
            .path
            .as_deref()
            .expect("AuditReport: enabled implies a path");
        (CheckStatus::Ok, format!("on → {path} (content: {mode})"))
    } else {
        (
            CheckStatus::Skip,
            "off (set --audit-log/GLASS_AUDIT_LOG to enable)".to_string(),
        )
    };
    Section::new("audit", None, vec![Check::new("audit log", status, detail)])
}

/// Downgrade any `Fail` check to `Warn`, noting that it's only required when `backend` is
/// selected — shared by [`soften_inactive_android`] and [`soften_inactive_ios`] so a missing
/// tool for a backend the user isn't driving doesn't fail the overall diagnosis. The actual
/// status is still reported, just softened.
fn soften_inactive_fails(checks: &mut [Check], backend: &str) {
    for c in checks {
        if c.status == CheckStatus::Fail {
            c.status = CheckStatus::Warn;
            c.detail = format!(
                "{} (only required when the {backend} backend is selected)",
                c.detail
            );
        }
    }
}

/// When android isn't the active backend, its presence checks are advisory, so adjust them
/// to read honestly for a user on another backend: soften any `Fail` via
/// [`soften_inactive_fails`], and, when `--deep` *was* requested, correct the deep-capture
/// probes' skip reason — they were gated off because android isn't the selected backend, not
/// because `--deep` was missing. The android crate only sees the collapsed
/// `deep && android_selected` bool, so it emits its "run with --deep" hint (which the user
/// already did); point at the real gate instead.
fn soften_inactive_android(checks: &mut [Check], deep_requested: bool) {
    soften_inactive_fails(checks, "android");
    if deep_requested {
        for c in checks {
            if c.status == CheckStatus::Skip
                && c.detail == glass_android::doctor::DEEP_NOT_REQUESTED_DETAIL
            {
                c.detail =
                    "deep probes run only for the selected backend — set GLASS_BACKEND=android \
                     to probe capture"
                        .to_string();
            }
        }
    }
}

/// When ios isn't the active backend, its presence checks are advisory — same rationale as
/// [`soften_inactive_android`], via the shared [`soften_inactive_fails`]. Unlike android, ios
/// has no deep-probe skip message to correct: `glass_ios::doctor::checks` accepts `deep` only
/// for signature parity (iOS has no expensive probe of its own), so it never emits a
/// "run with --deep" hint that would need re-pointing. Only used from the macOS-only ios
/// section above (see this crate's Cargo.toml for why `glass-ios` itself is macOS-only).
#[cfg(target_os = "macos")]
fn soften_inactive_ios(checks: &mut [Check]) {
    soften_inactive_fails(checks, "ios");
}

/// The shared `idb_companion` install remedy.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
const IDB_COMPANION_REMEDY: &str =
    "brew tap facebook/fb && brew trust facebook/fb && brew install idb-companion";

/// The check for a resolvable/unresolvable `idb_companion` on the *passive* (non-`--deep`)
/// path — takes the already-resolved presence fact so it's unit-tested without touching
/// PATH/env; `glass_ios::doctor::companion_present` gathers the real fact on macOS. A missing
/// companion is a **Fail**, not a Warn: unlike android — which keeps barebones function
/// without its companions — the iOS companion is required to drive apps at all, so without it
/// iOS is observe-only (unusable for development). Because this check is only added when iOS
/// is the *selected* backend, the Fail reddens the verdict only for someone actually driving
/// iOS. Kept out of `#[cfg]` (only its caller is macOS-only) so the test still runs on every
/// host.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
pub(crate) fn idb_companion_check(found: bool) -> Check {
    if found {
        Check::new(
            "idb_companion",
            CheckStatus::Ok,
            "idb_companion found — input + accessibility are available",
        )
    } else {
        idb_companion_not_found_check()
    }
}

/// The Fail check for an absent `idb_companion`, shared by the passive presence path and the
/// `--deep` probe's `CompanionProbe::NotFound` mapping.
#[cfg_attr(not(any(target_os = "macos", test)), allow(dead_code))]
fn idb_companion_not_found_check() -> Check {
    Check::new(
        "idb_companion",
        CheckStatus::Fail,
        "idb_companion not found — input + accessibility are unavailable (iOS cannot drive apps)",
    )
    .with_remedy(IDB_COMPANION_REMEDY)
}

/// Map a `--deep` iOS companion probe ([`glass_ios::doctor::probe_companion`]) to a `Check`.
/// Pure — the probe's I/O is done by the caller — so it's unit-tested per variant without a
/// real companion. Broken (`FailedToStart`/`SelfTestFailed`) or missing (`NotFound`) ⇒ Fail:
/// iOS cannot drive apps without the companion. Unverified (`SelfTestOk` — the binary runs but
/// no booted simulator was available to exercise a real start) ⇒ Warn. macOS-gated: it names a
/// `glass-ios` type, and glass-mcp only depends on glass-ios on macOS (mirrors `macos_checks_from`).
#[cfg(target_os = "macos")]
pub(crate) fn companion_deep_check(probe: glass_ios::doctor::CompanionProbe) -> Check {
    use glass_ios::doctor::CompanionProbe;
    match probe {
        CompanionProbe::Started => Check::new(
            "idb_companion",
            CheckStatus::Ok,
            "started and served its gRPC socket — input + accessibility are available",
        ),
        CompanionProbe::SelfTestOk => Check::new(
            "idb_companion",
            CheckStatus::Warn,
            "binary runs, but no booted simulator was available to verify a real start — \
             boot one and re-run with --deep to exercise the companion",
        ),
        CompanionProbe::FailedToStart(cause) => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            format!(
                "failed to start: {cause} — input + accessibility are unavailable (iOS is observe-only)"
            ),
        )
        .with_remedy(IDB_COMPANION_REMEDY),
        CompanionProbe::SelfTestFailed(cause) => Check::new(
            "idb_companion",
            CheckStatus::Fail,
            format!("binary failed to execute: {cause} — input + accessibility are unavailable"),
        )
        .with_remedy(IDB_COMPANION_REMEDY),
        CompanionProbe::NotFound => idb_companion_not_found_check(),
    }
}

/// macOS checks: the two TCC grants (Screen Recording, Accessibility), the console
/// session's three-way state (unlocked/locked/nobody-logged-in), and the resolved
/// backend. Pure — takes already-gathered facts, makes no OS calls itself — so it's
/// unit-tested without needing real grants or a particular session state;
/// [`macos_checks`] gathers the real facts via `glass_macos`. A locked/asleep session
/// is a `Warn`, not a `Fail`: it's recoverable in-place (`caffeinate -d`), unlike a
/// genuinely missing grant. No account being logged in at the console at all
/// (`SessionState::NoSession` — see `glass_macos::session`, verified to be a
/// console-wide fact, not merely "called over SSH") is also a `Warn`: distinct from
/// both, not fixable by unlocking, but still surfaced without failing the whole
/// doctor run over what's usually a launch-configuration issue rather than a broken
/// install.
#[cfg(target_os = "macos")]
fn macos_checks_from(
    resolved_backend: &str,
    screen_recording: bool,
    accessibility: bool,
    session_state: glass_macos::SessionState,
) -> Vec<Check> {
    vec![
        if screen_recording {
            Check::new("Screen Recording", CheckStatus::Ok, "granted")
        } else {
            Check::new(
                "Screen Recording",
                CheckStatus::Fail,
                "not granted — capture will fail with a permission error",
            )
            .with_remedy(glass_macos::screen_recording_remedy())
            .with_remedy_action(format!("open {}", glass_macos::screen_recording_pane_url()))
        },
        if accessibility {
            Check::new("Accessibility", CheckStatus::Ok, "granted")
        } else {
            Check::new(
                "Accessibility",
                CheckStatus::Fail,
                "not granted — window management and input injection will fail",
            )
            .with_remedy(glass_macos::accessibility_remedy())
            .with_remedy_action(format!("open {}", glass_macos::accessibility_pane_url()))
        },
        match session_state {
            glass_macos::SessionState::Unlocked => Check::new("display awake", CheckStatus::Ok, "session unlocked"),
            glass_macos::SessionState::Locked => Check::new(
                "display awake",
                CheckStatus::Warn,
                "console session is locked/asleep — capture and input are silently suppressed while it is",
            )
            .with_remedy("run `caffeinate -d` in the console session to keep the display awake (no sudo needed)"),
            glass_macos::SessionState::NoSession => Check::new(
                "display awake",
                CheckStatus::Warn,
                "no account is logged in at the console (or it's sitting at the login window) — capture and \
                 input need an actual GUI login, not just an unlocked screen; this is NOT about how glass-mcp \
                 itself was launched (a bare-SSH process still sees a real logged-in console's state fine)",
            )
            .with_remedy(
                "log in at the console for this account, then run glass-mcp as a gui/$(id -u) LaunchAgent: \
                 `launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tech.fixedwidth.glass.plist` \
                 (see docs/how-to/build-from-source.md)",
            ),
        },
        // The `general` section already prints the resolved default backend
        // (`default backend`, above); this doesn't re-discover that fact, it just
        // views it through a macOS-specific lens — is the backend this *macOS*
        // binary resolved to actually macOS, e.g. flagging a `GLASS_BACKEND`
        // override that names a backend not even compiled into this build.
        if resolved_backend == "macos" {
            Check::new("backend", CheckStatus::Ok, "resolved to macos")
        } else {
            Check::new(
                "backend",
                CheckStatus::Warn,
                format!("resolved backend is {resolved_backend}, not macos (GLASS_BACKEND override?)"),
            )
        },
    ]
}

/// Gather the real macOS facts (TCC grants, session state) and map them via
/// [`macos_checks_from`].
#[cfg(target_os = "macos")]
fn macos_checks(resolved_backend: &str) -> Vec<Check> {
    macos_checks_from(
        resolved_backend,
        glass_macos::screen_recording_granted(),
        glass_macos::accessibility_granted(),
        glass_macos::session_state(),
    )
}

/// The "accessibility (macos)" section: the `glass-a11y-macos` reader (AXUIElement) is a
/// system framework, not an optional install like Linux's AT-SPI daemon or a service that
/// needs spinning up — so it's always present in a macOS build, and the only real
/// precondition left to report is the Accessibility TCC grant itself. Pure — takes the
/// already-gathered grant fact, makes no OS calls — so it's unit-tested without needing a
/// real grant, mirroring `glass_a11y_linux::doctor::a11y_checks`/
/// `glass_a11y_windows::doctor::a11y_checks`.
#[cfg(target_os = "macos")]
fn macos_a11y_checks(accessibility_granted: bool) -> Vec<Check> {
    vec![
        Check::new(
            "a11y reader",
            CheckStatus::Ok,
            "AXUIElement reader available — glass_a11y_snapshot / glass_a11y_marks / \
             glass_click_element / glass_set_value will work once Accessibility is granted (see below)",
        ),
        if accessibility_granted {
            Check::new("Accessibility", CheckStatus::Ok, "granted")
        } else {
            Check::new(
                "Accessibility",
                CheckStatus::Fail,
                "not granted — glass_a11y_snapshot / glass_a11y_marks / glass_click_element / \
                 glass_set_value will fail with a permission error",
            )
            .with_remedy(glass_macos::accessibility_remedy())
            .with_remedy_action(format!("open {}", glass_macos::accessibility_pane_url()))
        },
    ]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn idb_companion_check_fails_when_absent_and_oks_when_present() {
        // The iOS companion is *required* to drive apps (unlike android's optional companions);
        // without it iOS is observe-only, which is not a usable dev workflow — so absent is a
        // Fail, not a Warn.
        let absent = idb_companion_check(false);
        assert_eq!(absent.status, CheckStatus::Fail);
        assert_eq!(absent.remedy.as_deref(), Some(IDB_COMPANION_REMEDY));
        let present = idb_companion_check(true);
        assert_eq!(present.status, CheckStatus::Ok);
        assert_eq!(present.remedy, None);
    }

    #[test]
    fn inactive_android_fails_soften_to_warn() {
        let mut checks = vec![
            Check::new("adb", CheckStatus::Fail, "`adb` not found")
                .with_remedy("install platform-tools"),
            Check::new("emulator", CheckStatus::Warn, "no AVDs listed"),
            Check::new("agent", CheckStatus::Skip, "not configured"),
            Check::new("device", CheckStatus::Ok, "1 online"),
        ];
        soften_inactive_android(&mut checks, false);
        assert_eq!(checks[0].status, CheckStatus::Warn); // Fail → Warn
        assert!(checks[0]
            .detail
            .contains("only required when the android backend is selected"));
        assert_eq!(checks[0].remedy.as_deref(), Some("install platform-tools")); // remedy preserved
        assert_eq!(checks[1].status, CheckStatus::Warn); // Warn untouched
        assert_eq!(checks[2].status, CheckStatus::Skip); // Skip untouched
        assert_eq!(checks[3].status, CheckStatus::Ok); // Ok untouched
    }

    #[test]
    fn inactive_android_deep_requested_corrects_capture_skip_message() {
        // --deep WAS passed, but android isn't the selected backend, so its deep probes were
        // gated off. The android crate can only emit its "run with --deep" hint (it sees the
        // collapsed bool) — which is misleading, since the user already passed --deep. The
        // aggregator must correct it to point at the real gate (GLASS_BACKEND), not the flag.
        let mut checks = vec![Check::new(
            "screencap",
            CheckStatus::Skip,
            glass_android::doctor::DEEP_NOT_REQUESTED_DETAIL,
        )];
        soften_inactive_android(&mut checks, true);
        assert_eq!(checks[0].status, CheckStatus::Skip); // still skipped, just honestly
        assert!(
            !checks[0].detail.contains("--deep"),
            "must not tell the user to pass --deep — they already did: {}",
            checks[0].detail
        );
        assert!(
            checks[0].detail.contains("GLASS_BACKEND"),
            "should point at the real gate: {}",
            checks[0].detail
        );
    }

    #[cfg(target_os = "macos")]
    #[test]
    fn inactive_ios_fails_soften_to_warn() {
        let mut checks = vec![
            Check::new("xcode", CheckStatus::Fail, "no active developer directory")
                .with_remedy("install Xcode from the App Store"),
            Check::new("device", CheckStatus::Ok, "1 iPhone simulator(s) available"),
        ];
        soften_inactive_ios(&mut checks);
        assert_eq!(checks[0].status, CheckStatus::Warn); // Fail → Warn
        assert!(checks[0]
            .detail
            .contains("only required when the ios backend is selected"));
        assert_eq!(
            checks[0].remedy.as_deref(),
            Some("install Xcode from the App Store")
        ); // remedy preserved
        assert_eq!(checks[1].status, CheckStatus::Ok); // Ok untouched
    }

    #[test]
    fn inactive_android_deep_not_requested_keeps_run_with_deep_hint() {
        // --deep was NOT passed: the "run with --deep" hint is the correct next step, so it
        // must be left intact (only the `deep_requested` case is misleading).
        let mut checks = vec![Check::new(
            "screencap",
            CheckStatus::Skip,
            glass_android::doctor::DEEP_NOT_REQUESTED_DETAIL,
        )];
        soften_inactive_android(&mut checks, false);
        assert_eq!(
            checks[0].detail,
            glass_android::doctor::DEEP_NOT_REQUESTED_DETAIL
        );
    }

    #[test]
    fn diagnose_lists_only_compiled_in_backends() {
        // Passive (deep=false) — runs the real passive probes but only the structure
        // is asserted, so it's deterministic regardless of host.
        let d = diagnose(false);
        let titles: Vec<&str> = d.sections.iter().map(|s| s.title.as_str()).collect();
        // Platform-gated backends compiled into THIS binary get a section; android is always
        // present (a host-OS-agnostic crate) via a runtime gate. iOS is compiled into
        // glass-mcp — and so gets a section — only on macOS (see this crate's Cargo.toml).
        // No "not built into this binary" placeholders. Accessibility is per-OS (AT-SPI on
        // Linux, UIA on Windows); macOS's grants (Screen Recording, Accessibility) live
        // inside its own "macos" section rather than a separate accessibility section.
        #[cfg(target_os = "linux")]
        assert_eq!(
            titles,
            [
                "general",
                "network",
                "x11",
                "wayland",
                "sandbox",
                "accessibility (linux)",
                "android"
            ]
        );
        #[cfg(windows)]
        assert_eq!(
            titles,
            [
                "general",
                "network",
                "windows",
                "sandbox",
                "accessibility (windows)",
                "android"
            ]
        );
        #[cfg(target_os = "macos")]
        assert_eq!(
            titles,
            [
                "general",
                "network",
                "macos",
                "sandbox",
                "accessibility (macos)",
                "android",
                "ios"
            ]
        );
        // Android's section is always present and non-empty — its basic presence checks run
        // unconditionally (deep probes gated to the selected backend; Fails softened to Warn
        // when not active). Asserting non-empty catches accidental removal of the section,
        // without depending on which backend the ambient env resolves to.
        let android = d
            .sections
            .iter()
            .find(|s| s.title == "android")
            .expect("android section");
        assert_eq!(android.backend.as_deref(), Some("android"));
        assert!(!android.checks.is_empty());
        // iOS's section follows the same non-empty shape, but only exists on macOS.
        #[cfg(target_os = "macos")]
        {
            let ios = d
                .sections
                .iter()
                .find(|s| s.title == "ios")
                .expect("ios section");
            assert_eq!(ios.backend.as_deref(), Some("ios"));
            assert!(!ios.checks.is_empty());
        }
        #[cfg(not(target_os = "macos"))]
        assert!(!titles.contains(&"ios"));
        // The `network` section is always present (Ok when compiled in, else Skip).
        let net = d
            .sections
            .iter()
            .find(|s| s.title == "network")
            .expect("network section");
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
        assert!(
            !placeholder,
            "no 'not built into this binary' placeholders when network is compiled in"
        );
        #[cfg(not(feature = "network"))]
        let _ = placeholder; // network shows its own Skip line in the stdio-only build
    }

    #[test]
    fn missing_companion_fails_the_ios_verdict() {
        // A Fail companion check in the ios section must escalate the overall verdict to Fail
        // (exit code 1) when ios is the backend — iOS is unusable without the companion.
        let ios = Section::new("ios", Some("ios".into()), vec![idb_companion_check(false)]);
        let d = Diagnosis::new(vec![ios]);
        assert_eq!(d.overall("ios"), CheckStatus::Fail);
        assert_eq!(d.exit_code("ios"), 1);
    }

    #[test]
    fn diagnose_with_audit_reports_posture() {
        let on = crate::audit::AuditReport {
            enabled: true,
            path: Some("/v/g.jsonl".into()),
            content: crate::audit::ContentMode::Redacted,
            prefix_len: 8,
        };
        let t = diagnose_with_audit(false, &on).render_text("x11");
        assert!(
            t.contains("audit") && t.contains("/v/g.jsonl") && t.contains("redacted"),
            "{t}"
        );
        let off = crate::audit::AuditReport {
            enabled: false,
            path: None,
            content: crate::audit::ContentMode::Redacted,
            prefix_len: 8,
        };
        let t = diagnose_with_audit(false, &off)
            .render_text("x11")
            .to_lowercase();
        assert!(t.contains("audit") && t.contains("off"), "{t}");
    }

    #[cfg(target_os = "macos")]
    mod macos {
        use super::*;

        use glass_macos::SessionState;

        #[test]
        fn all_granted_awake_and_resolved_is_all_ok() {
            let checks = macos_checks_from("macos", true, true, SessionState::Unlocked);
            assert!(
                checks.iter().all(|c| c.status == CheckStatus::Ok),
                "{checks:?}"
            );
        }

        #[test]
        fn missing_screen_recording_fails_with_the_shared_remedy() {
            let checks = macos_checks_from("macos", false, true, SessionState::Unlocked);
            let c = checks
                .iter()
                .find(|c| c.name == "Screen Recording")
                .unwrap();
            assert_eq!(c.status, CheckStatus::Fail);
            // Same wording `preflight`'s `PermissionDenied` error uses — no separate,
            // driftable copy in glass-mcp.
            assert_eq!(
                c.remedy.as_deref(),
                Some(glass_macos::screen_recording_remedy())
            );
        }

        #[test]
        fn missing_screen_recording_points_at_the_screen_capture_pane() {
            let checks = macos_checks_from("macos", false, true, SessionState::Unlocked);
            let c = checks
                .iter()
                .find(|c| c.name == "Screen Recording")
                .unwrap();
            assert_eq!(
                c.remedy_action.as_deref(),
                Some(format!("open {}", glass_macos::screen_recording_pane_url()).as_str())
            );
        }

        #[test]
        fn missing_accessibility_fails_with_the_shared_remedy() {
            let checks = macos_checks_from("macos", true, false, SessionState::Unlocked);
            let c = checks.iter().find(|c| c.name == "Accessibility").unwrap();
            assert_eq!(c.status, CheckStatus::Fail);
            assert_eq!(
                c.remedy.as_deref(),
                Some(glass_macos::accessibility_remedy())
            );
        }

        #[test]
        fn missing_accessibility_points_at_the_accessibility_pane() {
            let checks = macos_checks_from("macos", true, false, SessionState::Unlocked);
            let c = checks.iter().find(|c| c.name == "Accessibility").unwrap();
            assert_eq!(
                c.remedy_action.as_deref(),
                Some(format!("open {}", glass_macos::accessibility_pane_url()).as_str())
            );
        }

        #[test]
        fn locked_session_warns_and_names_caffeinate() {
            let checks = macos_checks_from("macos", true, true, SessionState::Locked);
            let c = checks.iter().find(|c| c.name == "display awake").unwrap();
            assert_eq!(c.status, CheckStatus::Warn);
            assert!(
                c.remedy.as_deref().unwrap().contains("caffeinate -d"),
                "{c:?}"
            );
        }

        #[test]
        fn locked_session_does_not_fail_the_whole_doctor_run() {
            // The display-awake WARN must not escalate `Diagnosis::overall` to FAIL —
            // it's recoverable in place, unlike a genuinely missing grant.
            let checks = macos_checks_from("macos", true, true, SessionState::Locked);
            let d = Diagnosis::new(vec![Section::new("macos", Some("macos".into()), checks)]);
            assert_eq!(d.overall("macos"), CheckStatus::Warn);
            assert_eq!(d.exit_code("macos"), 0);
        }

        #[test]
        fn no_session_warns_and_names_launchctl_bootstrap() {
            // The distinct NULL-dict case — nobody logged in at the console at all
            // (verified to be a console-wide fact, not "this process happens to be
            // over SSH" — see `glass_macos::session`'s module docs), not a present-
            // but-unlocked one. Must not collapse into the `Unlocked` Ok case.
            let checks = macos_checks_from("macos", true, true, SessionState::NoSession);
            let c = checks.iter().find(|c| c.name == "display awake").unwrap();
            assert_eq!(c.status, CheckStatus::Warn);
            assert!(
                c.detail.contains("no account is logged in at the console"),
                "{c:?}"
            );
            assert!(
                c.remedy
                    .as_deref()
                    .unwrap()
                    .contains("launchctl bootstrap gui/"),
                "{c:?}"
            );
        }

        #[test]
        fn no_session_does_not_fail_the_whole_doctor_run() {
            // Also recoverable (relaunch as a LaunchAgent) rather than a broken
            // install, so it's a Warn like `Locked`, not escalated to Fail.
            let checks = macos_checks_from("macos", true, true, SessionState::NoSession);
            let d = Diagnosis::new(vec![Section::new("macos", Some("macos".into()), checks)]);
            assert_eq!(d.overall("macos"), CheckStatus::Warn);
            assert_eq!(d.exit_code("macos"), 0);
        }

        #[test]
        fn missing_grant_fails_the_whole_doctor_run_when_macos_is_the_default_backend() {
            let checks = macos_checks_from("macos", false, true, SessionState::Unlocked);
            let d = Diagnosis::new(vec![Section::new("macos", Some("macos".into()), checks)]);
            assert_eq!(d.overall("macos"), CheckStatus::Fail);
            assert_eq!(d.exit_code("macos"), 1);
        }

        #[test]
        fn backend_mismatch_warns_without_naming_it_a_failure() {
            let checks = macos_checks_from("android", true, true, SessionState::Unlocked);
            let c = checks.iter().find(|c| c.name == "backend").unwrap();
            assert_eq!(c.status, CheckStatus::Warn);
        }

        #[test]
        fn a11y_reader_is_always_reported_present() {
            // The AXUIElement reader is a system framework compiled unconditionally into
            // a macOS build — unlike Linux's AT-SPI daemon, there's no "not installed"
            // state to detect, so this line is always Ok regardless of the grant.
            let checks = macos_a11y_checks(false);
            let reader = checks.iter().find(|c| c.name == "a11y reader").unwrap();
            assert_eq!(reader.status, CheckStatus::Ok);
        }

        #[test]
        fn a11y_granted_is_all_ok() {
            let checks = macos_a11y_checks(true);
            assert!(
                checks.iter().all(|c| c.status == CheckStatus::Ok),
                "{checks:?}"
            );
        }

        #[test]
        fn a11y_not_granted_fails_with_the_shared_remedy() {
            let checks = macos_a11y_checks(false);
            let c = checks.iter().find(|c| c.name == "Accessibility").unwrap();
            assert_eq!(c.status, CheckStatus::Fail);
            // Same remedy string as the "macos" section's own Accessibility check — no
            // separate, driftable copy here either.
            assert_eq!(
                c.remedy.as_deref(),
                Some(glass_macos::accessibility_remedy())
            );
        }

        #[test]
        fn a11y_not_granted_points_at_the_accessibility_pane() {
            let checks = macos_a11y_checks(false);
            let c = checks.iter().find(|c| c.name == "Accessibility").unwrap();
            // Same pane URL as the "macos" section's own Accessibility check — kept in
            // sync for the same reason as the shared remedy string above.
            assert_eq!(
                c.remedy_action.as_deref(),
                Some(format!("open {}", glass_macos::accessibility_pane_url()).as_str())
            );
        }

        #[test]
        fn companion_deep_check_maps_every_probe_outcome() {
            use glass_ios::doctor::CompanionProbe;

            let started = companion_deep_check(CompanionProbe::Started);
            assert_eq!(started.status, CheckStatus::Ok);
            assert_eq!(started.remedy, None);

            let unverified = companion_deep_check(CompanionProbe::SelfTestOk);
            assert_eq!(unverified.status, CheckStatus::Warn);

            let broken =
                companion_deep_check(CompanionProbe::FailedToStart("exited 1: boom".into()));
            assert_eq!(broken.status, CheckStatus::Fail);
            assert!(
                broken.detail.contains("boom"),
                "cause must surface: {}",
                broken.detail
            );
            assert_eq!(broken.remedy.as_deref(), Some(IDB_COMPANION_REMEDY));

            let unrunnable =
                companion_deep_check(CompanionProbe::SelfTestFailed("spawn: nope".into()));
            assert_eq!(unrunnable.status, CheckStatus::Fail);
            assert!(
                unrunnable.detail.contains("nope"),
                "cause must surface: {}",
                unrunnable.detail
            );

            let missing = companion_deep_check(CompanionProbe::NotFound);
            assert_eq!(missing.status, CheckStatus::Fail);
            assert_eq!(missing.remedy.as_deref(), Some(IDB_COMPANION_REMEDY));
        }
    }
}
