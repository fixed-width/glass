//! `glass-mcp smoke` — drive glass's own MCP tool surface and report whether the
//! shipped binary works. Experimental surface: not covered by the 1.x freeze.

pub mod checks;
pub mod client;
pub mod envelope;
pub mod ledger;
pub mod profile;
pub mod report;
pub mod selfcheck;
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

/// Resolve `--backend` through [`crate::recognized_backend`] — the crate's single
/// backend-recognition predicate — and return the canonical name alongside its candidate
/// apps. Keying the table off what that returns, rather than matching a literal here, is
/// what stops `smoke --backend X11` being rejected while `GLASS_BACKEND=X11` is honoured
/// everywhere else in the binary.
fn candidates_for(backend: &str) -> Result<(&'static str, &'static [Candidate]), String> {
    let Some(name) = crate::recognized_backend(backend) else {
        return Err(format!(
            "unknown backend {backend:?} — glass knows: {}. The smoke runner drives: x11.",
            crate::BACKENDS.join(", ")
        ));
    };
    match name {
        "x11" => Ok((name, &X11_CANDIDATES)),
        other => Err(format!(
            "no smoke candidates for backend {other:?} yet — this build drives: x11. \
             Pass --backend x11."
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

/// The last check, run after every other one whatever they reported.
const STOP_CHECK: (u8, &str) = (10, "stop");

/// Every check name a report can carry. The known-limits ledger is validated against this,
/// so an entry naming a check that does not exist fails a test rather than silently never
/// matching and hard-failing a release over an accepted limitation.
#[cfg(test)]
pub(crate) fn all_check_names() -> Vec<&'static str> {
    CHECK_NAMES
        .iter()
        .map(|(_, name)| *name)
        .chain(std::iter::once(STOP_CHECK.1))
        .collect()
}

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
    let (backend, candidates) = candidates_for(&opts.backend)?;
    let app = match &opts.app {
        Some(name) => candidates.iter().find(|c| c.label == name).ok_or_else(|| {
            let names: Vec<&str> = candidates.iter().map(|c| c.label).collect();
            format!(
                "unknown app {name:?} for {backend} — use one of: {}",
                names.join(", ")
            )
        })?,
        None => profile::resolve_app(candidates, &profile::on_path)?,
    };
    let p = Profile {
        backend: backend.to_string(),
        app,
    };

    if opts.dry_run {
        let mut checks: Vec<CheckOutcome> = CHECK_NAMES
            .iter()
            .map(|(step, name)| CheckOutcome::skip(*step, name, "dry run"))
            .collect();
        checks.push(CheckOutcome::skip(STOP_CHECK.0, STOP_CHECK.1, "dry run"));
        return Ok(SmokeReport {
            backend: p.backend,
            version: crate::VERSION.to_string(),
            app: app.label.to_string(),
            checks,
        });
    }

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    // The spawned server resolves its own backend from `GLASS_BACKEND`, and `glass_doctor`
    // takes no backend argument — its verdict grades whatever that resolution produced. So
    // hand it the backend under test rather than inheriting whatever the caller's shell has
    // set, or check 3 would grade a backend this run is not exercising. `check_health`
    // re-reads the server's active backend and fails on a mismatch, so this plumbing breaking
    // is a visible failure rather than a silently misdirected verdict.
    let mut t = client::StdioClient::spawn(&exe, &[("GLASS_BACKEND", backend)])?;
    let version = t.server_version()?;
    let mut out = vec![
        version_check(&mut t, opts.expect_version.as_deref()),
        checks::check_start(&mut t, &p),
        checks::check_health(&mut t, &p),
        checks::check_screenshot(&mut t),
    ];
    let (a11y, nodes) = checks::check_a11y(&mut t);
    out.push(a11y);
    out.push(checks::check_interaction(&mut t, nodes.as_deref()));
    out.push(checks::check_logs(&mut t));
    out.push(checks::check_error_honesty(&mut t));
    out.push(checks::check_stop(&mut t));

    let checks = out.into_iter().map(|c| ledger::apply(backend, c)).collect();
    Ok(SmokeReport {
        backend: p.backend,
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
        let mut t = ScriptedTransport::new(vec![]).with_version("1.1.0");
        let pass = version_check(&mut t, Some("1.1.0"));
        assert_eq!(pass.status, CheckStatus::Pass);

        let mut t = ScriptedTransport::new(vec![]).with_version("1.0.0");
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
    fn a_backend_name_is_recognized_the_same_way_the_rest_of_the_binary_recognizes_it() {
        // `recognized_backend` is case-insensitive, so `GLASS_BACKEND=X11` is honoured
        // everywhere else; a second, stricter recognition site here would reject the same
        // spelling the binary otherwise accepts.
        let r = run(SmokeOptions {
            backend: "X11".into(),
            app: None,
            expect_version: None,
            dry_run: true,
        })
        .expect("X11 must resolve the same way GLASS_BACKEND=X11 does");
        assert_eq!(
            r.backend, "x11",
            "the report must record the canonical name, not the spelling passed in"
        );
    }

    #[test]
    fn a_backend_glass_knows_but_smoke_cannot_drive_yet_says_so() {
        let err = run(SmokeOptions {
            backend: "wayland".into(),
            app: None,
            expect_version: None,
            dry_run: true,
        })
        .unwrap_err();
        assert!(
            err.contains("wayland") && err.contains("x11"),
            "must name the backend asked for and what is drivable: {err}"
        );
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
