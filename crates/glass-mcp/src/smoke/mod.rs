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

use std::ffi::OsStr;

use profile::{Candidate, Profile, WAYLAND_CANDIDATES, X11_CANDIDATES};
use report::{CheckOutcome, RunMode, SmokeReport, TargetApp};

pub struct SmokeOptions {
    /// Backend to exercise (see [`candidates_for`] for the supported set).
    pub backend: String,
    /// Force a specific candidate app instead of probing for the first one present on the host.
    pub app: Option<String>,
    /// Print the plan and exit without calling anything.
    pub dry_run: bool,
}

/// Every backend the smoke runner drives, with its candidate apps: the resolution below and the
/// errors it produces read this table, and `cli.rs`'s hand-written help is tested against it.
const DRIVABLE: &[(&str, &[Candidate])] =
    &[("x11", &X11_CANDIDATES), ("wayland", &WAYLAND_CANDIDATES)];

/// The drivable backend names, in table order, for a message that tells the caller what to pass.
pub fn drivable_backends() -> Vec<&'static str> {
    DRIVABLE.iter().map(|(name, _)| *name).collect()
}

/// What a bare `smoke` drives. Derived from the table so `--backend`'s clap default and the
/// error telling a caller what to pass cannot name different backends.
pub const DEFAULT_BACKEND: &str = DRIVABLE[0].0;

/// The reference doc's target-app table, rendered from [`DRIVABLE`]. A doc-sync test compares it
/// against the checked-in markdown, so a candidate can't land in the code and not in the docs.
pub fn render_candidate_table() -> String {
    let mut out = String::from("| Backend | Candidates, in probe order |\n|---|---|\n");
    for (backend, candidates) in DRIVABLE {
        let bins: Vec<String> = candidates.iter().map(|c| format!("`{}`", c.bin)).collect();
        out.push_str(&format!("| `{backend}` | {} |\n", bins.join(", ")));
    }
    out
}

/// The clause both resolution errors carry, spelled once so a test can scope its assertion to
/// the span derived from [`DRIVABLE`] rather than to surrounding prose that also names backends.
const DRIVES_CLAUSE: &str = "the smoke runner drives: ";

/// Resolve `--backend` through [`crate::recognized_backend`] — the crate's single
/// backend-recognition predicate — and return the canonical name alongside its candidate apps.
/// Keying the table off what that returns is what stops `smoke --backend X11` being rejected
/// while `GLASS_BACKEND=X11` is honoured everywhere else in the binary.
fn candidates_for(backend: &str) -> Result<(&'static str, &'static [Candidate]), String> {
    let drivable = drivable_backends().join(", ");
    let Some(name) = crate::recognized_backend(backend) else {
        return Err(format!(
            "unknown backend {backend:?} — glass knows: {}; {DRIVES_CLAUSE}{drivable}.",
            crate::BACKENDS.join(", ")
        ));
    };
    DRIVABLE
        .iter()
        .find(|(n, _)| *n == name)
        .map(|(n, c)| (*n, *c))
        .ok_or_else(|| {
            format!(
                "no smoke candidates for backend {name:?} yet — {DRIVES_CLAUSE}{drivable}. \
                 Pass --backend {DEFAULT_BACKEND}."
            )
        })
}

/// The checks, in order, except `stop` (appended last). Envelope discipline is not among
/// them and never was a step of its own: it is asserted inside every check below. Used by
/// `--dry-run` and to keep the report shape stable whether or not a check ran.
const CHECK_NAMES: [(u8, &str); 7] = [
    (1, "start"),
    (2, "capabilities+doctor"),
    (3, "screenshot"),
    (4, "a11y snapshot"),
    (5, "interaction"),
    (6, "logs"),
    (7, "error honesty"),
];

/// The last check, run after every other one whatever they reported.
const STOP_CHECK: (u8, &str) = (8, "stop");

/// Every `(step, name)` a report carries, in order. A `--dry-run` preview is built from this
/// directly and a real run's rows must match it, which is why the end-to-end gate compares a
/// real run against a `--dry-run` of the same binary rather than a hand-copied list.
fn planned_rows() -> Vec<(u8, &'static str)> {
    CHECK_NAMES
        .iter()
        .copied()
        .chain(std::iter::once(STOP_CHECK))
        .collect()
}

/// Every check name a report can carry. The known-limits ledger is validated against this,
/// so an entry naming a check that does not exist fails a test rather than silently never
/// matching and hard-failing a release over an accepted limitation.
pub fn all_check_names() -> Vec<&'static str> {
    planned_rows().into_iter().map(|(_, name)| name).collect()
}

/// The step whose `detail` carries an unavailable app: `start` is the check that would hit the gap.
const START_STEP: u8 = 1;

/// `--dry-run`'s rows: every check a real run would produce, each a `skip` saying what it
/// would have done. One of them says more than "dry run", because one of the run's inputs is
/// only visible here: the missing target app.
fn plan_checks(app: &TargetApp) -> Vec<CheckOutcome> {
    planned_rows()
        .into_iter()
        .map(|(step, name)| {
            let detail = match (step, app.note()) {
                (START_STEP, Some(note)) => note.to_string(),
                _ => "dry run".to_string(),
            };
            CheckOutcome::skip(step, name, detail)
        })
        .collect()
}

pub fn run(opts: SmokeOptions) -> Result<SmokeReport, String> {
    let path = std::env::var_os("PATH");
    run_with(opts, path.as_deref())
}

/// `run`'s actual logic, taking the search path to probe as a parameter rather than reading
/// `PATH` itself — the seam that lets tests drive it against a directory they control instead
/// of the host's real environment. `run` is the only caller that should pass the host's `PATH`.
fn run_with(opts: SmokeOptions, path: Option<&OsStr>) -> Result<SmokeReport, String> {
    let (backend, candidates) = candidates_for(&opts.backend)?;

    // Resolve an explicit `--app` up front, dry run or not: naming a candidate that doesn't
    // exist in the table is a typo in the caller's input, not an environment gap, so it must
    // be rejected the same way in both modes rather than only once probing would happen.
    let forced = match &opts.app {
        Some(name) => Some(candidates.iter().find(|c| c.label == name).ok_or_else(|| {
            let names: Vec<&str> = candidates.iter().map(|c| c.label).collect();
            format!(
                "unknown app {name:?} for {backend} — use one of: {}",
                names.join(", ")
            )
        })?),
        None => None,
    };

    if opts.dry_run {
        // Unlike a real run, dry-run must not fail just because no candidate is present — the
        // moment a user most wants the plan is while setting up — so it records the gap
        // instead of erroring. A forced `--app` is probed too: a plan naming an app the host
        // does not have is otherwise indistinguishable from one it does.
        let app = match forced {
            Some(c) if !profile::runnable(c, path) => TargetApp::Unavailable(format!(
                "would fail: --app {} was given, but {} is not runnable on PATH",
                c.label, c.bin
            )),
            Some(c) => TargetApp::Selected(c.label.to_string()),
            None => match profile::resolve_app(candidates, path) {
                Ok(c) => TargetApp::Selected(c.label.to_string()),
                Err(e) => TargetApp::Unavailable(e.plan_note(candidates)),
            },
        };
        return Ok(SmokeReport {
            backend: backend.to_string(),
            // No server is spawned, so nothing reports a version over MCP; this is the one
            // compiled into the binary that would have been spawned.
            version: Some(crate::VERSION.to_string()),
            mode: RunMode::DryRun,
            checks: plan_checks(&app),
            app,
        });
    }

    // A real run does need an app to drive — unlike dry-run's preview, there is nothing to
    // fall back to here.
    let app = match forced {
        Some(c) => c,
        None => profile::resolve_app(candidates, path).map_err(|e| e.blocking_error(candidates))?,
    };
    let p = Profile {
        backend: backend.to_string(),
        app,
    };

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    // The spawned server resolves its own backend from `GLASS_BACKEND`, and `glass_doctor`
    // grades whatever that resolution produced. So hand it the backend under test rather than
    // inheriting the caller's shell, or check 2 would grade a backend this run is not
    // exercising. `check_health` re-reads the active backend, so this plumbing breaking is a
    // visible failure rather than a silently misdirected verdict.
    let mut t = client::StdioClient::spawn(&exe, &[("GLASS_BACKEND", backend)])?;
    let version = t.server_version();
    let mut out = vec![
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
        mode: RunMode::Full,
        app: TargetApp::Selected(app.label.to_string()),
        checks,
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Every row a report must carry, in order, written out rather than derived from
    /// [`CHECK_NAMES`] — a list checked against itself pins nothing. Deleting a check must
    /// fail here, in the default `cargo test` suite, not only in the `#[ignore]`d x11/wayland gates.
    const CANONICAL_ROWS: [(u8, &str); 8] = [
        (1, "start"),
        (2, "capabilities+doctor"),
        (3, "screenshot"),
        (4, "a11y snapshot"),
        (5, "interaction"),
        (6, "logs"),
        (7, "error honesty"),
        (8, "stop"),
    ];

    /// A search path holding real executables named `bins` — empty for the shape of a bare CI
    /// runner with none of the candidates installed. The `TempDir` must stay bound for as long
    /// as the path is used: dropping it deletes the directory.
    fn host_with(bins: &[&str]) -> (tempfile::TempDir, std::ffi::OsString) {
        profile::path_fixture(bins, &[])
    }

    fn dry_run(app: Option<&str>, path: Option<&OsStr>) -> SmokeReport {
        run_with(
            SmokeOptions {
                backend: "x11".into(),
                app: app.map(str::to_string),
                dry_run: true,
            },
            path,
        )
        .expect("a dry run on a known backend must produce a plan")
    }

    fn rows(r: &SmokeReport) -> Vec<(u8, &str)> {
        r.checks.iter().map(|c| (c.step, c.name.as_str())).collect()
    }

    /// The span of a resolution error interpolated from [`DRIVABLE`], up to the period that ends
    /// the clause. Both surrounding sentences also name backends — `crate::BACKENDS` before it,
    /// `Pass --backend x11.` after — so an assertion over the whole message holds vacuously.
    fn drives_clause(err: &str) -> &str {
        let (_, rest) = err
            .split_once(DRIVES_CLAUSE)
            .unwrap_or_else(|| panic!("must have a drives clause: {err}"));
        rest.split_once('.')
            .unwrap_or_else(|| panic!("the drives clause must end in a period: {err}"))
            .0
    }

    fn detail_of<'a>(r: &'a SmokeReport, name: &str) -> &'a str {
        &r.checks
            .iter()
            .find(|c| c.name == name)
            .unwrap_or_else(|| panic!("no {name:?} row in {:?}", rows(r)))
            .detail
    }

    /// The invariant `run_with` is built on: the preview and a real run carry the same rows.
    /// Pinned against a written-out list, so deleting a [`CHECK_NAMES`] entry fails a test.
    #[test]
    fn a_plan_previews_every_check_in_order() {
        let (_dir, path) = host_with(&[]);
        let r = dry_run(None, Some(&path));
        assert_eq!(rows(&r), CANONICAL_ROWS.to_vec());
    }

    #[test]
    fn a_plan_runs_nothing_and_so_cannot_fail() {
        let (_dir, path) = host_with(&[]);
        let r = dry_run(None, Some(&path));
        assert!(!r.checks.is_empty(), "an empty plan would vacuously pass");
        assert!(
            r.checks
                .iter()
                .all(|c| c.status == report::CheckStatus::Skip)
        );
        assert_eq!(r.exit_code(), 0);
    }

    /// Previewing before any candidate is installed must not fail; the gap is reported on the
    /// `start` row, the check that would hit it, not in the slot that names the selected app.
    #[test]
    fn a_plan_with_no_candidate_says_so_on_the_check_that_would_fail() {
        let (_dir, path) = host_with(&[]);
        let r = dry_run(None, Some(&path));
        let start = detail_of(&r, "start");
        assert!(
            start.contains("install") && start.contains("zenity"),
            "the start row must name what to install: {start}"
        );
        assert_eq!(
            r.app,
            TargetApp::Unavailable(start.to_string()),
            "the app field must model the gap, not carry it as a label"
        );
    }

    /// A heading, an exit code and eight uniform skips all said "healthy" on a machine that
    /// could not run a single check. The heading is the channel that has to give.
    #[test]
    fn a_plan_does_not_head_its_report_pass() {
        let (_dir, path) = host_with(&[]);
        let text = dry_run(None, Some(&path)).to_text();
        let heading = text.lines().next().unwrap_or_default();
        assert!(heading.contains("plan only"), "got: {heading}");
    }

    #[test]
    fn a_plan_names_the_candidate_it_would_pick_when_one_is_present() {
        let (_dir, path) = host_with(&["zenity"]);
        let r = dry_run(None, Some(&path));
        assert_eq!(r.app, TargetApp::Selected("zenity".into()));
        assert_eq!(detail_of(&r, "start"), "dry run");
    }

    /// `--app` selects among the candidates; it does not conjure one. A plan naming an app the
    /// host does not have looked exactly like a plan that could run.
    #[test]
    fn a_plan_marks_a_forced_app_that_is_not_installed() {
        let (_dir, path) = host_with(&["zenity"]);
        let r = dry_run(Some("xterm"), Some(&path));
        let start = detail_of(&r, "start");
        assert!(start.contains("xterm"), "must name the forced app: {start}");
        assert!(
            matches!(r.app, TargetApp::Unavailable(_)),
            "got: {:?}",
            r.app
        );
    }

    #[test]
    fn a_plan_accepts_a_forced_app_that_is_installed() {
        let (_dir, path) = host_with(&["xterm"]);
        let r = dry_run(Some("xterm"), Some(&path));
        assert_eq!(r.app, TargetApp::Selected("xterm".into()));
    }

    /// An unset `PATH` is not "nothing installed": installing another candidate cannot fix it,
    /// so the plan must not send the reader off to do that.
    #[test]
    fn a_plan_with_no_search_path_says_to_set_path() {
        let r = dry_run(None, None);
        let start = detail_of(&r, "start");
        assert!(
            start.contains("PATH is unset"),
            "must name the real cause: {start}"
        );
    }

    #[test]
    fn an_unknown_backend_is_rejected_by_name() {
        let err = run_with(
            SmokeOptions {
                backend: "beos".into(),
                app: None,
                dry_run: true,
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("beos"), "got: {err}");
    }

    #[test]
    fn a_backend_name_is_recognized_the_same_way_the_rest_of_the_binary_recognizes_it() {
        // `recognized_backend` is case-insensitive, so a second, stricter recognition site
        // here would reject a spelling the rest of the binary accepts.
        let (_dir, path) = host_with(&[]);
        let r = run_with(
            SmokeOptions {
                backend: "X11".into(),
                app: None,
                dry_run: true,
            },
            Some(&path),
        )
        .expect("X11 must resolve the same way GLASS_BACKEND=X11 does");
        assert_eq!(
            r.backend, "x11",
            "the report must record the canonical name, not the spelling passed in"
        );
    }

    #[test]
    fn a_backend_glass_knows_but_smoke_cannot_drive_yet_says_so() {
        let err = run_with(
            SmokeOptions {
                backend: "android".into(),
                app: None,
                dry_run: true,
            },
            None,
        )
        .unwrap_err();
        assert!(
            err.contains("android"),
            "must name the backend asked for: {err}"
        );
        let drives = drives_clause(&err);
        for b in drivable_backends() {
            assert!(
                drives.contains(b),
                "the drives clause must name {b:?}: {err}"
            );
        }
    }

    /// Nothing else requires a [`DRIVABLE`] name to be the canonical spelling `recognized_backend`
    /// returns. `("Wayland", …)` compiles, never matches the lookup below it, and conceals itself:
    /// the error prose and the CLI help test both read this table, so both go on calling a dead
    /// row drivable while `--backend Wayland` errors.
    #[test]
    fn every_drivable_row_can_actually_be_resolved() {
        let mut seen = std::collections::BTreeSet::new();
        for (name, candidates) in DRIVABLE {
            assert_eq!(
                crate::recognized_backend(name),
                Some(*name),
                "{name:?} is not the canonical spelling glass resolves to"
            );
            assert!(!candidates.is_empty(), "{name:?} has no candidate apps");
            assert!(seen.insert(*name), "{name:?} appears twice");
        }
    }

    /// Written out rather than derived from [`DRIVABLE`] — a list checked against itself pins
    /// nothing, and would stay green both if a name changed and if the table were emptied.
    #[test]
    fn drivable_backends_lists_every_row_of_the_table() {
        assert_eq!(drivable_backends(), ["x11", "wayland"]);
    }

    /// `xed` heads both tables, so naming one candidate would not tell the two apart.
    #[test]
    fn wayland_resolves_the_wayland_table() {
        let (name, candidates) = candidates_for("wayland").expect("wayland must be drivable");
        assert_eq!(name, "wayland");
        let resolved: Vec<&str> = candidates.iter().map(|c| c.bin).collect();
        let wayland: Vec<&str> = WAYLAND_CANDIDATES.iter().map(|c| c.bin).collect();
        assert_eq!(resolved, wayland);
    }

    /// What a Wayland user with nothing installed is told. A `start` row built from the x11 table
    /// would send them to install `xterm`, which cannot drive a Wayland session.
    #[test]
    fn a_wayland_plan_previews_the_same_rows_and_names_the_wayland_candidates() {
        let (_dir, path) = host_with(&[]);
        let r = run_with(
            SmokeOptions {
                backend: "wayland".into(),
                app: None,
                dry_run: true,
            },
            Some(&path),
        )
        .expect("wayland must be drivable");
        assert_eq!(rows(&r), CANONICAL_ROWS.to_vec());
        assert_eq!(r.backend, "wayland");
        let start = detail_of(&r, "start");
        assert!(
            start.contains("install") && start.contains("zenity"),
            "the start row must name what to install: {start}"
        );
        assert!(
            !start.contains("xterm"),
            "an X11-only client is not something to install for wayland: {start}"
        );
    }

    /// `--app` resolves against the backend under test, not x11's table: `xterm` is a valid x11
    /// candidate, so resolving it there would readmit through `--app` the client the wayland
    /// table exists to exclude.
    #[test]
    fn an_explicit_app_override_resolves_against_the_backend_under_test() {
        let (_dir, path) = host_with(&[]);
        let err = run_with(
            SmokeOptions {
                backend: "wayland".into(),
                app: Some("xterm".into()),
                dry_run: true,
            },
            Some(&path),
        )
        .unwrap_err();
        assert!(err.contains("xterm"), "must name the app asked for: {err}");
        // Scoped to the offer, because the rejected name is `xterm` too.
        let (_, offered) = err
            .split_once("use one of: ")
            .unwrap_or_else(|| panic!("must offer the backend's candidates: {err}"));
        assert!(
            !offered.contains("xterm"),
            "the wayland offer must not include an X11-only client: {err}"
        );
        for c in &WAYLAND_CANDIDATES {
            assert!(offered.contains(c.label), "must offer {}: {err}", c.label);
        }
    }

    /// The prose naming what is drivable is derived from the table, so a backend cannot land
    /// while a message still says otherwise.
    #[test]
    fn an_unknown_backend_names_every_drivable_backend() {
        let err = run_with(
            SmokeOptions {
                backend: "beos".into(),
                app: None,
                dry_run: true,
            },
            None,
        )
        .unwrap_err();
        let drives = drives_clause(&err);
        for b in drivable_backends() {
            assert!(
                drives.contains(b),
                "the drives clause must name {b:?}: {err}"
            );
        }
    }

    /// A name that is not in the candidate table at all is a typo in the caller's input, not
    /// an environment gap, so it stays a hard error in both modes.
    #[test]
    fn an_explicit_app_override_must_exist_in_the_candidate_list() {
        let (_dir, path) = host_with(&[]);
        let err = run_with(
            SmokeOptions {
                backend: "x11".into(),
                app: Some("emacs".into()),
                dry_run: true,
            },
            Some(&path),
        )
        .unwrap_err();
        assert!(err.contains("emacs"), "got: {err}");
    }
}
