//! Environment diagnostics — the platform-agnostic vocabulary for "glass doctor".
//!
//! `glass-core` defines only the reporting types and the pure rendering/severity
//! logic. The backends contribute their own [`Check`]s (whether Xvfb/sway/etc. are
//! present); `glass-mcp` aggregates them into a [`Diagnosis`] and drives the CLI
//! subcommand and the `glass_doctor` MCP tool.

use serde::Serialize;

/// Outcome of a single check.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    /// Ready.
    Ok,
    /// Works, but something is sub-optimal or only matters for a non-default path.
    Warn,
    /// Will not work; carries a remedy.
    Fail,
    /// Not applicable in this build/host (e.g. a backend that isn't compiled in).
    Skip,
}

impl CheckStatus {
    /// Glyph for human output.
    pub fn glyph(self) -> char {
        match self {
            CheckStatus::Ok => '✓',
            CheckStatus::Warn => '⚠',
            CheckStatus::Fail => '✗',
            CheckStatus::Skip => '–',
        }
    }
    fn rank(self) -> u8 {
        match self {
            CheckStatus::Fail => 2,
            CheckStatus::Warn => 1,
            CheckStatus::Ok | CheckStatus::Skip => 0,
        }
    }
}

/// One diagnostic check.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Check {
    pub name: String,
    pub status: CheckStatus,
    pub detail: String,
    /// How to fix it, when failing or warning.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy: Option<String>,
}

impl Check {
    pub fn new(name: impl Into<String>, status: CheckStatus, detail: impl Into<String>) -> Self {
        Check { name: name.into(), status, detail: detail.into(), remedy: None }
    }
    /// Attach a remedy (builder style).
    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
}

/// A group of checks. `backend` names the backend it diagnoses (`"x11"`/`"wayland"`)
/// or `None` for general checks; this drives default-backend severity (below).
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Section {
    pub title: String,
    pub backend: Option<String>,
    pub checks: Vec<Check>,
}

impl Section {
    pub fn new(title: impl Into<String>, backend: Option<String>, checks: Vec<Check>) -> Self {
        Section { title: title.into(), backend, checks }
    }
}

/// A full environment report.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub struct Diagnosis {
    pub sections: Vec<Section>,
}

impl Diagnosis {
    pub fn new(sections: Vec<Section>) -> Self {
        Diagnosis { sections }
    }

    /// Is this section critical to the run? General checks (no backend) and the
    /// default backend's checks are; another backend's are not.
    fn is_critical(section: &Section, default_backend: &str) -> bool {
        section.backend.as_deref().is_none_or(|b| b == default_backend)
    }

    /// A check's *effective* status: a `Fail` in a non-default backend is only a
    /// `Warn` for the run as a whole (you're not using that backend).
    fn effective(status: CheckStatus, critical: bool) -> CheckStatus {
        match status {
            CheckStatus::Fail if !critical => CheckStatus::Warn,
            other => other,
        }
    }

    /// Worst effective status across all checks, given the active default backend.
    pub fn overall(&self, default_backend: &str) -> CheckStatus {
        let worst = self
            .sections
            .iter()
            .flat_map(|s| {
                let critical = Self::is_critical(s, default_backend);
                s.checks.iter().map(move |c| Self::effective(c.status, critical).rank())
            })
            .max()
            .unwrap_or(0);
        match worst {
            2 => CheckStatus::Fail,
            1 => CheckStatus::Warn,
            _ => CheckStatus::Ok,
        }
    }

    /// Process exit code: non-zero only when the default backend can't run.
    pub fn exit_code(&self, default_backend: &str) -> i32 {
        i32::from(self.overall(default_backend) == CheckStatus::Fail)
    }

    /// Human-readable report. Non-default backend sections are flagged, and their
    /// failures shown as warnings, to match [`Diagnosis::overall`].
    pub fn render_text(&self, default_backend: &str) -> String {
        let mut out = String::from("glass doctor\n");
        let (mut ok, mut warn, mut fail) = (0u32, 0u32, 0u32);
        for s in &self.sections {
            let critical = Self::is_critical(s, default_backend);
            let note = match &s.backend {
                Some(b) if !critical => format!("  (only needed for backend={b})"),
                _ => String::new(),
            };
            out.push_str(&format!("\n[{}]{note}\n", s.title));
            for c in &s.checks {
                let eff = Self::effective(c.status, critical);
                match eff {
                    CheckStatus::Ok => ok += 1,
                    CheckStatus::Warn => warn += 1,
                    CheckStatus::Fail => fail += 1,
                    CheckStatus::Skip => {}
                }
                out.push_str(&format!("  {} {}: {}\n", eff.glyph(), c.name, c.detail));
                if let Some(r) = &c.remedy {
                    if eff != CheckStatus::Ok {
                        out.push_str(&format!("      → {r}\n"));
                    }
                }
            }
        }
        let overall = self.overall(default_backend);
        out.push_str(&format!(
            "\nSummary: {ok} ok, {warn} warning(s), {fail} failure(s) — {}\n",
            match overall {
                CheckStatus::Fail => "FAIL",
                CheckStatus::Warn => "OK (with warnings)",
                _ => "OK",
            }
        ));
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag() -> Diagnosis {
        Diagnosis::new(vec![
            Section::new("general", None, vec![Check::new("default backend", CheckStatus::Ok, "x11")]),
            Section::new(
                "x11",
                Some("x11".into()),
                vec![Check::new("Xvfb", CheckStatus::Ok, "/usr/bin/Xvfb")],
            ),
            Section::new(
                "wayland",
                Some("wayland".into()),
                vec![Check::new("sway >=1.12", CheckStatus::Fail, "not found")
                    .with_remedy("build it")],
            ),
        ])
    }

    #[test]
    fn nondefault_backend_failure_is_only_a_warning() {
        let d = diag();
        // x11 is default; wayland's Fail must not fail the run.
        assert_eq!(d.overall("x11"), CheckStatus::Warn);
        assert_eq!(d.exit_code("x11"), 0);
    }

    #[test]
    fn default_backend_failure_fails_the_run() {
        let d = diag();
        // now wayland is default, so its Fail is critical.
        assert_eq!(d.overall("wayland"), CheckStatus::Fail);
        assert_eq!(d.exit_code("wayland"), 1);
    }

    #[test]
    fn all_ok_is_ok() {
        let d = Diagnosis::new(vec![Section::new(
            "x11",
            Some("x11".into()),
            vec![Check::new("Xvfb", CheckStatus::Ok, "found")],
        )]);
        assert_eq!(d.overall("x11"), CheckStatus::Ok);
        assert_eq!(d.exit_code("x11"), 0);
    }

    #[test]
    fn render_flags_nondefault_section_and_downgrades_its_failure() {
        let text = diag().render_text("x11");
        assert!(text.contains("[wayland]  (only needed for backend=wayland)"), "{text}");
        // wayland's Fail renders as a warning glyph, and its remedy is shown.
        assert!(text.contains("⚠ sway >=1.12: not found"), "{text}");
        assert!(text.contains("→ build it"), "{text}");
        assert!(text.contains("— OK (with warnings)"), "{text}");
    }

    #[test]
    fn render_shows_failure_for_default_backend() {
        let text = diag().render_text("wayland");
        assert!(text.contains("✗ sway >=1.12: not found"), "{text}");
        assert!(text.trim().ends_with("— FAIL"), "{text}");
    }

    #[test]
    fn serializes_to_json_without_null_remedy() {
        let c = Check::new("Xvfb", CheckStatus::Ok, "found");
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"name":"Xvfb","status":"ok","detail":"found"}"#);
    }
}
