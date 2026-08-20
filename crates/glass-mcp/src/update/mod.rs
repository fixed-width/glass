//! `glass-mcp update`: fetch the latest release, verify it, and replace this binary.
//!
//! The step order below is a contract the tests assert: the from-source refusal happens before any
//! network request, the file the download will land in is created before anything is downloaded,
//! and consent is taken before the download rather than before the swap.

mod release;
mod swap;
#[cfg(test)]
mod testserver;
mod verify;
mod version;

use crate::color::ColorChoice;
use anyhow::Context as _;
use release::ReleaseSource;
use std::io::IsTerminal;
use std::path::{Path, PathBuf};
use verify::Attestation;

/// Everything the flow needs to know that is not the release itself.
///
/// `interactive`, `smoke` and `attest_program` are separate from the CLI flags so the tests can
/// drive the non-interactive path, skip executing a fixture that is not a real binary, and reach
/// step 8b without running the real `gh attestation verify`, which queries GitHub.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) check: bool,
    pub(crate) yes: bool,
    pub(crate) skip_attestation: bool,
    pub(crate) json: bool,
    pub(crate) color: ColorChoice,
    pub(crate) interactive: bool,
    pub(crate) smoke: bool,
    pub(crate) attest_program: &'static str,
}

impl Options {
    pub(crate) fn from_flags(
        check: bool,
        yes: bool,
        skip_attestation: bool,
        json: bool,
        color: ColorChoice,
    ) -> Self {
        Options {
            check,
            yes,
            skip_attestation,
            json,
            color,
            // `--json` must never block on a prompt, nor share stdout with the three human lines
            // `confirm` prints; consent there comes from `--yes` alone.
            interactive: !json && std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            smoke: true,
            attest_program: verify::GH,
        }
    }
}

#[derive(Debug)]
pub(crate) enum Refusal {
    /// A local build. Replacing it with a release binary would be a silent substitution.
    FromSource(String),
    /// No published asset for this target.
    UnsupportedTarget,
    /// macOS installs are an app bundle, not a bare binary.
    MacosBundle,
    /// The temp file step 6 creates beside the installed binary could not be created. Named for
    /// what failed rather than for a diagnosis: a read-only directory is the common cause, but
    /// `ENOSPC`, a disk quota, a path-length limit and an `EEXIST` race all land here too, and the
    /// `io::Error` is the only thing that can tell them apart.
    CannotStage {
        dir: PathBuf,
        why: String,
    },
    NeedsConsent,
    ChecksumMismatch {
        expected: String,
        got: String,
    },
    /// The `.sha256` sidecar could not be used. Carries the parse error itself rather than its
    /// text, because "unreadable" and "for a different asset" are different failures and the
    /// message has to tell them apart.
    BadSidecar(verify::SidecarError),
    AttestationFailed(String),
    SmokeCheckFailed(String),
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Checked,
    UpToDate,
    Updated,
    Refused(Refusal),
    /// The flow could not finish: the release endpoint was unreachable, answered with something
    /// that is not a release, or a download/swap failed. Distinct from `Refused` — a refusal is a
    /// decision this command made about this install, while this is the command not getting far
    /// enough to make one.
    Error(String),
}

/// What happened, and everything the renderer needs to say so.
#[derive(Debug)]
pub(crate) struct Report {
    pub(crate) outcome: Outcome,
    pub(crate) current: String,
    pub(crate) latest: Option<String>,
    /// Whether `latest` is strictly newer than `current`. `false` while the latest release is
    /// still unknown, and `false` for a from-source build, whose version is not comparable.
    pub(crate) update_available: bool,
    pub(crate) asset: Option<String>,
    pub(crate) url: Option<String>,
    pub(crate) install_path: PathBuf,
    pub(crate) attestation: AttestationStatus,
    /// Whether the apply path can run on this target at all. Independent of `outcome` because
    /// `--check` reports it on macOS without refusing.
    pub(crate) supported: bool,
    /// Whether `current` parsed as a released version at all. `false` for a from-source build,
    /// where `update_available` is also `false` — "unknown", not "you are up to date".
    pub(crate) current_comparable: bool,
    /// Whether a `glass-mcp serve --http` process is currently answering `/healthz`. Only ever set
    /// after a successful update, by `run_cli` — the only caller that can probe the real process.
    pub(crate) running_server: bool,
}

/// The `attestation` field of the JSON output. `NotChecked` is the initial state — reached only by
/// `--check` and every refusal before step 8b, none of which touch provenance at all. `Skipped` is
/// `--skip-attestation`; `Unavailable` is `gh` not being installed.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttestationStatus {
    NotChecked,
    Verified,
    Unavailable,
    Skipped,
    Failed,
}

impl Report {
    /// The report before anything has been resolved: everything knowable from the process itself,
    /// with every field that needs the network still unset. [`run`] starts from this, and
    /// [`run_cli`] reuses it to render a failure that stopped `run` outright — so a caller parsing
    /// `--json` sees the same field set whether the run succeeded, refused, or failed.
    fn initial(current: &str, exe: &Path) -> Report {
        Report {
            outcome: Outcome::Checked,
            current: current.to_string(),
            latest: None,
            update_available: false,
            asset: None,
            url: None,
            install_path: exe.to_path_buf(),
            attestation: AttestationStatus::NotChecked,
            supported: !cfg!(target_os = "macos") && release::asset_suffix().is_some(),
            current_comparable: version::Version::parse_released(current).is_some(),
            running_server: false,
        }
    }

    fn refused(mut self, why: Refusal) -> Report {
        self.outcome = Outcome::Refused(why);
        self
    }

    fn finished(mut self, outcome: Outcome) -> Report {
        self.outcome = outcome;
        self
    }
}

/// The whole flow. Takes the current version and this binary's path as arguments rather than
/// reading them itself, so the tests exercise every branch without a real install.
pub(crate) async fn run(
    opts: Options,
    source: &ReleaseSource,
    current: &str,
    exe: &Path,
) -> anyhow::Result<Report> {
    let current_version = version::Version::parse_released(current);
    let mut report = Report::initial(current, exe);

    // 1. From-source, before any network request. A `--check` still reports.
    if !opts.check && current_version.is_none() {
        return Ok(report.refused(Refusal::FromSource(current.to_string())));
    }
    // 2. Unsupported target, also before the network. `--check` still reports.
    if !opts.check && !report.supported {
        let why = if cfg!(target_os = "macos") {
            Refusal::MacosBundle
        } else {
            Refusal::UnsupportedTarget
        };
        return Ok(report.refused(why));
    }

    // 3. Resolve the latest release. First network call.
    let tag = source.latest_tag().await?;
    // `strip_prefix`, not `trim_start_matches`: the latter strips repeated leading `v`s, which
    // would silently accept a malformed tag `build.rs`'s own `glass_version()` would not.
    let latest = version::Version::parse_released(tag.strip_prefix('v').unwrap_or(&tag))
        .ok_or_else(|| anyhow::anyhow!("the latest release tag {tag:?} is not a version"))?;
    report.latest = Some(latest.to_string());
    report.update_available = current_version.as_ref().is_some_and(|c| latest > *c);

    if opts.check {
        return Ok(report.finished(Outcome::Checked));
    }

    // 4. Nothing to do.
    let current_version = current_version.expect("checked above for the apply path");
    if latest <= current_version {
        return Ok(report.finished(Outcome::UpToDate));
    }

    // 5. What we are about to fetch. Resolved before the staging step below so a refusal there
    //    can still print the URL to download by hand.
    let asset = release::asset_name(&tag).ok_or_else(|| anyhow::anyhow!("unsupported target"))?;
    let url = source.asset_url(&tag, &asset);
    report.asset = Some(asset.clone());
    report.url = Some(url.clone());

    // 6. Stage: create the temp file the download will land in, rather than inspect the
    //    directory's permissions — opening it is the operation the download will actually
    //    perform, so a permission bit the kernel may not enforce is never consulted. Not the
    //    same moment, though: `download_inner` creates the file again after the consent prompt,
    //    so this rules out the permission mismatch, not every later failure.
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    let temp = dir.join(format!(".glass-mcp.update-{:016x}", rand::random::<u64>()));
    let mut open = std::fs::OpenOptions::new();
    open.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        // 0755 at creation, not left for `swap` (step 9) to set. The smoke check (8c) EXECUTES
        // this file, and `swap` runs after it — a 0644 temp makes every real update fail at the
        // gate that exists to prove the binary runs, with `execve` returning EACCES.
        open.mode(0o755);
    }
    if let Err(e) = open.open(&temp) {
        return Ok(report.refused(Refusal::CannotStage {
            dir: dir.to_path_buf(),
            why: e.to_string(),
        }));
    }
    // From here on every exit tries to remove `temp`: a refusal that leaves a stray half-download
    // beside the binary is not the no-op it claims to be. Best-effort, deliberately — surfacing a
    // failed unlink would replace the real reason for the exit (a bad checksum, a refused
    // attestation) with a secondary filesystem complaint. The one exit that deliberately does NOT
    // remove `temp` is a failed `swap` at step 9 — see the comment there.
    let discard = |report: Report, why: Refusal| -> anyhow::Result<Report> {
        let _ = std::fs::remove_file(&temp);
        Ok(report.refused(why))
    };
    let abort = |e: anyhow::Error| -> anyhow::Error {
        let _ = std::fs::remove_file(&temp);
        e
    };

    // 7. Consent. A non-tty without --yes refuses rather than assuming it.
    if !opts.yes && !opts.interactive {
        return discard(report, Refusal::NeedsConsent);
    }
    if !opts.yes
        && !confirm(&current_version.to_string(), &latest.to_string(), &url).map_err(abort)?
    {
        return discard(report, Refusal::NeedsConsent);
    }

    // Sweep any binary a previous update left displaced (Windows only, best-effort) now that we
    // are committed to proceeding — not at step 6, where a refusal must not have already mutated
    // the filesystem.
    swap::sweep_old(exe);

    // 8a. Download, hashing in one pass.
    let got = source.download_to(&url, &temp).await.map_err(abort)?;
    let sidecar_text = source
        .fetch_text(&format!("{url}.sha256"))
        .await
        .map_err(abort)?;
    let expected = match verify::parse_sidecar(&sidecar_text, &asset) {
        Ok(h) => h,
        Err(e) => return discard(report, Refusal::BadSidecar(e)),
    };
    if expected != got {
        return discard(report, Refusal::ChecksumMismatch { expected, got });
    }

    // 8b. Provenance. Fail closed — see `verify::attest`.
    if opts.skip_attestation {
        report.attestation = AttestationStatus::Skipped;
    } else {
        match verify::attest(opts.attest_program, &temp) {
            Attestation::Verified => report.attestation = AttestationStatus::Verified,
            Attestation::Unavailable => report.attestation = AttestationStatus::Unavailable,
            Attestation::Failed(why) => {
                report.attestation = AttestationStatus::Failed;
                return discard(report, Refusal::AttestationFailed(why));
            }
        }
    }

    // 8c. Only now is the file executed.
    if opts.smoke
        && let Err(why) = verify::smoke_check(&temp, &latest.to_string())
    {
        return discard(report, Refusal::SmokeCheckFailed(why));
    }

    // 9. Swap. Not through `abort`: by this point `temp` has been downloaded, checksummed,
    //    attested and proved to run, and if the swap fails it is the only copy of the new binary
    //    on the machine. On Windows's double-failure path — the old binary moved aside and the
    //    restore also failing — recovery means putting a working binary back at `exe` without a
    //    second download. The error names where this one is; `swap`'s own message names the
    //    displaced old binary.
    swap::swap(&temp, exe).with_context(|| {
        format!(
            "the verified new binary is still at {} — move it into place by hand",
            for_display(&temp)
        )
    })?;
    Ok(report.finished(Outcome::Updated))
}

/// A path as a user should see it. `canonicalize` yields Windows extended-length paths
/// (`\\?\C:\…`), which every filesystem call accepts but Explorer rejects — and one caller is the
/// swap-failure message naming a binary the user has to move by hand.
///
/// Display only: the stripped string never goes back to the filesystem. Not `cfg(windows)`,
/// because it is string work and stays testable on a Linux dev box.
fn for_display(path: &Path) -> String {
    let s = path.display().to_string();
    // `\\?\UNC\server\share` is a UNC path; stripping the whole prefix would leave `UNC\…`.
    if let Some(rest) = s.strip_prefix(r"\\?\UNC\") {
        return format!(r"\\{rest}");
    }
    s.strip_prefix(r"\\?\").unwrap_or(&s).to_string()
}

/// Ask on the terminal. Only reached when stdin and stdout are both a tty.
fn confirm(current: &str, latest: &str, url: &str) -> anyhow::Result<bool> {
    use std::io::Write;
    println!("glass-mcp {current} → {latest}");
    println!("  {url}");
    print!("Replace this binary? [y/N] ");
    std::io::stdout().flush()?;
    let mut line = String::new();
    std::io::stdin().read_line(&mut line)?;
    let line = line.trim();
    Ok(line.eq_ignore_ascii_case("y") || line.eq_ignore_ascii_case("yes"))
}

/// Resolve what `run` needs from the process itself, then render. Split from [`run`] so the flow
/// is testable without a real install: everything impure lives here.
pub(crate) async fn run_cli(
    check: bool,
    yes: bool,
    skip_attestation: bool,
    json: bool,
    color: ColorChoice,
) -> anyhow::Result<()> {
    // `.context` rather than folding the io::Error in with `{e}`: the latter discards it as this
    // error's `source()` — see the comment on release.rs's `fetch_text`.
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("could not resolve this binary's path")?;
    let opts = Options::from_flags(check, yes, skip_attestation, json, color);
    let source = ReleaseSource::github();
    let mut report = or_error(
        run(opts.clone(), &source, crate::VERSION, &exe).await,
        crate::VERSION,
        &exe,
    );
    // A running `serve --http` keeps its own inode, so it goes on serving the OLD build until it
    // is restarted — which reads to a connected agent as "the update did nothing". On the report
    // rather than printed separately, so `--json` stays one parseable object.
    if matches!(report.outcome, Outcome::Updated) {
        report.running_server = crate::setup::fetch_health("127.0.0.1:7300").is_some();
    }
    print!("{}", render(&report, &opts));
    if matches!(report.outcome, Outcome::Refused(_) | Outcome::Error(_)) {
        std::process::exit(1);
    }
    Ok(())
}

/// Fold a failure out of [`run`] into a report, so every exit renders through the same path.
///
/// Without this a transport failure reached `main` as a bare anyhow message — under `--json`, no
/// JSON object at all. `{e:#}` rather than `{e}` so the whole source chain survives: release.rs
/// keeps the real cause (a redirect-policy refusal, a connect error) on `source()`.
fn or_error(result: anyhow::Result<Report>, current: &str, exe: &Path) -> Report {
    result.unwrap_or_else(|e| {
        Report::initial(current, exe).finished(Outcome::Error(format!("{e:#}")))
    })
}

/// Human text, or the JSON object under `--json`. Pure in `report` and `opts`, so the whole
/// output surface is unit-testable without running an update.
fn render(report: &Report, opts: &Options) -> String {
    if opts.json {
        return render_json(report);
    }
    let p = crate::color::palette(opts.color);
    let latest = report.latest.as_deref().unwrap_or("unknown");
    match &report.outcome {
        Outcome::Checked if report.update_available => format!(
            "glass-mcp {} → {} is available.\n{}\n",
            report.current,
            p.paint(p.bold, latest),
            if report.supported {
                "Run `glass-mcp update` to install it."
            } else {
                "This install is not replaced in place — see the setup guide for your platform."
            }
        ),
        // A from-source build's version is not comparable, so `update_available` is always
        // `false` here regardless of whether a newer release exists — printing "is the latest
        // release" (the arm below) would be a claim we cannot back.
        Outcome::Checked if !report.current_comparable => format!(
            "glass-mcp {} is a from-source build; the latest release is {}.\n",
            report.current, latest
        ),
        // This arm covers `latest <= current`, which is only true when they are equal unless the
        // running build is a prerelease newer than the latest release — "is the latest release"
        // would be a claim the code never established in that case, so say "up to date" instead.
        Outcome::Checked | Outcome::UpToDate => {
            format!("glass-mcp {} is up to date.\n", report.current)
        }
        Outcome::Updated => format!(
            "{} glass-mcp {} → {}\n{}{}",
            p.paint(p.ok, "updated"),
            report.current,
            latest,
            match report.attestation {
                AttestationStatus::Verified => "build provenance verified.\n".to_string(),
                AttestationStatus::Unavailable => format!(
                    "{}\n",
                    p.paint(
                        p.dim,
                        "build provenance not checked — install the GitHub CLI (`gh`) to verify it."
                    )
                ),
                AttestationStatus::Skipped => format!(
                    "{}\n",
                    p.paint(
                        p.warn,
                        "build provenance NOT verified (--skip-attestation)."
                    )
                ),
                // Unreachable: 8b sets one of the other three before an `Updated` can be
                // produced, and a Failed attestation refuses rather than updating.
                AttestationStatus::NotChecked | AttestationStatus::Failed => String::new(),
            },
            if report.running_server {
                "note: a glass server is running on 127.0.0.1:7300 and keeps the previous build \
                 until it is restarted.\n"
            } else {
                ""
            }
        ),
        Outcome::Refused(why) => format!(
            "{} {}\n",
            p.paint(p.fail, "cannot update:"),
            refusal_message(why, report)
        ),
        // A different prefix from a refusal: nothing was decided about this install.
        Outcome::Error(why) => format!("{} {why}\n", p.paint(p.fail, "update failed:")),
    }
}

/// One message per refusal: what happened, and what to do instead. Never suggests `sudo` —
/// an updater that acquires root on the user's behalf is a much larger thing than this command.
fn refusal_message(why: &Refusal, report: &Report) -> String {
    match why {
        Refusal::FromSource(v) => format!(
            "this glass-mcp ({v}) was built from source. Update it with \
             `git pull && cargo build --release` rather than replacing it with a release binary."
        ),
        Refusal::MacosBundle => "on macOS glass installs as GlassMcp.app, not a bare binary. \
             Download the latest .dmg — see docs/how-to/setup-macos.md."
            .to_string(),
        Refusal::UnsupportedTarget => "no release asset is published for this platform. \
             Build from source — see docs/how-to/build-from-source.md."
            .to_string(),
        // Reports the OS error, never a diagnosis of it — see `Refusal::CannotStage`.
        Refusal::CannotStage { dir, why } => format!(
            "could not create the temporary file to download into, in {}: {why}\n  \
             Download the new binary and move it into place yourself:\n  {}",
            for_display(dir),
            report.url.as_deref().unwrap_or("(url unresolved)")
        ),
        Refusal::NeedsConsent => "declined, or no way to ask. Pass --yes to update without a \
             prompt (required with --json, which never prompts)."
            .to_string(),
        Refusal::ChecksumMismatch { expected, got } => format!(
            "the downloaded asset does not match the checksum the release published.\n  \
             expected {expected}\n  got      {got}"
        ),
        Refusal::BadSidecar(verify::SidecarError::Malformed) => {
            "the release's checksum file is not a `sha256sum` line this can read.".to_string()
        }
        Refusal::BadSidecar(verify::SidecarError::WrongAsset(named)) => format!(
            "the release's checksum file is for a different asset: it names {named}, not {}.",
            report.asset.as_deref().unwrap_or("the asset downloaded")
        ),
        // `gh` exits non-zero for more than a bad artifact, and the text it printed is the only
        // thing that says which — so name the other causes rather than let the reader assume the
        // artifact is bad.
        Refusal::AttestationFailed(why) => format!(
            "build provenance could not be verified: {why}\n  \
             If GitHub is unreachable, or `gh` is installed but not signed in \
             (check with `gh auth status`), rather than the artifact being wrong, fix that and \
             retry — or pass --skip-attestation to accept the checksum alone."
        ),
        // "failed the run check", not "did not run": only one of `smoke_check`'s six failure
        // paths is a spawn failure. On the other five the binary ran — it hung, exited non-zero,
        // or printed the wrong version — so any lead-in claiming it did not run contradicts the
        // very error it introduces. Say what the gate concluded and let `why` say the rest.
        Refusal::SmokeCheckFailed(why) => format!(
            "the downloaded binary failed the run check: {why}\n  \
             Build variants for this platform are listed in docs/reference/platforms.md."
        ),
    }
}

/// The machine-readable form. `reason` appears only on a refusal or a failure.
fn render_json(report: &Report) -> String {
    let (action, reason) = match &report.outcome {
        Outcome::Checked | Outcome::UpToDate => ("checked", None),
        Outcome::Updated => ("updated", None),
        Outcome::Refused(why) => ("refused", Some(refusal_message(why, report))),
        Outcome::Error(why) => ("error", Some(why.clone())),
    };
    let mut obj = serde_json::json!({
        "action": action,
        "current": report.current,
        "latest": report.latest,
        "update_available": report.update_available,
        // Without this, `--check --json` on a from-source build reads as "you are up to date".
        "current_comparable": report.current_comparable,
        "supported": report.supported,
        "asset": report.asset,
        "url": report.url,
        "install_path": for_display(&report.install_path),
        "attestation": match report.attestation {
            AttestationStatus::NotChecked => "not_checked",
            AttestationStatus::Verified => "verified",
            AttestationStatus::Unavailable => "unavailable",
            AttestationStatus::Skipped => "skipped",
            AttestationStatus::Failed => "failed",
        },
        "running_server": report.running_server,
    });
    if let Some(reason) = reason {
        obj["reason"] = serde_json::Value::String(reason);
    }
    format!(
        "{}\n",
        serde_json::to_string_pretty(&obj).expect("a json object serializes")
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::update::testserver::FakeRelease;

    const BODY: &[u8] = b"pretend binary";
    /// sha256 of BODY. Recompute with `printf '%s' 'pretend binary' | sha256sum` if BODY changes.
    const BODY_SHA: &str = "9f05ef97cc90b003959b48cd01b637658368bdfc78fe54c1a061d5eff0c46104";

    /// A program name no host has on `PATH`, so `attest` reports `Unavailable` deterministically.
    /// Used instead of the real `gh` by every test that lets step 8b run: `gh attestation verify`
    /// makes a request to GitHub, and nothing in this suite touches the network.
    const ABSENT_GH: &str = "glass-mcp-test-no-such-attestation-tool";

    /// The apply path, non-interactive, with the two gates that need a real network or a real
    /// executable turned off. Each test that cares re-enables or re-disables what it is testing.
    fn opts() -> Options {
        Options {
            check: false,
            yes: true,
            skip_attestation: true,
            json: false,
            color: Default::default(),
            interactive: false,
            smoke: true,
            attest_program: ABSENT_GH,
        }
    }

    /// A fake install: a directory holding a "binary" we are allowed to replace.
    fn install(dir: &std::path::Path, bytes: &[u8]) -> std::path::PathBuf {
        let exe = dir.join(if cfg!(windows) {
            "glass-mcp.exe"
        } else {
            "glass-mcp"
        });
        std::fs::write(&exe, bytes).unwrap();
        exe
    }

    fn sidecar(asset: &str) -> String {
        format!("{BODY_SHA}  {asset}\n")
    }

    /// Does the apply path exist on this target at all?
    ///
    /// CI's macOS job runs `cargo test --workspace --lib`, so this module executes there — and on
    /// macOS `run` refuses with `MacosBundle` by design, while an unsupported target has no asset
    /// name to build. The macOS behavior has its own test at the end.
    fn apply_path_exists() -> bool {
        !cfg!(target_os = "macos") && release::asset_suffix().is_some()
    }

    #[tokio::test]
    async fn a_from_source_build_refuses_before_any_network_request() {
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        // A base that would fail loudly if it were ever contacted.
        let src = ReleaseSource::with_base("http://127.0.0.1:1");
        let out = run(opts(), &src, "1.3.0-5-g563feea", &exe)
            .await
            .unwrap()
            .outcome;
        assert!(
            matches!(out, Outcome::Refused(Refusal::FromSource(_))),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    #[tokio::test]
    async fn being_on_the_latest_release_is_a_no_op() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.3.0").expect("supported target");
        let server = FakeRelease::start("v1.3.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let out = run(opts(), &src, "1.3.0", &exe).await.unwrap().outcome;
        assert!(matches!(out, Outcome::UpToDate), "{out:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    /// The apply path's `latest <= current` decision (glass#447) in its non-equal shape: a
    /// running build that is a prerelease *newer* than the latest release. The render test pins
    /// the string; this pins the classification itself — the binary must be reported up to date
    /// and left untouched, not downloaded (the `1.3.0` server below would 404 no asset for
    /// `1.4.0-rc1`, so a misclassified "update" would fail the run, not just the assert).
    #[tokio::test]
    async fn a_prerelease_running_above_the_latest_release_is_up_to_date() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.3.0").expect("supported target");
        let server = FakeRelease::start("v1.3.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let out = run(opts(), &src, "1.4.0-rc1", &exe).await.unwrap().outcome;
        assert!(matches!(out, Outcome::UpToDate), "{out:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    /// The whole flow, end to end, against the real code path.
    #[tokio::test]
    async fn a_newer_release_replaces_the_binary() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.4.0").expect("supported target");
        let server = FakeRelease::start("v1.4.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let mut o = opts();
        o.smoke = false; // the served bytes are not a runnable binary
        let out = run(o, &src, "1.3.0", &exe).await.unwrap().outcome;
        assert!(matches!(out, Outcome::Updated), "{out:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), BODY);
    }

    /// The apply path with the smoke gate ON, against a body that is actually runnable.
    ///
    /// Every other apply-path test sets `smoke: false`, so this is the only one that exercises
    /// step 8c — and 8c executes the temp file, which means it also pins the file's MODE. With the
    /// temp created 0644 this fails with EACCES, which is exactly what shipped before this test.
    #[cfg(unix)]
    #[tokio::test]
    async fn the_smoke_gate_runs_the_downloaded_binary() {
        if !apply_path_exists() {
            return;
        }
        const SCRIPT: &[u8] = b"#!/bin/sh\necho glass-mcp 1.4.0\n";
        const SCRIPT_SHA: &str = "4605fe04920b0a38e4b11a2b08453755ecae4319cafb5069a19253d0a1ce0cc2";
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.4.0").expect("supported target");
        let server = FakeRelease::start(
            "v1.4.0",
            &asset,
            SCRIPT,
            &format!("{SCRIPT_SHA}  {asset}\n"),
        );
        let src = ReleaseSource::with_base(server.base());
        // opts() already has smoke: true — that is the point of this test.
        //
        // Retried only on ETXTBSY, a window that belongs to this multi-threaded test binary
        // rather than to the updater — see the note on `verify`'s `is_etxtbsy`. Any other
        // outcome breaks out on the first attempt.
        let mut attempts = 0;
        let out = loop {
            let out = run(opts(), &src, "1.3.0", &exe).await.unwrap().outcome;
            let busy = matches!(
                &out,
                Outcome::Refused(Refusal::SmokeCheckFailed(why)) if why.contains("os error 26")
            );
            if !busy {
                break out;
            }
            attempts += 1;
            assert!(
                attempts < 20,
                "the smoke gate kept hitting ETXTBSY: {out:?}"
            );
            std::thread::sleep(std::time::Duration::from_millis(20));
        };
        assert!(matches!(out, Outcome::Updated), "{out:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), SCRIPT);
    }

    /// Step 8b, actually executed.
    ///
    /// Every other apply-path test sets `skip_attestation: true`, so the whole `else` arm — the
    /// three-way match on `verify::attest` — never ran in any test: swap its `Verified` and
    /// `Unavailable` arms and the suite stayed green. This drives it with `gh` pointed at a
    /// program that does not exist, which is the one `attest` outcome reachable without making a
    /// request to GitHub.
    #[tokio::test]
    async fn a_missing_gh_is_recorded_rather_than_treated_as_verified() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.4.0").expect("supported target");
        let server = FakeRelease::start("v1.4.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let mut o = opts();
        o.skip_attestation = false; // the point of this test
        o.smoke = false; // the served bytes are not a runnable binary
        let report = run(o, &src, "1.3.0", &exe).await.unwrap();
        assert!(matches!(report.outcome, Outcome::Updated), "{report:?}");
        assert_eq!(report.attestation, AttestationStatus::Unavailable);
        assert_eq!(std::fs::read(&exe).unwrap(), BODY);
    }

    /// The assertion that matters is the second one: "it errored" says nothing about what it
    /// left behind.
    #[tokio::test]
    async fn a_bad_checksum_refuses_and_leaves_the_binary_untouched() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.4.0").expect("supported target");
        let wrong = format!("{}  {asset}\n", "0".repeat(64));
        let server = FakeRelease::start("v1.4.0", &asset, BODY, &wrong);
        let src = ReleaseSource::with_base(server.base());
        let mut o = opts();
        o.smoke = false;
        let out = run(o, &src, "1.3.0", &exe).await.unwrap().outcome;
        assert!(
            matches!(out, Outcome::Refused(Refusal::ChecksumMismatch { .. })),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        let strays: Vec<_> = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .map(|e| e.file_name().to_string_lossy().into_owned())
            .filter(|n| n.starts_with(".glass-mcp.update-"))
            .collect();
        assert!(
            strays.is_empty(),
            "a failed update must clean up: {strays:?}"
        );
    }

    #[tokio::test]
    async fn a_non_tty_without_yes_refuses_rather_than_assuming_consent() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.4.0").expect("supported target");
        let server = FakeRelease::start("v1.4.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let mut o = opts();
        o.yes = false;
        o.interactive = false;
        let out = run(o, &src, "1.3.0", &exe).await.unwrap().outcome;
        assert!(
            matches!(out, Outcome::Refused(Refusal::NeedsConsent)),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    /// Unix-only: this needs directory permissions to actually deny a write, which Windows ACLs
    /// do not express the same way. It also self-checks that the denial took effect — running as
    /// root ignores mode 0o555 entirely, and without the probe this test would pass vacuously in
    /// a root container by refusing for no reason at all.
    #[cfg(unix)]
    #[tokio::test]
    async fn an_unwritable_directory_refuses_before_downloading() {
        if !apply_path_exists() {
            return;
        }
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o555)).unwrap();

        let denied = std::fs::File::create(dir.path().join(".probe")).is_err();
        if !denied {
            std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
            eprintln!("skipping: this user can write to a 0o555 directory (root?)");
            return;
        }

        let asset = release::asset_name("v1.4.0").expect("supported target");
        let server = FakeRelease::start("v1.4.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let out = run(opts(), &src, "1.3.0", &exe).await.unwrap().outcome;

        std::fs::set_permissions(dir.path(), std::fs::Permissions::from_mode(0o755)).unwrap();
        // Bind the fields rather than `{ .. }`: the variant alone says nothing about whether the
        // `io::Error` was actually captured. Replacing the capture with `String::new()` leaves a
        // `matches!` on the variant green, and the refusal then prints a trailing colon with
        // nothing after it.
        let Outcome::Refused(Refusal::CannotStage { dir: named, why }) = &out else {
            panic!("expected a staging refusal, got {out:?}");
        };
        assert_eq!(named, dir.path(), "the refusal must name the directory");
        assert!(
            !why.is_empty(),
            "the OS error must be carried into the refusal, not discarded"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    /// `--check` reports on every platform and refuses nothing, including on a from-source build —
    /// so, unlike the apply-path tests, this one carries no `apply_path_exists()` guard and no
    /// `release::asset_name(...)` (which would return `None` and skip the test on macOS, exactly
    /// the platform this contract most needs covering). The asset name is a literal because
    /// `--check` resolves only the tag, never an asset.
    #[tokio::test]
    async fn check_reports_on_a_from_source_build() {
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = "glass-mcp-v1.4.0-x86_64-linux-gnu";
        let server = FakeRelease::start("v1.4.0", asset, BODY, &sidecar(asset));
        let src = ReleaseSource::with_base(server.base());
        let mut o = opts();
        o.check = true;
        let report = run(o, &src, "1.3.0-5-g563feea", &exe).await.unwrap();
        assert!(
            matches!(report.outcome, Outcome::Checked),
            "{:?}",
            report.outcome
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
        // The outcome alone does not distinguish "up to date" from "we don't know" — assert the
        // actual rendered claim, not just that nothing was refused.
        let human = render(&report, &plain(false));
        assert!(
            human.contains("from-source build"),
            "must not claim to be the latest release: {human}"
        );
        assert!(
            human.contains("1.4.0"),
            "must name the real latest release: {human}"
        );
    }

    fn report_fixture() -> Report {
        Report {
            outcome: Outcome::Checked,
            current: "1.3.0".into(),
            latest: Some("1.4.0".into()),
            update_available: true,
            asset: Some("glass-mcp-v1.4.0-x86_64-linux-gnu".into()),
            url: Some("https://example.invalid/asset".into()),
            install_path: std::path::PathBuf::from("/opt/bin/glass-mcp"),
            attestation: AttestationStatus::Verified,
            supported: true,
            current_comparable: true,
            running_server: false,
        }
    }

    /// Color off, so these assert on the text rather than on escape sequences.
    fn plain(json: bool) -> Options {
        let mut o = opts();
        o.json = json;
        o.color = crate::color::ColorChoice::Never;
        o
    }

    #[test]
    fn the_json_report_carries_every_documented_field() {
        let out = render(&report_fixture(), &plain(true));
        let v: serde_json::Value = serde_json::from_str(&out).expect("valid json");
        assert_eq!(v["action"], "checked");
        assert_eq!(v["current"], "1.3.0");
        assert_eq!(v["latest"], "1.4.0");
        assert_eq!(v["update_available"], true);
        assert_eq!(v["current_comparable"], true);
        assert_eq!(v["supported"], true);
        assert_eq!(v["attestation"], "verified");
        assert_eq!(v["install_path"], "/opt/bin/glass-mcp");
        assert_eq!(v["running_server"], false);
        assert!(v.get("reason").is_none(), "reason is refusal-only");
    }

    /// `update_available: false` has two quite different causes, and the JSON has to separate them
    /// the way the human renderer does: `cli.md` tells scripts to branch on `update_available`, so
    /// a from-source build that emitted only `update_available: false` would read to every one of
    /// them as "you are up to date".
    #[test]
    fn the_json_report_separates_a_from_source_build_from_being_up_to_date() {
        let mut r = report_fixture();
        r.current = "1.3.0-5-g563feea".into();
        r.current_comparable = false;
        r.update_available = false;
        let v: serde_json::Value = serde_json::from_str(&render(&r, &plain(true))).unwrap();
        assert_eq!(v["update_available"], false);
        assert_eq!(
            v["current_comparable"], false,
            "a from-source build's version is not comparable, and the object must say so"
        );
    }

    /// `--json`'s contract is one parseable object per run, which a transport failure used to
    /// break (see `or_error`). The fake server here answers `/releases/latest` with a redirect to
    /// an empty tag, so `run` fails for a real reason without any request leaving the loopback
    /// interface.
    #[tokio::test]
    async fn a_failure_to_resolve_the_release_still_renders_one_json_object() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let server = FakeRelease::start("", "asset", BODY, "");
        let src = ReleaseSource::with_base(server.base());
        let result = run(opts(), &src, "1.3.0", &exe).await;
        assert!(result.is_err(), "the fixture must actually fail `run`");

        let report = or_error(result, "1.3.0", &exe);
        let v: serde_json::Value = serde_json::from_str(&render(&report, &plain(true)))
            .expect("a failed run still emits one json object");
        assert_eq!(v["action"], "error");
        assert_eq!(v["current"], "1.3.0");
        assert!(
            v["reason"].as_str().is_some_and(|s| !s.is_empty()),
            "the failure has to say what went wrong: {v}"
        );
        // Not a refusal: nothing was decided about this install.
        let human = render(&report, &plain(false));
        assert!(human.contains("update failed:"), "{human}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    /// `Options::from_flags` is the only place the running binary's `Options` are built, and the
    /// only place `smoke` is ever set to `true` — flip it to `false` and every real update skips
    /// the gate that proves the downloaded binary runs, with the whole suite still green, because
    /// each test constructs `Options` literally. Same for `attest_program`: the tests point it at
    /// a program that does not exist, so nothing else pins that the binary uses the real `gh`.
    ///
    /// `interactive` is asserted only for `json: true`, where `!json` makes it `false` whatever
    /// the terminal is. Its value with `--json` off depends on whether the test process has a tty
    /// on both stdin and stdout, which is not something a test can pin.
    #[test]
    fn from_flags_passes_the_cli_flags_through_and_always_verifies() {
        let o = Options::from_flags(true, false, true, false, ColorChoice::Never);
        assert!(o.check);
        assert!(!o.yes);
        assert!(o.skip_attestation);
        assert!(!o.json);
        assert_eq!(o.color, ColorChoice::Never);
        assert!(
            o.smoke,
            "the apply path must always execute what it fetched"
        );
        assert_eq!(o.attest_program, verify::GH);

        // Every flag the other way round, so a field wired to a constant is caught whichever
        // constant it was wired to.
        let o = Options::from_flags(false, true, false, true, ColorChoice::Always);
        assert!(!o.check);
        assert!(o.yes);
        assert!(!o.skip_attestation);
        assert!(o.json);
        assert_eq!(o.color, ColorChoice::Always);
        assert!(o.smoke);
        assert_eq!(o.attest_program, verify::GH);
        assert!(!o.interactive, "--json must never prompt");
    }

    /// Each `AttestationStatus` an `Updated` report can carry renders as its own claim.
    ///
    /// No test rendered `Outcome::Updated` at all before this, so the three arms were
    /// interchangeable: swapping `Verified` and `Skipped` would have made `--skip-attestation`
    /// print "build provenance verified." with nothing to catch it.
    #[test]
    fn an_updated_report_states_what_was_done_about_provenance() {
        let rendered = |status: AttestationStatus| {
            let mut r = report_fixture();
            r.outcome = Outcome::Updated;
            r.attestation = status;
            render(&r, &plain(false))
        };

        let verified = rendered(AttestationStatus::Verified);
        assert!(
            verified.contains("build provenance verified."),
            "{verified}"
        );

        let unavailable = rendered(AttestationStatus::Unavailable);
        assert!(unavailable.contains("not checked"), "{unavailable}");
        assert!(
            unavailable.contains("gh"),
            "it must say what to install: {unavailable}"
        );
        assert!(
            !unavailable.contains("provenance verified."),
            "a missing gh must never read as a verification: {unavailable}"
        );

        let skipped = rendered(AttestationStatus::Skipped);
        assert!(skipped.contains("--skip-attestation"), "{skipped}");
        assert!(
            !skipped.contains("provenance verified."),
            "an explicit skip must never read as a verification: {skipped}"
        );

        // The two states 8b cannot leave behind on an update. They print nothing extra rather
        // than a claim, and the "updated" line itself still appears.
        for unreachable in [AttestationStatus::NotChecked, AttestationStatus::Failed] {
            let out = rendered(unreachable);
            assert!(out.contains("updated"), "{out}");
            assert!(!out.contains("provenance"), "{out}");
        }
    }

    /// `canonicalize` hands back `\\?\C:\…` on Windows, and Explorer will not take it — which
    /// matters most in the swap-failure message, whose whole job is a path the user can act on.
    #[test]
    fn display_paths_drop_the_windows_extended_length_prefix() {
        use std::path::PathBuf;
        assert_eq!(
            for_display(&PathBuf::from(r"\\?\C:\Users\mpd\glass-mcp.exe")),
            r"C:\Users\mpd\glass-mcp.exe"
        );
        // A UNC path must come back as a UNC path, not as `UNC\…`.
        assert_eq!(
            for_display(&PathBuf::from(r"\\?\UNC\server\share\glass-mcp.exe")),
            r"\\server\share\glass-mcp.exe"
        );
        // No prefix: unchanged. The Unix case is the only shape this sees on the dev box.
        assert_eq!(
            for_display(&PathBuf::from(r"C:\Users\mpd\glass-mcp.exe")),
            r"C:\Users\mpd\glass-mcp.exe"
        );
        assert_eq!(
            for_display(&PathBuf::from("/opt/bin/glass-mcp")),
            "/opt/bin/glass-mcp"
        );
    }

    /// The two `BadSidecar` messages describe genuinely different failures — a file this cannot
    /// parse, and a file that parsed fine but names another asset — and nothing pinned which
    /// message went with which. Swapping them puts "could not be read" on a sidecar that read
    /// perfectly.
    #[test]
    fn the_two_sidecar_failures_are_not_described_as_each_other() {
        let rendered = |e: verify::SidecarError| {
            let mut r = report_fixture();
            r.outcome = Outcome::Refused(Refusal::BadSidecar(e));
            render(&r, &plain(false))
        };

        let malformed = rendered(verify::SidecarError::Malformed);
        assert!(malformed.contains("not a `sha256sum` line"), "{malformed}");
        assert!(
            !malformed.contains("different asset"),
            "an unparseable sidecar is not a wrong-asset sidecar: {malformed}"
        );

        let wrong = rendered(verify::SidecarError::WrongAsset("some-other-asset".into()));
        assert!(wrong.contains("different asset"), "{wrong}");
        assert!(
            wrong.contains("some-other-asset"),
            "it must name what the sidecar claims: {wrong}"
        );
        assert!(
            wrong.contains("glass-mcp-v1.4.0-x86_64-linux-gnu"),
            "and what was actually downloaded: {wrong}"
        );
        assert!(
            !wrong.contains("not a `sha256sum` line"),
            "a sidecar that parsed must not be reported as unreadable: {wrong}"
        );
    }

    /// The arm that renders "up to date" covers `latest <= current` — true for an equal build,
    /// and true for a prerelease running above the latest release. "is the latest release" would
    /// be false in the latter case, so neither path may make that claim. One render test per path
    /// pins it (glass#447).
    #[test]
    fn up_to_date_does_not_claim_to_be_the_latest_release() {
        // current == latest: the ordinary up-to-date case.
        let mut r = report_fixture();
        r.outcome = Outcome::UpToDate;
        r.current = "1.4.0".into();
        r.latest = Some("1.4.0".into());
        r.update_available = false;
        let human = render(&r, &plain(false));
        assert!(human.contains("is up to date"), "the equal case: {human}");
        assert!(
            !human.contains("is the latest release"),
            "must not claim to be the latest release: {human}"
        );

        // current newer than latest (a prerelease build, the shape glass#447 observed).
        let mut r = report_fixture();
        r.outcome = Outcome::Checked;
        r.current = "1.4.0-rc1".into();
        r.latest = Some("1.3.0".into());
        r.update_available = false;
        let human = render(&r, &plain(false));
        assert!(
            human.contains("is up to date"),
            "the current-newer case: {human}"
        );
        assert!(
            !human.contains("is the latest release"),
            "a prerelease above the latest release is not the latest release: {human}"
        );
    }

    #[test]
    fn a_refusal_names_the_condition_and_never_suggests_sudo() {
        let mut r = report_fixture();
        r.outcome = Outcome::Refused(Refusal::CannotStage {
            dir: "/usr/local/bin".into(),
            why: "No space left on device (os error 28)".into(),
        });

        let human = render(&r, &plain(false));
        assert!(human.contains("/usr/local/bin"), "{human}");
        // The OS error, not a diagnosis of it: a message that named permissions would be wrong
        // for exactly the case this fixture uses.
        assert!(
            human.contains("No space left on device"),
            "the OS error must survive into the message: {human}"
        );
        assert!(
            !human.to_lowercase().contains("not writable")
                && !human.to_lowercase().contains("permission"),
            "must not diagnose a cause it did not establish: {human}"
        );
        assert!(
            human.contains("https://example.invalid/asset"),
            "the manual URL: {human}"
        );
        assert!(
            !human.to_lowercase().contains("sudo"),
            "must never suggest sudo: {human}"
        );

        let v: serde_json::Value = serde_json::from_str(&render(&r, &plain(true))).unwrap();
        assert_eq!(v["action"], "refused");
        assert!(
            v["reason"]
                .as_str()
                .is_some_and(|s| s.contains("/usr/local/bin"))
        );
    }

    /// The macOS behavior the guard above skips: refusing is the feature, not a gap. Asserted on
    /// a released version so the from-source refusal cannot be what produces the verdict.
    #[cfg(target_os = "macos")]
    #[tokio::test]
    async fn the_apply_path_refuses_a_bundle_install() {
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        // A base that would fail loudly if the flow ever reached the network — it must refuse first.
        let src = ReleaseSource::with_base("http://127.0.0.1:1");
        let out = run(opts(), &src, "1.3.0", &exe).await.unwrap().outcome;
        assert!(
            matches!(out, Outcome::Refused(Refusal::MacosBundle)),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }
}
