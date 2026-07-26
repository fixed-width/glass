//! `glass-mcp smoke` — drive glass's own MCP tool surface and report whether the
//! shipped binary works. Experimental surface: not covered by the 1.x freeze.

pub mod checks;
pub mod client;
pub mod envelope;
pub mod ledger;
pub mod profile;
pub mod report;
pub mod transport;

use profile::{Candidate, Profile, X11_CANDIDATES};
use report::{CheckOutcome, SmokeReport};
use transport::McpTransport;

pub struct SmokeOptions {
    /// Backend to exercise (see [`candidates_for`] for the supported set).
    pub backend: String,
    /// Force a specific candidate app instead of probing for the first one present on the host.
    pub app: Option<String>,
    /// Version the binary must report (the release tag). `None` skips check 1, recording why
    /// rather than omitting it — see [`version_check`].
    pub expect_version: Option<String>,
    /// Print the plan and exit without calling anything.
    pub dry_run: bool,
}

fn candidates_for(backend: &str) -> Result<&'static [Candidate], String> {
    match backend {
        "x11" => Ok(&X11_CANDIDATES),
        other => Err(format!(
            "unknown backend {other:?} — this build supports: x11. Pass --backend x11."
        )),
    }
}

/// The checks, in order, except `stop` (appended last) — step 7, envelope
/// discipline, is asserted inside every other check rather than standing alone.
/// Used by `--dry-run` and to keep the report shape stable whether or not a
/// check ran.
const CHECK_NAMES: [(u8, &str); 8] = [
    (1, "version"),
    (2, "start"),
    (3, "capabilities+doctor"),
    (4, "screenshot"),
    (5, "a11y snapshot"),
    (6, "interaction"),
    (8, "logs"),
    (9, "error honesty"),
];

/// Check 1. Runs [`checks::check_version`] when `--expect-version` was given; otherwise records
/// a `Skip` rather than omitting the check, so a real run's rows match `--dry-run`'s preview
/// (which always previews all nine, `version` included) instead of the row count depending on
/// whether the flag was passed.
fn version_check(t: &mut dyn McpTransport, expect_version: Option<&str>) -> CheckOutcome {
    match expect_version {
        Some(expected) => checks::check_version(t, expected),
        None => CheckOutcome::skip(1, "version", "no --expect-version given"),
    }
}

pub fn run(opts: SmokeOptions) -> Result<SmokeReport, String> {
    let candidates = candidates_for(&opts.backend)?;
    let app = match &opts.app {
        Some(name) => candidates.iter().find(|c| c.label == name).ok_or_else(|| {
            let names: Vec<&str> = candidates.iter().map(|c| c.label).collect();
            format!(
                "unknown app {name:?} for {} — use one of: {}",
                opts.backend,
                names.join(", ")
            )
        })?,
        None => profile::resolve_app(candidates, &profile::on_path)?,
    };
    let p = Profile {
        backend: opts.backend.clone(),
        app,
    };

    if opts.dry_run {
        let mut checks: Vec<CheckOutcome> = CHECK_NAMES
            .iter()
            .map(|(step, name)| CheckOutcome::skip(*step, name, "dry run"))
            .collect();
        checks.push(CheckOutcome::skip(10, "stop", "dry run"));
        return Ok(SmokeReport {
            backend: opts.backend,
            version: crate::VERSION.to_string(),
            app: app.label.to_string(),
            checks,
        });
    }

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    let mut t = client::StdioClient::spawn(&exe, &[])?;
    let version = t.server_version()?;
    let mut out = vec![
        version_check(&mut t, opts.expect_version.as_deref()),
        checks::check_start(&mut t, &p),
        checks::check_health(&mut t),
        checks::check_screenshot(&mut t),
    ];
    let (a11y, nodes) = checks::check_a11y(&mut t);
    out.push(a11y);
    out.push(checks::check_interaction(&mut t, &nodes));
    out.push(checks::check_logs(&mut t));
    out.push(checks::check_error_honesty(&mut t));
    out.push(checks::check_stop(&mut t));

    let checks = out
        .into_iter()
        .map(|c| ledger::apply(&opts.backend, c))
        .collect();
    Ok(SmokeReport {
        backend: opts.backend,
        version,
        app: app.label.to_string(),
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smoke::report::CheckStatus;
    use crate::smoke::transport::ScriptedTransport;

    #[test]
    fn expect_version_none_is_a_skip_not_an_omission() {
        // A real run's row count must match `--dry-run`'s preview regardless of whether
        // `--expect-version` was given, so a missing flag records why check 1 didn't run
        // rather than dropping it from the report.
        let mut t = ScriptedTransport::new(vec![]);
        let out = version_check(&mut t, None);
        assert_eq!(out.step, 1);
        assert_eq!(out.name, "version");
        assert_eq!(out.status, CheckStatus::Skip);
    }

    #[test]
    fn expect_version_some_reaches_check_version_pass_and_fail() {
        // ScriptedTransport::server_version() always reports "0.0.0-scripted" — see transport.rs.
        let mut t = ScriptedTransport::new(vec![]);
        let pass = version_check(&mut t, Some("0.0.0-scripted"));
        assert_eq!(pass.status, CheckStatus::Pass);

        let mut t = ScriptedTransport::new(vec![]);
        let fail = version_check(&mut t, Some("1.1.0"));
        assert_eq!(fail.status, CheckStatus::Fail);
        assert!(fail.detail.contains("1.1.0"), "got: {}", fail.detail);
    }

    #[test]
    fn dry_run_reports_the_plan_without_calling_anything() {
        let r = run(SmokeOptions {
            backend: "x11".into(),
            app: None,
            expect_version: None,
            dry_run: true,
        })
        .unwrap();
        assert!(
            r.checks
                .iter()
                .all(|c| c.status == report::CheckStatus::Skip)
        );
        assert_eq!(r.exit_code(), 0);
    }

    #[test]
    fn an_unknown_backend_is_rejected_by_name() {
        let err = run(SmokeOptions {
            backend: "beos".into(),
            app: None,
            expect_version: None,
            dry_run: true,
        })
        .unwrap_err();
        assert!(err.contains("beos") && err.contains("x11"), "got: {err}");
    }

    #[test]
    fn an_explicit_app_override_must_exist_in_the_candidate_list() {
        let err = run(SmokeOptions {
            backend: "x11".into(),
            app: Some("emacs".into()),
            expect_version: None,
            dry_run: true,
        })
        .unwrap_err();
        assert!(err.contains("emacs"), "got: {err}");
    }
}
