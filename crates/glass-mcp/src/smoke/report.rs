//! The smoke run's result: per-check outcomes, and how they render.

use glass_core::CheckStatus as DoctorStatus;
use serde::Serialize;

/// Outcome of a single check. `XFail` is a known limitation that failed as expected;
/// `XPass` is a known limitation that has started passing — reported so the support
/// matrix does not quietly rot, but never a failure.
///
/// One report carries both renderings — the text report and the JSON — so [`Display`]
/// and the serde representation must spell each status identically, or the docs telling a
/// reader to grep the JSON for what they saw on screen would be wrong. The text report
/// prints this spelling on every row, which is also what keeps the five statuses apart
/// where two of them share a glyph — see [`CheckStatus::severity`].
/// `display_matches_the_serialized_spelling` holds them together.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "lowercase")]
pub enum CheckStatus {
    Pass,
    Fail,
    XFail,
    XPass,
    Skip,
}

impl std::fmt::Display for CheckStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // `pad`, not `write_str`: the text report asks for `{:<5}` so the four- and
        // five-letter statuses put the step number in the same column, and `write_str`
        // silently drops the formatter's width.
        f.pad(match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Fail => "fail",
            CheckStatus::XFail => "xfail",
            CheckStatus::XPass => "xpass",
            CheckStatus::Skip => "skip",
        })
    }
}

impl CheckStatus {
    /// This status as one of `doctor`'s, which is what supplies the glyph and the summary
    /// bucket. Mapping onto that vocabulary rather than inventing a second one is what keeps
    /// the two reports readable side by side; deriving both the glyph and the count from it
    /// is what stops a row's glyph and the summary line disagreeing.
    ///
    /// `XFail` and `XPass` both land on `Warn`: neither fails a run, and both are the same
    /// kind of thing to look at — a recorded limitation. They are told apart by the status
    /// word printed beside the glyph, not by the glyph.
    fn severity(self) -> DoctorStatus {
        match self {
            CheckStatus::Pass => DoctorStatus::Ok,
            CheckStatus::Fail => DoctorStatus::Fail,
            CheckStatus::XFail | CheckStatus::XPass => DoctorStatus::Warn,
            CheckStatus::Skip => DoctorStatus::Skip,
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    pub step: u8,
    pub name: String,
    pub status: CheckStatus,
    /// What happened, in one line. On a failure this must say enough to triage
    /// without re-running.
    pub detail: String,
}

impl CheckOutcome {
    pub fn pass(step: u8, name: &str, detail: impl Into<String>) -> Self {
        Self::new(step, name, CheckStatus::Pass, detail)
    }

    pub fn fail(step: u8, name: &str, detail: impl Into<String>) -> Self {
        Self::new(step, name, CheckStatus::Fail, detail)
    }

    pub fn skip(step: u8, name: &str, detail: impl Into<String>) -> Self {
        Self::new(step, name, CheckStatus::Skip, detail)
    }

    fn new(step: u8, name: &str, status: CheckStatus, detail: impl Into<String>) -> Self {
        Self {
            step,
            name: name.into(),
            status,
            detail: detail.into(),
        }
    }
}

/// Whether the run drove anything. A `--dry-run` exercises nothing — every check is a `skip`,
/// so the run *cannot* fail — and a bare `PASS` over that would read as "this build works" to
/// the three channels (heading, exit code, statuses) a reader or script actually consults.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RunMode {
    /// The checks ran against a spawned server and a launched app.
    Full,
    /// `--dry-run`: the plan only. Nothing was spawned, launched or called.
    DryRun,
}

/// The app the run drove, or why it had none. Modelled as one value with two states rather
/// than a `String` carrying either, so a JSON consumer reads a discriminant instead of
/// sniffing a label for substrings that would tell it a remedy sentence from an app name.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
#[serde(tag = "state", content = "value", rename_all = "snake_case")]
pub enum TargetApp {
    /// The candidate selected, by label. Reports from different hosts are only comparable
    /// if this is visible.
    Selected(String),
    /// No app was available; the value says why and what to do. Only a `--dry-run` report can
    /// carry this — a real run needs an app and stops before it has a report to put it in.
    Unavailable(String),
}

impl TargetApp {
    /// The remedy note, when there is no app. The place to *read* it is the `start` check's
    /// `detail`, where the docs already send a reader; this is where that row gets it from.
    pub fn note(&self) -> Option<&str> {
        match self {
            Self::Selected(_) => None,
            Self::Unavailable(note) => Some(note),
        }
    }
}

/// The one-word label for the report's header line. Deliberately short in both states: the
/// header is a subtitle slot, and a sentence in it is what made an unusable host read as
/// healthy. The sentence lives in the `start` row instead.
impl std::fmt::Display for TargetApp {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Selected(label) => f.write_str(label),
            Self::Unavailable(_) => f.write_str("none available"),
        }
    }
}

#[derive(Debug, Clone, Serialize)]
pub struct SmokeReport {
    pub backend: String,
    /// The version the server reported at `initialize`, or `None` when it reported none.
    /// Nothing asserts on it — it is here so a report attached to an issue says which build
    /// produced it — so a server that omits it is recorded as `null` rather than ending the
    /// run. `null` is a discriminant a JSON consumer reads directly, unlike a placeholder
    /// string it would have to recognise.
    pub version: Option<String>,
    pub mode: RunMode,
    pub app: TargetApp,
    pub checks: Vec<CheckOutcome>,
}

impl SmokeReport {
    /// How the header names the server's version.
    fn version_label(&self) -> &str {
        self.version.as_deref().unwrap_or("(version not reported)")
    }

    /// Only a hard `Fail` fails a run. A known limitation that failed (`XFail`) and one
    /// that started passing (`XPass`) are both reportable, neither is a failure.
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }

    pub fn exit_code(&self) -> i32 {
        i32::from(self.failed())
    }

    /// The heading's verdict. `PASS` alone claims the build was exercised and held up; a
    /// `--dry-run` exercised nothing, so it says which kind of pass it is. Both still exit 0 —
    /// nothing failed.
    fn verdict(&self) -> &'static str {
        match (self.failed(), self.mode) {
            (true, _) => "FAIL",
            (false, RunMode::Full) => "PASS",
            (false, RunMode::DryRun) => "PASS (plan only)",
        }
    }

    /// The human report, in the shape `glass-mcp doctor` uses
    /// ([`glass_core::Diagnosis::render_text`]): a heading, one glyph-prefixed line per check,
    /// and a trailing `Summary:` line ending in the verdict. `--report` writes the JSON;
    /// this is the channel a person reads.
    pub fn to_text(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let verdict = self.verdict();
        // Every interpolated value goes through `one_line`, not only the ones that need it
        // today. `version` is the one that can actually carry a newline — it is `git describe`
        // output on a real run — and `detail` carries arbitrary text. `backend` is a canonical
        // literal and `app`'s `Display` yields a label or a fixed string, so neither needs it
        // now; both go through it anyway, because the reason the app field fell outside this
        // hardening once was that it started safe and *became* error-derived later.
        let _ = writeln!(
            out,
            "glass smoke — {} — {verdict}\nglass-mcp {} · app: {}\n",
            one_line(&self.backend),
            one_line(self.version_label()),
            one_line(&self.app.to_string())
        );
        let (mut ok, mut warn, mut fail, mut skipped) = (0u32, 0u32, 0u32, 0u32);
        for c in &self.checks {
            let severity = c.status.severity();
            match severity {
                DoctorStatus::Ok => ok += 1,
                DoctorStatus::Warn => warn += 1,
                DoctorStatus::Fail => fail += 1,
                DoctorStatus::Skip => skipped += 1,
            }
            // The status word is printed as well as the glyph: `xfail` and `xpass` share
            // `⚠`, and it is also the spelling the JSON uses, so what a reader sees here is
            // what they can grep the report file for.
            let _ = writeln!(
                out,
                "  {} {:<5} {} {}: {}",
                severity.glyph(),
                c.status,
                c.step,
                one_line(&c.name),
                one_line(&c.detail)
            );
            if c.status == CheckStatus::XPass {
                // Doctor's continuation-line convention, carrying the "reported" an `xpass`
                // promises: a limitation that has quietly been fixed must not rot the support
                // matrix.
                let _ = writeln!(
                    out,
                    "      → stale limitation: recorded as a known limitation but passed — \
                     re-check the support matrix"
                );
            }
        }
        let _ = writeln!(
            out,
            "\nSummary: {ok} ok, {warn} warning(s), {fail} failure(s), {skipped} skipped — \
             {verdict}"
        );
        out
    }
}

/// Collapse one value onto a single line so it cannot break the report's layout. Every line
/// of the report means something positionally — a check row, a continuation, the summary — so
/// a value spilling onto its own line would read as one of those, and a detail long on
/// newlines could push the summary off a reader's screen entirely. Arbitrary text reaches
/// `detail`: `check_error_honesty` formats raw tool error text into it, and
/// `GlassError::AccessibilityUnavailable` forwards backend stdout verbatim, so a newline
/// cannot be assumed absent. Nothing else needs escaping — the format has no delimiter a
/// value could impersonate.
fn one_line(s: &str) -> String {
    s.replace("\r\n", "\n").replace(['\n', '\r'], " ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(checks: Vec<CheckOutcome>) -> SmokeReport {
        SmokeReport {
            backend: "x11".into(),
            version: Some("1.1.0".into()),
            mode: RunMode::Full,
            app: TargetApp::Selected("xterm".into()),
            checks,
        }
    }

    #[test]
    fn a_failed_check_fails_the_report() {
        let r = report(vec![
            CheckOutcome::pass(1, "start", "800x600"),
            CheckOutcome::fail(2, "capabilities+doctor", "doctor FAIL: display"),
        ]);
        assert!(r.failed());
        assert_eq!(r.exit_code(), 1);
    }

    #[test]
    fn xfail_and_xpass_never_fail_the_report() {
        let mut xfail = CheckOutcome::fail(9, "gesture", "single contact only");
        xfail.status = CheckStatus::XFail;
        let mut xpass = CheckOutcome::pass(9, "gesture", "multi-contact worked");
        xpass.status = CheckStatus::XPass;
        let r = report(vec![xfail, xpass]);
        assert!(!r.failed());
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn display_matches_the_serialized_spelling() {
        // One report renders both; the docs tell readers to grep the JSON for the status they
        // saw on screen, which only works while these two agree.
        for status in [
            CheckStatus::Pass,
            CheckStatus::Fail,
            CheckStatus::XFail,
            CheckStatus::XPass,
            CheckStatus::Skip,
        ] {
            let json = serde_json::to_string(&status).expect("a status serializes");
            assert_eq!(
                format!("\"{status}\""),
                json,
                "the text report and JSON must spell {status:?} the same way"
            );
        }
    }

    /// Two statuses share `⚠`, so the glyph alone cannot carry all five. Every row therefore
    /// prints the status word too, and it is the pair that must be unique — otherwise `xfail`
    /// and `xpass`, which mean opposite things about a limitation, would read identically.
    #[test]
    fn every_status_renders_a_distinguishable_row() {
        let mut rendered = std::collections::HashSet::new();
        for status in [
            CheckStatus::Pass,
            CheckStatus::Fail,
            CheckStatus::XFail,
            CheckStatus::XPass,
            CheckStatus::Skip,
        ] {
            let mut c = CheckOutcome::pass(1, "start", "detail");
            c.status = status;
            let text = report(vec![c]).to_text();
            let row = text
                .lines()
                .find(|l| l.contains("start"))
                .unwrap_or_default()
                .to_string();
            assert!(
                row.contains(status.severity().glyph()) && row.contains(&status.to_string()),
                "a row must carry both the glyph and the status word: {row}"
            );
            assert!(
                rendered.insert(row.clone()),
                "{status} renders as {row:?}, which another status already produces"
            );
        }
    }

    /// `xfail`/`xpass` are a letter longer than `pass`/`fail`/`skip`, so without padding the
    /// step number — the thing a reader scans for — would sit in two different columns.
    #[test]
    fn every_row_puts_the_step_number_in_the_same_column() {
        let columns: std::collections::HashSet<usize> = [
            CheckStatus::Pass,
            CheckStatus::Fail,
            CheckStatus::XFail,
            CheckStatus::XPass,
            CheckStatus::Skip,
        ]
        .into_iter()
        .map(|status| {
            let mut c = CheckOutcome::pass(1, "start", "detail");
            c.status = status;
            let text = report(vec![c]).to_text();
            let row = text
                .lines()
                .find(|l| l.contains("start"))
                .expect("a row")
                .to_string();
            // Counted in characters, not bytes: the glyphs are multi-byte, so a byte offset
            // would only agree with the visual column by coincidence.
            let byte = row.find("1 start").expect("the step number");
            row[..byte].chars().count()
        })
        .collect();
        assert_eq!(
            columns.len(),
            1,
            "the step number moved column: {columns:?}"
        );
    }

    /// The glyphs are doctor's, so a reader moving between the two reports reads the same
    /// vocabulary. This pins the mapping itself — the summary buckets are derived from it, so
    /// a change here moves both together.
    #[test]
    fn the_glyphs_are_doctors() {
        assert_eq!(CheckStatus::Pass.severity(), DoctorStatus::Ok);
        assert_eq!(CheckStatus::Fail.severity(), DoctorStatus::Fail);
        assert_eq!(CheckStatus::XFail.severity(), DoctorStatus::Warn);
        assert_eq!(CheckStatus::XPass.severity(), DoctorStatus::Warn);
        assert_eq!(CheckStatus::Skip.severity(), DoctorStatus::Skip);
    }

    /// Doctor's trailing line, adapted: smoke counts skips too, because a run of nothing but
    /// skips is the case the docs most warn about reading as a pass.
    #[test]
    fn the_summary_line_counts_every_status_and_ends_in_the_verdict() {
        let mut xfail = CheckOutcome::fail(5, "interaction", "known");
        xfail.status = CheckStatus::XFail;
        let text = report(vec![
            CheckOutcome::pass(1, "start", "800x600"),
            CheckOutcome::fail(2, "capabilities+doctor", "doctor FAIL: display"),
            xfail,
            CheckOutcome::skip(3, "screenshot", "not reached"),
        ])
        .to_text();
        let summary = text
            .lines()
            .find(|l| l.starts_with("Summary:"))
            .expect("a summary line");
        assert_eq!(
            summary,
            "Summary: 1 ok, 1 warning(s), 1 failure(s), 1 skipped — FAIL"
        );
    }

    /// The step number is how the docs and the end-to-end gate refer to a check, so it has to
    /// survive on the row a reader actually sees.
    #[test]
    fn a_row_carries_its_step_number() {
        let text = report(vec![CheckOutcome::pass(7, "error honesty", "actionable")]).to_text();
        assert!(
            text.lines()
                .any(|l| l.contains("7 error honesty: actionable")),
            "got: {text}"
        );
    }

    /// A `|` used to be escaped, because it would have shifted a markdown table's columns.
    /// The text report has no such delimiter, so it must now survive verbatim — and still on
    /// one line.
    #[test]
    fn a_pipe_in_a_detail_is_rendered_verbatim_on_one_line() {
        let text = report(vec![CheckOutcome::fail(
            1,
            "start",
            "backend said: a | b | c",
        )])
        .to_text();
        assert!(
            text.contains("1 start: backend said: a | b | c"),
            "a pipe needs no escaping here: {text}"
        );
        assert!(!text.contains("\\|"), "nothing should escape it: {text}");
    }

    #[test]
    fn a_newline_in_a_detail_does_not_add_a_line_or_hide_a_later_check() {
        // The dangerous one: a detail that spills onto its own line reads as a row of its own
        // and pushes everything after it — the failing check, the summary — further away.
        let rows = |detail: &str| {
            vec![
                CheckOutcome::fail(1, "start", detail),
                CheckOutcome::fail(8, "stop", "the row that must not vanish"),
            ]
        };
        // Compared against a baseline whose detail carries the same text with no newline in
        // it, the way the header's sibling test does. Counting lines that merely *look* like
        // rows would not fail if the collapsing were removed: a spilled fragment starts at
        // column 0, so an indent-prefix filter would not count it and the total would still
        // come out right.
        let clean = report(rows("line one line two")).to_text();
        let text = report(rows("line one\nline two")).to_text();
        assert_eq!(
            text.lines().count(),
            clean.lines().count(),
            "the newline added a line to the report: {text}"
        );
        assert!(
            text.contains("1 start: line one line two"),
            "the newline must be collapsed in place: {text}"
        );
        assert!(
            text.contains("the row that must not vanish"),
            "a later row must survive an earlier row's newline: {text}"
        );
    }

    /// The failure this pins: a plan-only run on a machine that cannot run the checks at all
    /// used to head its report `PASS`, agreeing with exit 0 and uniform `skip` rows.
    #[test]
    fn a_dry_run_heading_says_it_exercised_nothing() {
        let mut r = report(vec![CheckOutcome::skip(1, "start", "dry run")]);
        r.mode = RunMode::DryRun;
        let text = r.to_text();
        assert!(
            text.lines()
                .next()
                .is_some_and(|h| h.contains("PASS (plan only)")),
            "a dry run must not head its report with a bare PASS: {text}"
        );
        assert_eq!(r.exit_code(), 0, "nothing failed, so the exit code stays 0");
    }

    /// The heading carries what a report has to be read against: the backend, the build, and
    /// the app that was driven.
    #[test]
    fn the_heading_names_the_backend_the_version_and_the_app() {
        let text = report(vec![]).to_text();
        let heading: Vec<&str> = text.lines().take(2).collect();
        assert!(heading[0].contains("x11"), "got: {heading:?}");
        assert!(heading[1].contains("glass-mcp 1.1.0"), "got: {heading:?}");
        assert!(heading[1].contains("app: xterm"), "got: {heading:?}");
    }

    /// A server that answered `initialize` without a version no longer ends the run, so the
    /// heading has to say the version is missing rather than leave an empty slot that reads
    /// like a rendering bug.
    #[test]
    fn an_unreported_version_says_so_in_the_heading() {
        let mut r = report(vec![]);
        r.version = None;
        assert!(
            r.to_text().contains("glass-mcp (version not reported)"),
            "got: {}",
            r.to_text()
        );
    }

    /// A dry run that *did* find nothing still reports the remedy — in the `start` row, not
    /// the header, whose slot takes a label.
    #[test]
    fn an_unavailable_app_renders_as_a_label_not_a_sentence() {
        let mut r = report(vec![]);
        r.app = TargetApp::Unavailable("would fail: no target app on PATH — install one".into());
        let header = r.to_text();
        assert!(header.contains("app: none available"), "got: {header}");
        assert!(
            !header.contains("install one"),
            "the remedy belongs in the start row, not the header: {header}"
        );
    }

    /// The header must survive the same text the `detail`s are hardened against. `version` is
    /// the value that can genuinely carry a newline — it is `git describe` output on a real
    /// run — and `app` is the one that became error-derived after the details were hardened.
    #[test]
    fn a_newline_in_a_header_value_cannot_break_the_report() {
        const HOSTILE: &str = "we\nird | value";
        let row = || vec![CheckOutcome::pass(1, "start", "800x600")];
        // What the report looks like with nothing hostile in it. Deriving the expected line
        // count beats writing one down: it stays right as the header gains or loses lines.
        let clean = report(row()).to_text().lines().count();

        let mut with_app = report(row());
        with_app.app = TargetApp::Selected(HOSTILE.into());
        let mut with_version = report(row());
        with_version.version = Some(HOSTILE.into());
        let mut with_backend = report(row());
        with_backend.backend = HOSTILE.into();

        for r in [with_app, with_version, with_backend] {
            let text = r.to_text();
            assert_eq!(
                text.lines().count(),
                clean,
                "a newline added a line to the report: {text}"
            );
            assert!(
                text.contains("we ird | value"),
                "the value must be collapsed in place, not dropped: {text}"
            );
        }
    }

    /// A consumer must be able to tell a selected app from a remedy without reading the text.
    #[test]
    fn the_json_app_field_carries_a_discriminant() {
        let selected = serde_json::to_value(TargetApp::Selected("zenity".into())).unwrap();
        assert_eq!(selected["state"], "selected");
        assert_eq!(selected["value"], "zenity");

        let none =
            serde_json::to_value(TargetApp::Unavailable("install one of: …".into())).unwrap();
        assert_eq!(none["state"], "unavailable");
    }

    #[test]
    fn the_text_report_names_the_app_and_every_check_and_flags_stale_limits() {
        let mut xpass = CheckOutcome::pass(5, "interaction", "multi-contact worked");
        xpass.status = CheckStatus::XPass;
        let text = report(vec![CheckOutcome::pass(1, "start", "800x600"), xpass]).to_text();
        assert!(text.contains("xterm"), "selected app must appear: {text}");
        assert!(text.contains("start"), "every check must appear: {text}");
        assert!(
            text.contains("interaction"),
            "every check must appear: {text}"
        );
        assert!(
            text.to_lowercase().contains("stale limitation"),
            "an XPass must be called out as a stale limitation: {text}"
        );
    }

    /// The callout belongs to the row that earned it: a reader scanning for `⚠` must find the
    /// reason next to it, not in a footer they may never reach.
    #[test]
    fn only_an_xpass_gets_the_stale_limitation_callout() {
        let mut xfail = CheckOutcome::fail(5, "interaction", "single contact only");
        xfail.status = CheckStatus::XFail;
        let text = report(vec![xfail, CheckOutcome::pass(1, "start", "800x600")]).to_text();
        assert!(
            !text.to_lowercase().contains("stale limitation"),
            "an XFail is the limitation working as recorded, not a stale one: {text}"
        );
    }
}
