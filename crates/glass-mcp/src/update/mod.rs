//! `glass-mcp update`: fetch the latest release, verify it, and replace this binary.
//!
//! The step order below is a contract, not an implementation detail, and the tests assert it: the
//! from-source refusal happens before any network request, writability is proved before anything
//! is downloaded, and consent is taken before the download rather than before the swap.

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
/// `interactive` and `smoke` are separate from the CLI flags so the tests can drive the
/// non-interactive path and skip executing a fixture that is not a real binary; the binary always
/// sets them from the real terminal and always leaves `smoke` on.
#[derive(Debug, Clone)]
pub(crate) struct Options {
    pub(crate) check: bool,
    pub(crate) yes: bool,
    pub(crate) skip_attestation: bool,
    pub(crate) json: bool,
    pub(crate) color: ColorChoice,
    pub(crate) interactive: bool,
    pub(crate) smoke: bool,
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
            interactive: std::io::stdin().is_terminal() && std::io::stdout().is_terminal(),
            smoke: true,
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
    NotWritable(PathBuf),
    NeedsConsent,
    ChecksumMismatch {
        expected: String,
        got: String,
    },
    BadSidecar(String),
    AttestationFailed(String),
    SmokeCheckFailed(String),
}

#[derive(Debug)]
pub(crate) enum Outcome {
    Checked,
    UpToDate,
    Updated,
    Refused(Refusal),
}

/// What happened, and everything the renderer needs to say so. One struct rather than data hung
/// off the `Outcome` variants: `--json` emits the same field set for every outcome, so the
/// renderer should not have to reach into a different shape per variant.
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
}

/// The `attestation` field of the JSON output. `Skipped` is `--skip-attestation`; `Unavailable`
/// is `gh` not being installed. Keeping them distinct is the point — "we did not check" and "you
/// told us not to" are different things for a reader auditing an update.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum AttestationStatus {
    Verified,
    Unavailable,
    Skipped,
    Failed,
}

impl Report {
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
    let mut report = Report {
        outcome: Outcome::Checked,
        current: current.to_string(),
        latest: None,
        update_available: false,
        asset: None,
        url: None,
        install_path: exe.to_path_buf(),
        attestation: AttestationStatus::Skipped,
        supported: !cfg!(target_os = "macos") && release::asset_suffix().is_some(),
    };
    let current_version = version::Version::parse_released(current);

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
    let latest = version::Version::parse_released(tag.trim_start_matches('v'))
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

    // 5. What we are about to fetch. Resolved before the writability check so a refusal there can
    //    still print the URL to download by hand.
    let asset = release::asset_name(&tag).ok_or_else(|| anyhow::anyhow!("unsupported target"))?;
    let url = source.asset_url(&tag, &asset);
    report.asset = Some(asset.clone());
    report.url = Some(url.clone());

    // 6. Writability, proved by creating the temp file we are about to download into rather than
    //    by inspecting permissions — the same act, so there is no window between the check and
    //    the use, and no way for the two to disagree.
    let dir = exe.parent().unwrap_or_else(|| Path::new("."));
    swap::sweep_old(exe);
    let temp = dir.join(format!(".glass-mcp.update-{:016x}", rand::random::<u64>()));
    if std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(&temp)
        .is_err()
    {
        return Ok(report.refused(Refusal::NotWritable(dir.to_path_buf())));
    }
    // From here on every exit must remove `temp`: a refusal that leaves a stray half-download
    // beside the binary is not the no-op it claims to be, and a test asserts there are none.
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
    if !opts.yes && !confirm(&current_version.to_string(), &latest.to_string(), &url)? {
        return discard(report, Refusal::NeedsConsent);
    }

    // 8a. Download, hashing in one pass.
    let got = source.download_to(&url, &temp).await.map_err(abort)?;
    let sidecar_text = source
        .fetch_text(&format!("{url}.sha256"))
        .await
        .map_err(abort)?;
    let expected = match verify::parse_sidecar(&sidecar_text, &asset) {
        Ok(h) => h,
        Err(e) => return discard(report, Refusal::BadSidecar(format!("{e:?}"))),
    };
    if expected != got {
        return discard(report, Refusal::ChecksumMismatch { expected, got });
    }

    // 8b. Provenance. Fail closed — see `verify::attest`.
    if opts.skip_attestation {
        report.attestation = AttestationStatus::Skipped;
    } else {
        match verify::attest(&temp) {
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

    // 9. Swap.
    swap::swap(&temp, exe).map_err(abort)?;
    Ok(report.finished(Outcome::Updated))
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
    Ok(matches!(line.trim(), "y" | "Y" | "yes"))
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
    // `with_context` rather than folding the io::Error into the message with `{e}`: the latter
    // discards it as this error's `source()`, which is exactly the mistake release.rs's
    // `with_context` calls exist to avoid (see the comment on `fetch_text`).
    let exe = std::env::current_exe()
        .and_then(|p| p.canonicalize())
        .context("could not resolve this binary's path")?;
    let opts = Options::from_flags(check, yes, skip_attestation, json, color);
    let source = ReleaseSource::github();
    let report = run(opts.clone(), &source, crate::VERSION, &exe).await?;
    print!("{}", render(&report, &opts));
    if matches!(report.outcome, Outcome::Updated) {
        report_running_server();
    }
    if matches!(report.outcome, Outcome::Refused(_)) {
        std::process::exit(1);
    }
    Ok(())
}

/// A running `serve --http` keeps its own inode, so it goes on serving the OLD build until it is
/// restarted — which reads to a connected agent as "the update did nothing". Only says so when a
/// server actually answers, reusing the same loopback probe `status` uses.
fn report_running_server() {
    if crate::setup::fetch_health("127.0.0.1:7300").is_some() {
        println!(
            "note: a glass server is running on 127.0.0.1:7300 and keeps the previous build \
             until it is restarted."
        );
    }
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
        Outcome::Checked | Outcome::UpToDate => {
            format!("glass-mcp {} is the latest release.\n", report.current)
        }
        Outcome::Updated => format!(
            "{} glass-mcp {} → {}\n{}",
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
                // Unreachable: a failed attestation refuses rather than updating.
                AttestationStatus::Failed => String::new(),
            }
        ),
        Outcome::Refused(why) => format!(
            "{} {}\n",
            p.paint(p.fail, "cannot update:"),
            refusal_message(why, report)
        ),
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
        Refusal::NotWritable(dir) => format!(
            "{} is not writable by this user. Download the new binary and move it into place \
             yourself:\n  {}",
            dir.display(),
            report.url.as_deref().unwrap_or("(url unresolved)")
        ),
        Refusal::NeedsConsent => {
            "declined. Pass --yes to update without a prompt (there is no terminal to ask on)."
                .to_string()
        }
        Refusal::ChecksumMismatch { expected, got } => format!(
            "the downloaded asset does not match the checksum the release published.\n  \
             expected {expected}\n  got      {got}"
        ),
        Refusal::BadSidecar(e) => {
            format!("the release's checksum file could not be read ({e}).")
        }
        Refusal::AttestationFailed(why) => format!(
            "build provenance could not be verified: {why}\n  \
             If GitHub is unreachable rather than the artifact being wrong, retry later — or pass \
             --skip-attestation to accept the checksum alone."
        ),
        Refusal::SmokeCheckFailed(why) => {
            let mut msg = format!("the downloaded binary did not run: {why}");
            // Only suggest musl when the gnu asset is what was fetched — a too-old glibc is the
            // overwhelmingly likely cause there, and nonsense advice everywhere else.
            if report.asset.as_deref().is_some_and(|a| a.ends_with("-gnu")) {
                msg.push_str(
                    "\n  If that is a glibc error, use the musl build instead — \
                     see docs/reference/platforms.md.",
                );
            }
            msg
        }
    }
}

/// The machine-readable form. `reason` appears only on a refusal.
fn render_json(report: &Report) -> String {
    let (action, reason) = match &report.outcome {
        Outcome::Checked | Outcome::UpToDate => ("checked", None),
        Outcome::Updated => ("updated", None),
        Outcome::Refused(why) => ("refused", Some(refusal_message(why, report))),
    };
    let mut obj = serde_json::json!({
        "action": action,
        "current": report.current,
        "latest": report.latest,
        "update_available": report.update_available,
        "supported": report.supported,
        "asset": report.asset,
        "url": report.url,
        "install_path": report.install_path.display().to_string(),
        "attestation": match report.attestation {
            AttestationStatus::Verified => "verified",
            AttestationStatus::Unavailable => "unavailable",
            AttestationStatus::Skipped => "skipped",
            AttestationStatus::Failed => "failed",
        },
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
    /// name to build. Tests below that drive the apply path return early rather than assert the
    /// opposite of the intended behavior. The macOS behavior has its own test at the end.
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
        assert!(
            matches!(out, Outcome::Refused(Refusal::NotWritable(_))),
            "{out:?}"
        );
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
    }

    /// `--check` reports on every platform and refuses nothing, including on a from-source build.
    #[tokio::test]
    async fn check_reports_on_a_from_source_build() {
        if !apply_path_exists() {
            return;
        }
        let dir = tempfile::tempdir().unwrap();
        let exe = install(dir.path(), b"old");
        let asset = release::asset_name("v1.4.0").expect("supported target");
        let server = FakeRelease::start("v1.4.0", &asset, BODY, &sidecar(&asset));
        let src = ReleaseSource::with_base(server.base());
        let mut o = opts();
        o.check = true;
        let out = run(o, &src, "1.3.0-5-g563feea", &exe)
            .await
            .unwrap()
            .outcome;
        assert!(matches!(out, Outcome::Checked), "{out:?}");
        assert_eq!(std::fs::read(&exe).unwrap(), b"old");
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
        assert_eq!(v["supported"], true);
        assert_eq!(v["attestation"], "verified");
        assert_eq!(v["install_path"], "/opt/bin/glass-mcp");
        assert!(v.get("reason").is_none(), "reason is refusal-only");
    }

    #[test]
    fn a_refusal_names_the_condition_and_never_suggests_sudo() {
        let mut r = report_fixture();
        r.outcome = Outcome::Refused(Refusal::NotWritable("/usr/local/bin".into()));

        let human = render(&r, &plain(false));
        assert!(human.contains("/usr/local/bin"), "{human}");
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

    /// The glibc hint is only correct advice when the gnu asset is what failed to run.
    #[test]
    fn the_glibc_hint_appears_only_for_the_gnu_asset() {
        let mut r = report_fixture();
        r.outcome = Outcome::Refused(Refusal::SmokeCheckFailed("exited 1".into()));
        assert!(render(&r, &plain(false)).contains("musl build"));

        r.asset = Some("glass-mcp-v1.4.0-x86_64-linux-musl".into());
        assert!(!render(&r, &plain(false)).contains("musl build"));
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
