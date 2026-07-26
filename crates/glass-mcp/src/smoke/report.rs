//! The smoke run's result: per-check outcomes, and how they render.

use serde::Serialize;

/// Outcome of a single check. `XFail` is a known limitation that failed as expected;
/// `XPass` is a known limitation that has started passing — reported so the support
/// matrix does not quietly rot, but never a failure.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CheckStatus {
    Pass,
    Fail,
    XFail,
    XPass,
    Skip,
}

#[derive(Debug, Clone, Serialize)]
pub struct CheckOutcome {
    pub step: u8,
    pub name: String,
    pub status: CheckStatus,
    /// What happened, in one line. On a failure this must say enough to triage
    /// without re-running.
    pub detail: String,
    pub retries: u32,
}

impl CheckOutcome {
    pub fn pass(step: u8, name: &str, detail: impl Into<String>) -> Self {
        Self {
            step,
            name: name.into(),
            status: CheckStatus::Pass,
            detail: detail.into(),
            retries: 0,
        }
    }

    pub fn fail(step: u8, name: &str, detail: impl Into<String>) -> Self {
        Self {
            step,
            name: name.into(),
            status: CheckStatus::Fail,
            detail: detail.into(),
            retries: 0,
        }
    }

    pub fn skip(step: u8, name: &str, detail: impl Into<String>) -> Self {
        Self {
            step,
            name: name.into(),
            status: CheckStatus::Skip,
            detail: detail.into(),
            retries: 0,
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
        let _ = writeln!(out, "| # | check | status | retries | detail |");
        let _ = writeln!(out, "|---|---|---|---|---|");
        for c in &self.checks {
            let _ = writeln!(
                out,
                "| {} | {} | {:?} | {} | {} |",
                c.step, c.name, c.status, c.retries, c.detail
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
