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

/// ANSI attributes for the human `doctor` and `env` renderings — a table of escape-sequence
/// prefixes plus one reset. [`Palette::PLAIN`] leaves every field empty, so rendering through it
/// emits no escapes at all and is byte-identical to the uncolored form. Deciding *whether* a
/// terminal wants color is the caller's job; this type only says what the escapes are.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Palette {
    pub ok: &'static str,
    pub warn: &'static str,
    pub fail: &'static str,
    pub dim: &'static str,
    pub bold: &'static str,
    pub heading: &'static str,
    pub reset: &'static str,
}

impl Palette {
    /// No color.
    pub const PLAIN: Palette = Palette {
        ok: "",
        warn: "",
        fail: "",
        dim: "",
        bold: "",
        heading: "",
        reset: "",
    };

    /// Basic SGR attributes, which every terminal that renders ANSI at all handles.
    pub const ANSI: Palette = Palette {
        ok: "\x1b[32m",
        warn: "\x1b[33m",
        fail: "\x1b[31m",
        dim: "\x1b[2m",
        bold: "\x1b[1m",
        heading: "\x1b[1;36m",
        reset: "\x1b[0m",
    };

    /// `text` wrapped in `attr` and this palette's reset. An empty `attr` returns `text`
    /// unchanged — with no reset appended, which is what keeps [`Palette::PLAIN`] byte-identical.
    pub fn paint(&self, attr: &str, text: &str) -> String {
        if attr.is_empty() {
            text.to_string()
        } else {
            format!("{attr}{text}{}", self.reset)
        }
    }

    /// The attribute a check of this status is drawn in. `Skip` shares `dim`: it reports
    /// something that did not run, so nothing in it should draw the eye.
    pub fn for_status(&self, status: CheckStatus) -> &'static str {
        match status {
            CheckStatus::Ok => self.ok,
            CheckStatus::Warn => self.warn,
            CheckStatus::Fail => self.fail,
            CheckStatus::Skip => self.dim,
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
    /// An actionable remedy: a command/URL a tool can run (e.g. open the exact
    /// Settings pane). Additive to `remedy` (human text); shown as a separate line.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub remedy_action: Option<String>,
}

impl Check {
    pub fn new(name: impl Into<String>, status: CheckStatus, detail: impl Into<String>) -> Self {
        Check {
            name: name.into(),
            status,
            detail: detail.into(),
            remedy: None,
            remedy_action: None,
        }
    }
    /// Attach a remedy (builder style).
    pub fn with_remedy(mut self, remedy: impl Into<String>) -> Self {
        self.remedy = Some(remedy.into());
        self
    }
    /// Attach an actionable remedy (builder style).
    pub fn with_remedy_action(mut self, action: impl Into<String>) -> Self {
        self.remedy_action = Some(action.into());
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
        Section {
            title: title.into(),
            backend,
            checks,
        }
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
        section
            .backend
            .as_deref()
            .is_none_or(|b| b == default_backend)
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
                s.checks
                    .iter()
                    .map(move |c| Self::effective(c.status, critical).rank())
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
        self.render_styled(default_backend, &Palette::PLAIN)
    }

    /// [`Diagnosis::render_text`] drawn with `palette`. Through [`Palette::PLAIN`] the two are
    /// byte-identical; a color palette only wraps spans of the same text in escapes.
    pub fn render_styled(&self, default_backend: &str, palette: &Palette) -> String {
        let mut out = format!("{}\n", palette.paint(palette.bold, "glass doctor"));
        let (mut ok, mut warn, mut fail) = (0u32, 0u32, 0u32);
        for s in &self.sections {
            let critical = Self::is_critical(s, default_backend);
            let note = match &s.backend {
                Some(b) if !critical => {
                    palette.paint(palette.dim, &format!("  (only needed for backend={b})"))
                }
                _ => String::new(),
            };
            out.push_str(&format!(
                "\n{}{note}\n",
                palette.paint(palette.heading, &format!("[{}]", s.title))
            ));
            for c in &s.checks {
                let eff = Self::effective(c.status, critical);
                match eff {
                    CheckStatus::Ok => ok += 1,
                    CheckStatus::Warn => warn += 1,
                    CheckStatus::Fail => fail += 1,
                    CheckStatus::Skip => {}
                }
                let attr = palette.for_status(eff);
                let glyph = eff.glyph().to_string();
                out.push_str(&match eff {
                    // Dim in full: a skipped check reports something that did not run.
                    CheckStatus::Skip => {
                        palette.paint(attr, &format!("  {glyph} {}: {}", c.name, c.detail))
                    }
                    // The name carries the color too, because the name is what you scan a
                    // failing report for; a lone glyph at a fixed column is easy to miss.
                    CheckStatus::Warn | CheckStatus::Fail => format!(
                        "  {} {}: {}",
                        palette.paint(attr, &glyph),
                        palette.paint(attr, &c.name),
                        c.detail
                    ),
                    // Glyph only, so a healthy report is not a wall of green.
                    CheckStatus::Ok => {
                        format!("  {} {}: {}", palette.paint(attr, &glyph), c.name, c.detail)
                    }
                });
                out.push('\n');
                if eff != CheckStatus::Ok {
                    if let Some(r) = &c.remedy {
                        out.push_str(&palette.paint(palette.dim, &format!("      → {r}")));
                        out.push('\n');
                    }
                    if let Some(a) = &c.remedy_action {
                        out.push_str(&palette.paint(palette.dim, &format!("      ↪ {a}")));
                        out.push('\n');
                    }
                }
            }
        }
        let overall = self.overall(default_backend);
        out.push_str(&format!(
            "\nSummary: {}, {}, {} — {}\n",
            summary_count(palette, ok, palette.ok, "ok"),
            summary_count(palette, warn, palette.warn, "warning(s)"),
            summary_count(palette, fail, palette.fail, "failure(s)"),
            palette.paint(
                &format!("{}{}", palette.bold, palette.for_status(overall)),
                match overall {
                    CheckStatus::Fail => "FAIL",
                    CheckStatus::Warn => "OK (with warnings)",
                    _ => "OK",
                }
            ),
        ));
        out
    }
}

/// One `N label` cell of the summary line, in `attr` when `n` is non-zero and dim when it is
/// zero — a red `0 failure(s)` reads as an alarm about a number that means "fine".
fn summary_count(palette: &Palette, n: u32, attr: &str, label: &str) -> String {
    let attr = if n == 0 { palette.dim } else { attr };
    palette.paint(attr, &format!("{n} {label}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn diag() -> Diagnosis {
        Diagnosis::new(vec![
            Section::new(
                "general",
                None,
                vec![Check::new("default backend", CheckStatus::Ok, "x11")],
            ),
            Section::new(
                "x11",
                Some("x11".into()),
                vec![Check::new("Xvfb", CheckStatus::Ok, "/usr/bin/Xvfb")],
            ),
            Section::new(
                "wayland",
                Some("wayland".into()),
                vec![
                    Check::new("sway >=1.12", CheckStatus::Fail, "not found")
                        .with_remedy("build it"),
                ],
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
        assert!(
            text.contains("[wayland]  (only needed for backend=wayland)"),
            "{text}"
        );
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
    fn the_default_backends_section_is_not_flagged_as_optional() {
        let text = diag().render_text("x11");
        assert!(text.contains("[x11]\n"), "{text}");
    }

    #[test]
    fn the_summary_counts_every_check_by_its_effective_status() {
        // wayland's Fail is a Warn here: x11 is the default backend.
        let text = diag().render_text("x11");
        assert!(
            text.contains("Summary: 2 ok, 1 warning(s), 0 failure(s)"),
            "{text}"
        );
    }

    #[test]
    fn the_summary_counts_a_failure_in_the_default_backend() {
        let text = diag().render_text("wayland");
        assert!(
            text.contains("Summary: 2 ok, 0 warning(s), 1 failure(s)"),
            "{text}"
        );
    }

    #[test]
    fn serializes_to_json_without_null_remedy() {
        let c = Check::new("Xvfb", CheckStatus::Ok, "found");
        let j = serde_json::to_string(&c).unwrap();
        assert_eq!(j, r#"{"name":"Xvfb","status":"ok","detail":"found"}"#);
    }

    #[test]
    fn render_shows_remedy_action_line() {
        let check = Check::new("Screen Recording", CheckStatus::Fail, "not granted")
            .with_remedy("enable it in System Settings")
            .with_remedy_action(
                "open: x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture",
            );
        let d = Diagnosis::new(vec![Section::new(
            "macos",
            Some("macos".into()),
            vec![check],
        )]);
        let out = d.render_text("macos");
        assert!(out.contains("→ enable it in System Settings"));
        assert!(out.contains(
            "↪ open: x-apple.systempreferences:com.apple.preference.security?Privacy_ScreenCapture"
        ));
    }

    /// Drop every `\x1b[…m` SGR sequence, leaving the text bytes behind. Test-only: the
    /// byte-identity assertions compare a colored render against a plain one through it.
    ///
    /// Deliberately duplicated in glass-mcp's `env` tests rather than shared — exporting a
    /// test-only helper from this crate's public API to serve one consumer is the worse trade.
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
    fn strip_ansi_removes_escapes_and_keeps_everything_else() {
        assert_eq!(strip_ansi("plain"), "plain");
        assert_eq!(strip_ansi("\x1b[32m✓\x1b[0m"), "✓");
        assert_eq!(strip_ansi("\x1b[1m\x1b[36m[x11]\x1b[0m"), "[x11]");
        assert_eq!(strip_ansi("done\x1b[0m"), "done");
    }

    #[test]
    fn strip_ansi_is_not_the_identity_function() {
        // The byte-identity tests compare `strip_ansi(colored)` against `plain`. A helper that
        // returned its input unchanged would make them pass against an implementation that
        // emitted no color at all, so pin that it actually removes something.
        let colored = Palette::ANSI.paint(Palette::ANSI.ok, "x");
        assert!(colored.len() > "x".len(), "expected escapes: {colored:?}");
        assert_eq!(strip_ansi(&colored), "x");
    }

    #[test]
    fn a_plain_paint_appends_nothing_at_all() {
        // Not merely "looks the same": an implementation that always appended `reset` would
        // render identically to the eye while adding an escape to every painted span, and every
        // byte-identity assertion downstream rests on this.
        let p = Palette::PLAIN;
        assert_eq!(p.paint(p.bold, "text"), "text");
        assert_eq!(p.paint(p.ok, "✓"), "✓");
        // The PLAIN assertions above cannot discriminate on their own: PLAIN's `reset` is empty
        // too, so appending it appends nothing. Only an empty attribute against a palette with a
        // real reset separates "skip the reset" from "append it unconditionally".
        assert_eq!(Palette::ANSI.paint("", "text"), "text");
    }

    #[test]
    fn an_ansi_paint_wraps_the_text_in_the_attribute_and_a_reset() {
        let p = Palette::ANSI;
        assert_eq!(p.paint(p.ok, "✓"), "\x1b[32m✓\x1b[0m");
    }

    #[test]
    fn each_status_draws_in_its_own_attribute() {
        // Compared pairwise rather than checked non-empty, so swapping two match arms fails.
        let p = Palette::ANSI;
        assert_eq!(p.for_status(CheckStatus::Ok), p.ok);
        assert_eq!(p.for_status(CheckStatus::Warn), p.warn);
        assert_eq!(p.for_status(CheckStatus::Fail), p.fail);
        assert_eq!(p.for_status(CheckStatus::Skip), p.dim);
        // and the attributes are distinct, so those comparisons can discriminate
        assert_ne!(p.ok, p.warn);
        assert_ne!(p.warn, p.fail);
        assert_ne!(p.fail, p.dim);
    }

    #[test]
    fn color_adds_escapes_and_changes_nothing_else() {
        // The load-bearing property. Strip the escapes from a colored render and it must equal
        // the plain one byte for byte — this fails if color adds, drops, or reorders a single
        // text byte. Both backends, so the non-default-section path is covered too.
        let d = diag();
        for backend in ["x11", "wayland"] {
            assert_eq!(
                strip_ansi(&d.render_styled(backend, &Palette::ANSI)),
                d.render_text(backend),
                "backend {backend}"
            );
        }
    }

    #[test]
    fn a_skipped_check_is_dim_across_its_whole_line() {
        let p = Palette::ANSI;
        let d = Diagnosis::new(vec![Section::new(
            "android",
            None,
            vec![Check::new("agent", CheckStatus::Skip, "not configured")],
        )]);
        let out = d.render_styled("x11", &p);
        assert!(
            out.contains(&format!("{}  – agent: not configured{}", p.dim, p.reset)),
            "{out:?}"
        );
    }

    #[test]
    fn a_warning_colors_its_name_but_a_passing_check_does_not() {
        let p = Palette::ANSI;
        let d = Diagnosis::new(vec![Section::new(
            "general",
            None,
            vec![
                Check::new("device", CheckStatus::Warn, "none online"),
                Check::new("glass", CheckStatus::Ok, "1.2.0"),
            ],
        )]);
        let out = d.render_styled("x11", &p);
        // Warn: the name carries the color too — it is what you scan a failing report for.
        assert!(
            out.contains(&format!("{}device{}", p.warn, p.reset)),
            "{out:?}"
        );
        // Ok: glyph only, so a healthy report is not a wall of green.
        assert!(
            out.contains(&format!("{}✓{} glass: 1.2.0", p.ok, p.reset)),
            "{out:?}"
        );
        assert!(
            !out.contains(&format!("{}glass{}", p.ok, p.reset)),
            "{out:?}"
        );
    }

    #[test]
    fn a_zero_count_is_dim_and_a_non_zero_one_takes_its_status_color() {
        // diag() under x11 is 2 ok, 1 warning, 0 failures.
        let p = Palette::ANSI;
        let out = diag().render_styled("x11", &p);
        assert!(out.contains(&format!("{}2 ok{}", p.ok, p.reset)), "{out:?}");
        assert!(
            out.contains(&format!("{}1 warning(s){}", p.warn, p.reset)),
            "{out:?}"
        );
        // Red on a zero would read as an alarm about a number that means "fine".
        assert!(
            out.contains(&format!("{}0 failure(s){}", p.dim, p.reset)),
            "{out:?}"
        );
        assert!(
            !out.contains(&format!("{}0 failure(s){}", p.fail, p.reset)),
            "{out:?}"
        );
    }

    #[test]
    fn the_verdict_is_bold_in_the_overall_status_color() {
        let p = Palette::ANSI;
        let out = diag().render_styled("wayland", &p);
        assert!(
            out.contains(&format!("{}{}FAIL{}", p.bold, p.fail, p.reset)),
            "{out:?}"
        );
    }

    #[test]
    fn every_span_of_a_colored_report_is_painted_as_written_here() {
        // The escape-stripping byte-identity test above cannot see a *missing* escape: deleting
        // one leaves the stripped text unchanged. So one golden, over a fixture small enough to
        // read, pinning each structural span literally — the title, the section heading, the
        // glyph and name of a Warn and a Fail, both remedy lines, every summary cell, and the
        // verdict. Written as escape literals on purpose: building the expectation from
        // `palette.*` would make it pass against any palette, which is the hole it closes.
        let d = Diagnosis::new(vec![Section::new(
            "general",
            None,
            vec![
                Check::new("device", CheckStatus::Warn, "none online"),
                Check::new("sway", CheckStatus::Fail, "not found")
                    .with_remedy("build it")
                    .with_remedy_action("open: https://example.invalid/sway"),
            ],
        )]);
        assert_eq!(
            d.render_styled("x11", &Palette::ANSI),
            concat!(
                "\x1b[1mglass doctor\x1b[0m\n",
                "\n\x1b[1;36m[general]\x1b[0m\n",
                "  \x1b[33m⚠\x1b[0m \x1b[33mdevice\x1b[0m: none online\n",
                "  \x1b[31m✗\x1b[0m \x1b[31msway\x1b[0m: not found\n",
                "\x1b[2m      → build it\x1b[0m\n",
                "\x1b[2m      ↪ open: https://example.invalid/sway\x1b[0m\n",
                "\nSummary: \x1b[2m0 ok\x1b[0m, \x1b[33m1 warning(s)\x1b[0m, ",
                "\x1b[31m1 failure(s)\x1b[0m — \x1b[1m\x1b[31mFAIL\x1b[0m\n",
            )
        );
    }

    #[test]
    fn the_non_default_backend_note_is_dim() {
        // The one structural span the golden above cannot carry: it needs a section for a
        // backend other than the default.
        let d = Diagnosis::new(vec![Section::new(
            "wayland",
            Some("wayland".into()),
            vec![Check::new("sway >=1.12", CheckStatus::Ok, "found")],
        )]);
        let out = d.render_styled("x11", &Palette::ANSI);
        assert!(
            out.contains(
                "\x1b[1;36m[wayland]\x1b[0m\x1b[2m  (only needed for backend=wayland)\x1b[0m\n"
            ),
            "{out:?}"
        );
    }

    #[test]
    fn the_mcp_and_json_renderings_carry_no_escapes() {
        // `render_text` is what server.rs hands the glass_doctor MCP tool, and the same Diagnosis
        // is what --json serializes. Either carrying an escape would put terminal control bytes
        // into an agent's JSON.
        let d = diag();
        assert!(!d.render_text("x11").contains('\x1b'));
        assert!(!serde_json::to_string(&d).unwrap().contains('\x1b'));
    }
}
