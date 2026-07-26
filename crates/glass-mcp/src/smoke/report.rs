//! The smoke run's result: per-check outcomes, and how they render.

use serde::Serialize;

/// Outcome of a single check. `XFail` is a known limitation that failed as expected;
/// `XPass` is a known limitation that has started passing — reported so the support
/// matrix does not quietly rot, but never a failure.
///
/// One report carries both renderings — the markdown table and the JSON — so [`Display`]
/// and the serde representation must spell each status identically, or the docs telling a
/// reader to grep the JSON for what they saw in the table would be wrong.
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
        f.write_str(match self {
            CheckStatus::Pass => "pass",
            CheckStatus::Fail => "fail",
            CheckStatus::XFail => "xfail",
            CheckStatus::XPass => "xpass",
            CheckStatus::Skip => "skip",
        })
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

#[derive(Debug, Clone, Serialize)]
pub struct SmokeReport {
    pub backend: String,
    pub version: String,
    /// The candidate app actually selected. Reports from different hosts are only
    /// comparable if this is visible.
    pub app: String,
    pub checks: Vec<CheckOutcome>,
}

impl SmokeReport {
    /// Only a hard `Fail` fails a run. A known limitation that failed (`XFail`) and one
    /// that started passing (`XPass`) are both reportable, neither is a failure.
    pub fn failed(&self) -> bool {
        self.checks.iter().any(|c| c.status == CheckStatus::Fail)
    }

    pub fn exit_code(&self) -> i32 {
        i32::from(self.failed())
    }

    pub fn to_markdown(&self) -> String {
        use std::fmt::Write;
        let mut out = String::new();
        let verdict = if self.failed() { "FAIL" } else { "PASS" };
        let _ = writeln!(
            out,
            "# glass smoke — {} — {verdict}\n\nglass-mcp {} · app: `{}`\n",
            self.backend, self.version, self.app
        );
        let _ = writeln!(out, "| # | check | status | detail |");
        let _ = writeln!(out, "|---|---|---|---|");
        for c in &self.checks {
            let _ = writeln!(
                out,
                "| {} | {} | {} | {} |",
                c.step,
                cell(&c.name),
                c.status,
                cell(&c.detail)
            );
        }
        let stale: Vec<&CheckOutcome> = self
            .checks
            .iter()
            .filter(|c| c.status == CheckStatus::XPass)
            .collect();
        if !stale.is_empty() {
            let _ = writeln!(out, "\n## Stale limitations\n");
            for c in stale {
                let _ = writeln!(
                    out,
                    "- `{}` is recorded as a known limitation but passed. Re-check the support matrix.",
                    c.name
                );
            }
        }
        out
    }
}

/// Make one value safe to splice into a markdown table row. A `|` would end the cell early
/// and shift every column after it; a newline would end the *table*, silently dropping every
/// remaining row — including, quite possibly, the failing one the reader is looking for.
/// Arbitrary text reaches these cells: `check_error_honesty` formats raw tool error text into
/// `detail`, and `GlassError::AccessibilityUnavailable` forwards backend stdout verbatim, so
/// neither character can be assumed absent.
fn cell(s: &str) -> String {
    s.replace("\r\n", "\n")
        .replace(['\n', '\r'], " ")
        .replace('|', "\\|")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn report(checks: Vec<CheckOutcome>) -> SmokeReport {
        SmokeReport {
            backend: "x11".into(),
            version: "1.1.0".into(),
            app: "xterm".into(),
            checks,
        }
    }

    #[test]
    fn a_failed_check_fails_the_report() {
        let r = report(vec![
            CheckOutcome::pass(1, "version", "1.1.0"),
            CheckOutcome::fail(2, "start", "no geometry returned"),
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
        // saw in the table, which only works while these two agree.
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
                "markdown and JSON must spell {status:?} the same way"
            );
        }
    }

    #[test]
    fn a_pipe_in_a_detail_does_not_shift_the_row() {
        let md = report(vec![CheckOutcome::fail(
            2,
            "start",
            "backend said: a | b | c",
        )])
        .to_markdown();
        let row = md
            .lines()
            .find(|l| l.contains("start"))
            .expect("the failing row must be in the table");
        assert_eq!(
            row.matches("\\|").count(),
            2,
            "every literal pipe in the detail must be escaped: {row}"
        );
    }

    #[test]
    fn a_newline_in_a_detail_does_not_truncate_the_table() {
        // The dangerous one: an unescaped newline ends the table, so every check after this
        // point silently disappears from the report — the failing one included.
        let md = report(vec![
            CheckOutcome::fail(2, "start", "line one\nline two"),
            CheckOutcome::fail(10, "stop", "the row that must not vanish"),
        ])
        .to_markdown();
        assert!(
            md.contains("the row that must not vanish"),
            "a later row must survive an earlier row's newline: {md}"
        );
        let rows = md.lines().filter(|l| l.starts_with("| ")).count();
        assert_eq!(rows, 3, "header row plus two check rows: {md}");
    }

    #[test]
    fn markdown_names_the_app_and_every_check_and_flags_stale_limits() {
        let mut xpass = CheckOutcome::pass(9, "gesture", "multi-contact worked");
        xpass.status = CheckStatus::XPass;
        let md = report(vec![CheckOutcome::pass(1, "version", "1.1.0"), xpass]).to_markdown();
        assert!(md.contains("xterm"), "selected app must appear: {md}");
        assert!(md.contains("version"), "every check must appear: {md}");
        assert!(md.contains("gesture"), "every check must appear: {md}");
        assert!(
            md.to_lowercase().contains("stale limitation"),
            "an XPass must be called out as a stale limitation: {md}"
        );
    }
}
