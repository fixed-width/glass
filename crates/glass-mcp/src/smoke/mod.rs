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

use std::env::VarError;
use std::ffi::OsStr;
use std::time::Duration;

use profile::{
    ANDROID_CANDIDATES, Candidate, Profile, Resolution, WAYLAND_CANDIDATES, X11_CANDIDATES,
};
use report::{CheckOutcome, RunMode, SmokeReport, TargetApp};
use transport::CALL_TIMEOUT;

pub struct SmokeOptions {
    /// Backend to exercise (see [`backend_for`] for the supported set).
    pub backend: String,
    /// Drive this candidate instead of the one the backend's resolution would select.
    pub app: Option<String>,
    /// Print the plan and exit without calling anything.
    pub dry_run: bool,
}

/// One backend the smoke runner drives. The resolution below, the errors it produces and the
/// reference doc's rendered table all read this table; `cli.rs`'s hand-written help is tested
/// against it.
pub struct DrivableBackend {
    pub name: &'static str,
    pub candidates: &'static [Candidate],
    pub resolution: Resolution,
    /// Whether a launch may boot a device first. Tracked separately from `resolution` because
    /// they are different facts: an already-attached device needs no boot, and its target is
    /// preinstalled either way.
    pub boots_a_device: bool,
}

const DRIVABLE: &[DrivableBackend] = &[
    DrivableBackend {
        name: "x11",
        candidates: &X11_CANDIDATES,
        resolution: Resolution::OnPath,
        boots_a_device: false,
    },
    DrivableBackend {
        name: "wayland",
        candidates: &WAYLAND_CANDIDATES,
        resolution: Resolution::OnPath,
        boots_a_device: false,
    },
    DrivableBackend {
        name: "android",
        candidates: &ANDROID_CANDIDATES,
        resolution: Resolution::Preinstalled,
        boots_a_device: true,
    },
];

/// The drivable backend names, in table order, for a message that tells the caller what to pass.
pub fn drivable_backends() -> Vec<&'static str> {
    DRIVABLE.iter().map(|b| b.name).collect()
}

/// What a bare `smoke` drives. Derived from the table so `--backend`'s clap default and the
/// error telling a caller what to pass cannot name different backends.
pub const DEFAULT_BACKEND: &str = DRIVABLE[0].name;

/// The reference doc's target-app table, rendered from [`DRIVABLE`]. A doc-sync test compares it
/// against the checked-in markdown, so a candidate can't land in the code and not in the docs.
pub fn render_candidate_table() -> String {
    let mut out = String::from("| Backend | Chosen by | Candidates, in order |\n|---|---|---|\n");
    for b in DRIVABLE {
        let how = match b.resolution {
            Resolution::OnPath => "first runnable on `PATH`",
            Resolution::Preinstalled => {
                "preinstalled; the first row is the default, the rest need `--app`"
            }
        };
        let names: Vec<String> = b.candidates.iter().map(render_candidate).collect();
        out.push_str(&format!(
            "| `{}` | {how} | {} |\n",
            b.name,
            names.join(", ")
        ));
    }
    out
}

/// A candidate as the doc names it: the label alone where it is also the target, and the target
/// beside it where the two differ — as they do for a component nobody would want to read twice.
fn render_candidate(c: &Candidate) -> String {
    if c.label == c.bin {
        format!("`{}`", c.label)
    } else {
        format!("`{}` (`{}`)", c.label, c.bin)
    }
}

/// The clause both resolution errors carry, spelled once so a test can scope its assertion to
/// the span derived from [`DRIVABLE`] rather than to surrounding prose that also names backends.
const DRIVES_CLAUSE: &str = "the smoke runner drives: ";

/// Resolve `--backend` through [`crate::recognized_backend`] — the crate's single
/// backend-recognition predicate — and return the row. Keying the table off what that returns is
/// what stops `smoke --backend X11` being rejected while `GLASS_BACKEND=X11` is honoured
/// everywhere else in the binary.
fn backend_for(backend: &str) -> Result<&'static DrivableBackend, String> {
    let drivable = drivable_backends().join(", ");
    let Some(name) = crate::recognized_backend(backend) else {
        return Err(format!(
            "unknown backend {backend:?} — glass knows: {}; {DRIVES_CLAUSE}{drivable}.",
            crate::BACKENDS.join(", ")
        ));
    };
    DRIVABLE.iter().find(|b| b.name == name).ok_or_else(|| {
        format!(
            "no smoke candidates for backend {name:?} yet — {DRIVES_CLAUSE}{drivable}. \
             Pass --backend {DEFAULT_BACKEND}."
        )
    })
}

/// The emulator boot budget glass applies when [`BOOT_BUDGET_VAR`] is unset — the
/// android backend's own constant, not a second copy of the number: a smaller value here would
/// make this client give up while the device is still within the budget glass granted it.
const BOOT_BUDGET_DEFAULT_MS: u64 = glass_android::DEFAULT_BOOT_TIMEOUT_MS;

/// What an operator raises to grant a slow host a longer boot. Read here and, independently, by
/// `glass-android` itself, which is why both must derive the same number from it.
const BOOT_BUDGET_VAR: &str = "GLASS_EMULATOR_BOOT_TIMEOUT_MS";

/// How long `glass_start` may take. Derived rather than chosen: a backend that may boot a device
/// must outlast the device's own boot budget, or this client gives up before the boot it is
/// waiting for can finish — and reports it as a launch failure, which is the hardest thing there
/// is to diagnose from the report. The addend is `CALL_TIMEOUT`, not a fraction of it: once the
/// device is up, installing and launching the app is just an ordinary call, so it gets an
/// ordinary call's worth of time on top of the whole boot budget — at every budget, not only
/// above some threshold. That also means the result is never below `CALL_TIMEOUT`, so nothing
/// has to clamp it back up for a small budget. `get` and `warn` are injected so the arithmetic
/// and what it reports are both testable without touching the process environment.
fn start_deadline(
    b: &DrivableBackend,
    get: &dyn Fn(&str) -> Result<String, VarError>,
    warn: &mut dyn FnMut(String),
) -> Duration {
    if !b.boots_a_device {
        return CALL_TIMEOUT;
    }
    // Parsed exactly as `glass-android` parses it — no `.trim()`, no wider grammar, non-UTF-8
    // falling back too — so the two always wait out the same budget. Reading it more liberally
    // here would derive a deadline from a budget the device itself fell back away from.
    let ms = match get(BOOT_BUDGET_VAR) {
        Ok(v) => match v.parse::<u64>() {
            Ok(ms) => ms,
            Err(_) => {
                warn(unusable_boot_budget(&v));
                BOOT_BUDGET_DEFAULT_MS
            }
        },
        // Set, and unusable to both readers. `std::env::var(..).ok()` would report this as unset,
        // which is the one thing it is not.
        Err(VarError::NotUnicode(v)) => {
            warn(unusable_boot_budget(&v.to_string_lossy()));
            BOOT_BUDGET_DEFAULT_MS
        }
        Err(VarError::NotPresent) => BOOT_BUDGET_DEFAULT_MS,
    };
    Duration::from_millis(ms) + CALL_TIMEOUT
}

/// What an operator who raised the boot budget and had it ignored has to be told: the value is
/// unusable to the boot as well, so the longer wait they asked for is in effect nowhere. Silence
/// here left a deliberately-raised budget indistinguishable from one never set.
fn unusable_boot_budget(value: &str) -> String {
    format!(
        "warning: {BOOT_BUDGET_VAR} is set to {value:?}, which is not a whole number of \
         milliseconds — using {BOOT_BUDGET_DEFAULT_MS} instead. glass's android backend reads \
         this variable the same way, so the boot itself falls back to the same default."
    )
}

/// Everything a real run's calls read about the app under test, assembled where the backend row,
/// its resolution policy and the search path are all in hand. Extracted from [`run_with`] because
/// a real run spawns a server and no test reaches it: this is the last point at which a derived
/// deadline and a derived offer can be checked against the backend they came from.
fn profile_for(
    b: &'static DrivableBackend,
    app: &'static Candidate,
    path: Option<&OsStr>,
    get: &dyn Fn(&str) -> Result<String, VarError>,
    warn: &mut dyn FnMut(String),
) -> Profile {
    Profile {
        backend: b.name.to_string(),
        app,
        alternatives: alternatives(b, app, path),
        start_deadline: start_deadline(b, get, warn),
    }
}

/// The candidates a failed launch may offer to try instead. Filtered by the backend's own
/// resolution, not by the selected label alone: under [`Resolution::OnPath`] this run probed, so
/// offering a candidate that is not runnable here sends the operator to an `--app` that fails the
/// same launch — a real run does not probe a forced app. Under [`Resolution::Preinstalled`]
/// nothing was probed, so nothing is known absent.
fn alternatives(b: &DrivableBackend, app: &Candidate, path: Option<&OsStr>) -> Vec<&'static str> {
    b.candidates
        .iter()
        .filter(|c| c.label != app.label && profile::runnable(c, path, b.resolution))
        .map(|c| c.label)
        .collect()
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
    let b = backend_for(&opts.backend)?;

    // Resolve an explicit `--app` up front, dry run or not: naming a candidate that doesn't
    // exist in the table is a typo in the caller's input, not an environment gap, so it must
    // be rejected the same way in both modes rather than only once probing would happen.
    let forced = match &opts.app {
        Some(name) => Some(
            b.candidates
                .iter()
                .find(|c| c.label == name)
                .ok_or_else(|| {
                    let names: Vec<&str> = b.candidates.iter().map(|c| c.label).collect();
                    format!(
                        "unknown app {name:?} for {} — use one of: {}",
                        b.name,
                        names.join(", ")
                    )
                })?,
        ),
        None => None,
    };

    if opts.dry_run {
        // Unlike a real run, dry-run must not fail just because no candidate is present — the
        // moment a user most wants the plan is while setting up — so it records the gap
        // instead of erroring. A forced `--app` is probed too: a plan naming an app the host
        // does not have is otherwise indistinguishable from one it does.
        let app = match forced {
            Some(c) if !profile::runnable(c, path, b.resolution) => {
                TargetApp::Unavailable(format!(
                    "would fail: --app {} was given, but {} is not runnable on PATH",
                    c.label, c.bin
                ))
            }
            Some(c) => TargetApp::Selected(c.label.to_string()),
            None => match profile::resolve_app(b.candidates, path, b.resolution) {
                Ok(c) => TargetApp::Selected(c.label.to_string()),
                Err(e) => TargetApp::Unavailable(e.plan_note(b.candidates)),
            },
        };
        return Ok(SmokeReport {
            backend: b.name.to_string(),
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
        None => profile::resolve_app(b.candidates, path, b.resolution)
            .map_err(|e| e.blocking_error(b.candidates))?,
    };
    let p = profile_for(b, app, path, &|k| std::env::var(k), &mut |m| {
        eprintln!("glass: {m}")
    });

    let exe = std::env::current_exe().map_err(|e| format!("cannot locate this binary: {e}"))?;
    // The spawned server resolves its own backend from `GLASS_BACKEND`, and `glass_doctor`
    // grades whatever that resolution produced. So hand it the backend under test rather than
    // inheriting the caller's shell, or check 2 would grade a backend this run is not
    // exercising. `check_health` re-reads the active backend, so this plumbing breaking is a
    // visible failure rather than a silently misdirected verdict.
    let mut t = client::StdioClient::spawn(&exe, &[("GLASS_BACKEND", b.name)])?;
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

    let checks = out.into_iter().map(|c| ledger::apply(b.name, c)).collect();
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
    use std::time::Duration;

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
                backend: "ios".into(),
                app: None,
                dry_run: true,
            },
            None,
        )
        .unwrap_err();
        // The branch, not just the backend name: the unknown-backend error also names what was
        // asked for and carries the same drives clause, so dropping `ios` from `crate::BACKENDS`
        // would leave this asserting about the wrong error.
        assert!(
            err.contains("no smoke candidates for backend"),
            "must be the not-yet-drivable branch: {err}"
        );
        assert!(
            err.contains("ios"),
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
    /// row drivable while `--backend Wayland` errors. Nor does anything else require a row's
    /// resolution to be reachable: a [`Resolution::Preinstalled`] row with an empty table resolves
    /// to an error whose prose tells the caller to install something, on a backend where
    /// installing is not the remedy.
    #[test]
    fn every_drivable_row_can_actually_be_resolved() {
        let mut seen = std::collections::BTreeSet::new();
        for b in DRIVABLE {
            assert_eq!(
                crate::recognized_backend(b.name),
                Some(b.name),
                "{:?} is not the canonical spelling glass resolves to",
                b.name
            );
            assert!(
                !b.candidates.is_empty(),
                "{:?} has no candidate for {:?} to return",
                b.name,
                b.resolution
            );
            assert!(seen.insert(b.name), "{:?} appears twice", b.name);
        }
    }

    /// Written out rather than derived from [`DRIVABLE`] — a list checked against itself pins
    /// nothing, and would stay green both if a name changed and if the table were emptied.
    #[test]
    fn drivable_backends_lists_every_row_of_the_table() {
        assert_eq!(drivable_backends(), ["x11", "wayland", "android"]);
    }

    /// `xed` heads both tables, so naming one candidate would not tell the two apart.
    #[test]
    fn wayland_resolves_the_wayland_table() {
        let b = backend_for("wayland").expect("wayland must be drivable");
        assert_eq!(b.name, "wayland");
        let resolved: Vec<&str> = b.candidates.iter().map(|c| c.bin).collect();
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

    /// The whole point of `Preinstalled`: an android plan resolves with no search path at all,
    /// because `PATH` says nothing about what is installed on a device.
    #[test]
    fn an_android_plan_names_the_default_component_without_a_search_path() {
        let r = run_with(
            SmokeOptions {
                backend: "android".into(),
                app: None,
                dry_run: true,
            },
            None,
        )
        .expect("android must be drivable with no PATH");
        assert_eq!(r.backend, "android");
        assert_eq!(r.app, TargetApp::Selected("contacts".into()));
        assert_eq!(
            detail_of(&r, "start"),
            "dry run",
            "nothing was probed, so there is no gap to report"
        );
    }

    /// Without a probe the table is not a fallback chain — every row but the first exists only for
    /// `--app`, so `--app` has to reach them.
    #[test]
    fn an_android_plan_honours_a_forced_candidate() {
        let r = run_with(
            SmokeOptions {
                backend: "android".into(),
                app: Some("settings".into()),
                dry_run: true,
            },
            None,
        )
        .expect("android must be drivable with no PATH");
        assert_eq!(r.app, TargetApp::Selected("settings".into()));
    }

    /// `--app` selects among the backend's own candidates. An x11 label must not resolve here, or a
    /// run would try to launch `xed` as an activity component.
    #[test]
    fn an_android_forced_app_resolves_against_the_android_table() {
        let err = run_with(
            SmokeOptions {
                backend: "android".into(),
                app: Some("xed".into()),
                dry_run: true,
            },
            None,
        )
        .unwrap_err();
        assert!(err.contains("xed"), "must name the app asked for: {err}");
        let (_, offered) = err
            .split_once("use one of: ")
            .unwrap_or_else(|| panic!("must offer the backend's candidates: {err}"));
        for label in ["contacts", "settings"] {
            assert!(offered.contains(label), "must offer {label:?}: {err}");
        }
    }

    /// The rendered doc names the component for a candidate whose label is not its target, and does
    /// not repeat itself for one whose label is. Written against the two shapes rather than against
    /// the renderer, which would agree with itself.
    #[test]
    fn the_rendered_table_names_a_component_but_does_not_repeat_a_bare_label() {
        let t = render_candidate_table();
        assert!(
            t.contains(
                "`contacts` (`com.google.android.contacts/com.android.contacts.activities.CompactContactEditorActivity`)"
            ),
            "an android row must name the component an operator would pass: {t}"
        );
        assert!(
            !t.contains("`xed` (`xed`)"),
            "a candidate whose label is its target must be named once: {t}"
        );
    }

    /// A reader has to know that android's table is not a fallback chain, and the only place the doc
    /// can learn it is the renderer. Checked per row rather than by substring anywhere in the table:
    /// a `contains` over the whole table would still pass with the two `Resolution` arms' bodies
    /// swapped, since both phrases would still appear somewhere, just on the wrong rows.
    #[test]
    fn the_rendered_table_says_how_each_backend_chooses() {
        let t = render_candidate_table();
        let row = |name: &str| {
            t.lines()
                .find(|l| l.starts_with(&format!("| `{name}` |")))
                .unwrap_or_else(|| panic!("no {name:?} row in {t}"))
        };
        assert!(
            row("android").contains("the first row is the default"),
            "{t}"
        );
        for probing in ["x11", "wayland"] {
            assert!(
                row(probing).contains("first runnable on `PATH`"),
                "{probing}: {t}"
            );
        }
    }

    fn android() -> &'static DrivableBackend {
        backend_for("android").expect("android must be drivable")
    }

    /// An environment where nothing at all is set.
    fn unset(_: &str) -> Result<String, VarError> {
        Err(VarError::NotPresent)
    }

    /// An environment holding the boot budget and nothing else, answering as `std::env::var` does.
    fn budget_of(v: &'static str) -> impl Fn(&str) -> Result<String, VarError> {
        move |k| match k {
            "GLASS_EMULATOR_BOOT_TIMEOUT_MS" => Ok(v.to_string()),
            _ => Err(VarError::NotPresent),
        }
    }

    /// The deadline together with everything deriving it warned about, so a test can assert on
    /// both. A warning nobody can observe is one a test cannot require.
    fn deadline_and_warnings(
        b: &DrivableBackend,
        get: &dyn Fn(&str) -> Result<String, VarError>,
    ) -> (Duration, Vec<String>) {
        let mut warnings = Vec::new();
        let d = start_deadline(b, get, &mut |m| warnings.push(m));
        (d, warnings)
    }

    fn deadline(b: &DrivableBackend, get: &dyn Fn(&str) -> Result<String, VarError>) -> Duration {
        deadline_and_warnings(b, get).0
    }

    /// A backend that launches into a compositor already running does not need to outlast a boot.
    #[test]
    fn a_backend_that_never_boots_a_device_keeps_the_default_deadline() {
        let x11 = backend_for("x11").expect("x11 must be drivable");
        assert!(!x11.boots_a_device);
        assert_eq!(deadline(x11, &unset), CALL_TIMEOUT);
    }

    /// The relationship the whole function exists for: a client deadline inside the device's own boot
    /// budget makes an ordinary cold boot look like a failed launch.
    #[test]
    fn a_booting_backend_outlasts_the_devices_own_boot_budget() {
        let d = deadline(android(), &unset);
        assert!(
            d > Duration::from_millis(BOOT_BUDGET_DEFAULT_MS),
            "a deadline inside the boot budget reports the boot as a launch failure: {d:?}"
        );
        assert_eq!(
            d,
            Duration::from_millis(BOOT_BUDGET_DEFAULT_MS) + CALL_TIMEOUT
        );
    }

    /// Raising the device's budget must raise this one, and quietly: a warning on a value that was
    /// honoured would train an operator to ignore the one that says it was not.
    #[test]
    fn raising_the_boot_budget_raises_the_deadline() {
        let (d, warnings) = deadline_and_warnings(android(), &budget_of("600000"));
        assert_eq!(d, Duration::from_millis(600_000) + CALL_TIMEOUT);
        assert_eq!(warnings, Vec::<String>::new());
    }

    /// An unreadable value is not a shorter budget: glass falls back to its own default, so this must
    /// too, or the two would disagree about how long a boot may take.
    #[test]
    fn an_unreadable_boot_budget_falls_back_to_the_default() {
        for v in ["", "   ", "soon", "-1", "12.5", "99999999999999999999"] {
            assert_eq!(
                deadline(android(), &budget_of(v)),
                Duration::from_millis(BOOT_BUDGET_DEFAULT_MS) + CALL_TIMEOUT,
                "{v:?} must not shorten the deadline"
            );
        }
    }

    /// `glass-android` parses this same variable with a bare `.parse()` — no `.trim()` — so a
    /// whitespace-padded value is unreadable to the device too. Trimming it here would derive a
    /// deadline from a boot budget the device itself fell back away from, the two silently
    /// disagreeing about how long the boot may take. The padding an operator cannot see is
    /// exactly why the fallback has to announce itself.
    #[test]
    fn a_whitespace_padded_boot_budget_falls_back_to_the_default_like_glass_android() {
        let (d, warnings) = deadline_and_warnings(android(), &budget_of(" 600000 "));
        assert_eq!(
            d,
            Duration::from_millis(BOOT_BUDGET_DEFAULT_MS) + CALL_TIMEOUT
        );
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    }

    /// Falling back is right; falling back in silence is not. The operator who raised the budget
    /// for a slow host otherwise cannot tell a value that was ignored from one never set — and the
    /// boot is not getting it either, which is the part they have to hear.
    #[test]
    fn an_unreadable_boot_budget_says_so_rather_than_falling_back_in_silence() {
        let (_, warnings) = deadline_and_warnings(android(), &budget_of("300s"));
        let [w] = warnings.as_slice() else {
            panic!("exactly one warning, got: {warnings:?}");
        };
        assert!(
            w.contains("GLASS_EMULATOR_BOOT_TIMEOUT_MS"),
            "must name the variable: {w}"
        );
        assert!(
            w.contains("300s"),
            "must quote the value it could not use: {w}"
        );
        assert!(
            w.contains(&BOOT_BUDGET_DEFAULT_MS.to_string()),
            "must name the budget used instead: {w}"
        );
        assert!(
            w.contains("android backend reads this variable the same way"),
            "must say the boot falls back too: {w}"
        );
    }

    /// An unset variable is not a mistake — it is the ordinary case, and warning about it would
    /// bury the warning that matters.
    #[test]
    fn an_unset_boot_budget_warns_about_nothing() {
        let (_, warnings) = deadline_and_warnings(android(), &unset);
        assert_eq!(warnings, Vec::<String>::new());
    }

    /// A value whose bytes are not UTF-8 is *set* — `std::env::var(..).ok()` reports it as unset,
    /// which would leave an operator's garbled budget as silent as no budget at all.
    #[test]
    fn a_non_utf8_boot_budget_is_set_but_unusable_not_unset() {
        let (d, warnings) =
            deadline_and_warnings(android(), &|_| Err(VarError::NotUnicode(not_utf8())));
        assert_eq!(
            d,
            Duration::from_millis(BOOT_BUDGET_DEFAULT_MS) + CALL_TIMEOUT,
            "the value glass-android derives from it is the default"
        );
        assert_eq!(warnings.len(), 1, "got: {warnings:?}");
    }

    /// A value `std::env::var` would reject as `NotUnicode`, built per platform because a
    /// non-UTF-8 environment value is spelled in bytes on one and in UTF-16 on the other.
    #[cfg(unix)]
    fn not_utf8() -> std::ffi::OsString {
        use std::os::unix::ffi::OsStringExt;
        std::ffi::OsString::from_vec(vec![0x33, 0x30, 0x30, 0x80])
    }
    #[cfg(windows)]
    fn not_utf8() -> std::ffi::OsString {
        use std::os::windows::ffi::OsStringExt;
        // An unpaired surrogate: representable in a Windows environment block, not in UTF-8.
        std::ffi::OsString::from_wide(&[0x33, 0x30, 0x30, 0xD800])
    }

    /// A boot budget smaller than one ordinary call still leaves a full ordinary call's worth of
    /// time to install and launch after the boot completes — the addend is unconditional, not a
    /// clamp that only engages above some threshold.
    #[test]
    fn a_tiny_boot_budget_still_leaves_a_full_call_to_launch() {
        assert_eq!(
            deadline(android(), &budget_of("1")),
            Duration::from_millis(1) + CALL_TIMEOUT
        );
    }

    /// What a real run drives with. A real run spawns a server, so no test reaches it — and a
    /// profile built with a written-out deadline would leave every test, the lint gate and the
    /// `#[ignore]`d gates green while a cold boot on a slow host reported itself as a failed launch.
    #[test]
    fn a_profile_carries_the_deadline_its_backend_derives() {
        let android = profile_for(android(), &ANDROID_CANDIDATES[0], None, &unset, &mut |_| ());
        assert!(
            android.start_deadline > Duration::from_millis(BOOT_BUDGET_DEFAULT_MS),
            "a launch that may boot a device must outlast the device's own budget: {:?}",
            android.start_deadline
        );

        let (_dir, path) = host_with(&["zenity"]);
        let x11 = backend_for("x11").expect("x11 must be drivable");
        let x11 = profile_for(x11, &X11_CANDIDATES[2], Some(&path), &unset, &mut |_| ());
        assert_eq!(
            x11.start_deadline, CALL_TIMEOUT,
            "a backend that launches into a running compositor waits out no boot"
        );
    }

    /// The host the offer used to mislead on: only `zenity` and `xterm` are installed, so the run
    /// selects `zenity` — and the two candidates ahead of it are ones this run's own probe just
    /// found absent. Offering them sends the operator to an `--app` that fails the same launch,
    /// because a real run does not probe a forced app.
    #[test]
    fn an_offer_from_a_probing_backend_names_only_what_is_installed() {
        let (_dir, path) = host_with(&["zenity", "xterm"]);
        let b = backend_for("x11").expect("x11 must be drivable");
        let app = profile::resolve_app(b.candidates, Some(&path), b.resolution)
            .expect("zenity is installed");
        assert_eq!(app.label, "zenity", "the fixture must select a middle row");
        let p = profile_for(b, app, Some(&path), &unset, &mut |_| ());
        assert_eq!(p.alternatives, ["xterm"]);
    }

    /// Nothing probed the device, so nothing about it is known absent and every other row is worth
    /// offering. `label` and `bin` diverge only on android, so this is also the one table where an
    /// offer built from `bin` would read out a raw component string instead of the short name
    /// `--app` accepts.
    #[test]
    fn an_offer_from_a_preinstalled_backend_names_every_other_row() {
        let p = profile_for(android(), &ANDROID_CANDIDATES[0], None, &unset, &mut |_| ());
        assert_eq!(
            p.alternatives,
            ["settings"],
            "written out, because the component `settings` stands for is not what --app takes"
        );
    }

    /// The selected candidate is not an alternative to itself; offering it would send the operator
    /// back to the run that just failed. Both policies, because each builds its own list.
    #[test]
    fn an_offer_never_names_the_candidate_the_run_selected() {
        let (_dir, path) = host_with(&["zenity", "xterm"]);
        let b = backend_for("x11").expect("x11 must be drivable");
        let p = profile_for(b, &X11_CANDIDATES[3], Some(&path), &unset, &mut |_| ());
        assert_eq!(p.app.label, "xterm");
        assert_eq!(p.alternatives, ["zenity"]);

        let p = profile_for(android(), &ANDROID_CANDIDATES[1], None, &unset, &mut |_| ());
        assert_eq!(p.alternatives, ["contacts"]);
    }

    /// The reader is scoped to one variable: reading any name would let an unrelated setting move a
    /// deadline nobody connected to it.
    #[test]
    fn the_deadline_reads_only_the_boot_budget_variable() {
        let every_variable = |_: &str| Ok("600000".to_string());
        assert_ne!(
            deadline(android(), &budget_of("600000")),
            deadline(android(), &unset)
        );
        assert_eq!(
            deadline(android(), &every_variable),
            deadline(android(), &budget_of("600000"))
        );
    }
}
