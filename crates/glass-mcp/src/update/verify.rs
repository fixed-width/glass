//! The three gates a downloaded asset passes before it is allowed to replace this binary.
//!
//! They run in this order and the order is the point: the checksum proves the bytes are the ones
//! the release published, the attestation proves the release built them, and only then is the
//! file executed for the smoke check. Nothing unverified is ever run.

use std::path::Path;
use std::process::Command;
use std::time::Duration;

/// How long the downloaded binary gets to print its version before the check gives up. Generous
/// for a `--version` that only reads a compiled-in constant, bounded so a binary that hangs on
/// startup fails the gate rather than the process.
const SMOKE_TIMEOUT: Duration = Duration::from_secs(30);

#[derive(Debug, PartialEq, Eq)]
pub(crate) enum SidecarError {
    /// No `<hex>  <name>` line at all.
    Malformed,
    /// A well-formed line naming a different file — the wrong asset was fetched.
    WrongAsset(String),
}

/// What `gh attestation verify` had to say.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum Attestation {
    Verified,
    /// `gh` is not on PATH. Reported, never silently skipped.
    Unavailable,
    /// `gh` ran and did not succeed. This is a refusal, not a warning — see `attest`.
    Failed(String),
}

/// Pull the expected digest out of a `sha256sum` sidecar, checking it names the asset we fetched.
///
/// `sha256sum` writes `<hex>  <name>` in text mode and `<hex> *<name>` in binary mode; the
/// release workflow produces the former, but both are accepted because both are what the tool
/// emits and neither is ambiguous.
pub(crate) fn parse_sidecar(text: &str, asset: &str) -> Result<String, SidecarError> {
    let line = text
        .lines()
        .find(|l| !l.trim().is_empty())
        .ok_or(SidecarError::Malformed)?;
    let (hexpart, name) = line
        .split_once(char::is_whitespace)
        .ok_or(SidecarError::Malformed)?;
    let name = name.trim_start().trim_start_matches('*').trim();
    if hexpart.len() != 64 || !hexpart.bytes().all(|b| b.is_ascii_hexdigit()) {
        return Err(SidecarError::Malformed);
    }
    if name != asset {
        return Err(SidecarError::WrongAsset(name.to_string()));
    }
    Ok(hexpart.to_ascii_lowercase())
}

/// The GitHub CLI, as the binary invokes it. Named here and passed in as an argument rather than
/// written as a literal inside [`attest`], so a test can point the same code at a program that is
/// guaranteed absent (or guaranteed to fail) instead of depending on whether the host happens to
/// have `gh` installed — and, more importantly, without any test ever running the real
/// `gh attestation verify`, which queries GitHub over the network.
pub(crate) const GH: &str = "gh";

/// Verify the artifact's build provenance with the GitHub CLI, when it is available.
///
/// Any non-success is [`Attestation::Failed`], and the caller treats that as a refusal. That is
/// deliberate: `gh attestation verify` queries GitHub's attestations API, so a GitHub outage makes
/// it fail for a reason unrelated to the artifact — and telling "bad signature" apart from
/// "couldn't reach GitHub" by scraping another tool's stderr is exactly the brittleness worth not
/// building. Fail closed; `--skip-attestation` is the conscious override.
pub(crate) fn attest(program: &str, path: &Path) -> Attestation {
    let out = Command::new(program)
        .args(["attestation", "verify"])
        .arg(path)
        .args(["--repo", "fixed-width/glass"])
        .output();
    match out {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Attestation::Unavailable,
        Err(e) => Attestation::Failed(format!("could not run {program}: {e}")),
        Ok(o) if o.status.success() => Attestation::Verified,
        Ok(o) => {
            // `gh` normally explains itself on stderr, but it is not required to: a tool that
            // exits non-zero silently would otherwise produce a refusal whose reason is the empty
            // string. Fall back to the one fact we always have.
            let stderr = String::from_utf8_lossy(&o.stderr).trim().to_string();
            Attestation::Failed(if stderr.is_empty() {
                format!("{program} exited {} without explaining why", o.status)
            } else {
                stderr
            })
        }
    }
}

/// Does this `--version` output report exactly `expect`?
///
/// Whitespace-separated exact match on a token, not a prefix test: a prefix would accept `1.4.0`
/// where `1.4.0-rc1` was expected, and the other way round.
pub(crate) fn version_matches(stdout: &str, expect: &str) -> bool {
    stdout.split_whitespace().any(|tok| tok == expect)
}

/// Run the verified binary and require it to report `expect`.
///
/// A checksum proves the right *file* arrived; only executing it proves it runs *here*. A
/// too-old glibc or a wrong-architecture asset passes every hash check and then fails on first
/// use, and this is the gate that catches that before the swap rather than after.
pub(crate) fn smoke_check(path: &Path, expect: &str) -> Result<(), String> {
    let mut child = Command::new(path)
        .arg("--version")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .spawn()
        .map_err(|e| format!("could not run the downloaded binary: {e}"))?;

    let deadline = std::time::Instant::now() + SMOKE_TIMEOUT;
    loop {
        match child.try_wait() {
            Ok(Some(_)) => break,
            Ok(None) if std::time::Instant::now() >= deadline => {
                let _ = child.kill();
                let _ = child.wait();
                return Err(format!(
                    "the downloaded binary did not print its version within {}s",
                    SMOKE_TIMEOUT.as_secs()
                ));
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return Err(format!("could not wait on the downloaded binary: {e}")),
        }
    }
    let out = child
        .wait_with_output()
        .map_err(|e| format!("could not read the downloaded binary's output: {e}"))?;
    if !out.status.success() {
        return Err(format!(
            "the downloaded binary exited {} — stderr: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim()
        ));
    }
    let stdout = String::from_utf8_lossy(&out.stdout);
    if !version_matches(&stdout, expect) {
        return Err(format!(
            "the downloaded binary reported {:?}, expected {expect}",
            stdout.trim()
        ));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    const ASSET: &str = "glass-mcp-v1.4.0-x86_64-linux-gnu";
    const SHA: &str = "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08";

    #[test]
    fn a_sha256sum_line_parses() {
        let text = format!("{SHA}  {ASSET}\n");
        assert_eq!(parse_sidecar(&text, ASSET).unwrap(), SHA);
    }

    /// `sha256sum` emits two spaces for text mode and ` *` for binary mode. Both are real.
    #[test]
    fn binary_mode_marker_parses() {
        let text = format!("{SHA} *{ASSET}\n");
        assert_eq!(parse_sidecar(&text, ASSET).unwrap(), SHA);
    }

    #[test]
    fn uppercase_hex_is_normalized() {
        let text = format!("{}  {ASSET}\n", SHA.to_uppercase());
        assert_eq!(parse_sidecar(&text, ASSET).unwrap(), SHA);
    }

    /// A sidecar naming a different asset means the wrong file was fetched, which is exactly the
    /// mix-up a checksum is supposed to catch — so it must not be accepted just because the hex
    /// is well formed.
    #[test]
    fn a_sidecar_for_another_asset_is_rejected() {
        let text = format!("{SHA}  some-other-asset\n");
        assert!(matches!(
            parse_sidecar(&text, ASSET),
            Err(SidecarError::WrongAsset(_))
        ));
    }

    #[test]
    fn malformed_sidecars_are_rejected() {
        for text in [
            "",
            "\n",
            "not-hex-at-all  glass-mcp-v1.4.0-x86_64-linux-gnu\n",
            "9f86d081  glass-mcp-v1.4.0-x86_64-linux-gnu\n",
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08\n",
            // Right length (64), wrong alphabet — none of the above cases are the right length,
            // so this one is the only case that exercises the hex-digit check rather than the
            // length check.
            "gggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggggg  glass-mcp-v1.4.0-x86_64-linux-gnu\n",
            // 65 hex chars — the only over-length case here, and the only one that reaches the
            // length check with too MANY characters. Of the rest: the empty and blank inputs stop
            // at the "no non-empty line" guard, the bare 64-char digest has no filename and so
            // stops at `split_once`, and the others are too short or not hex. Every one of them is
            // therefore still rejected if `len() != 64` is weakened to `len() < 64` — this input
            // is what stops that weakening passing.
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08a  glass-mcp-v1.4.0-x86_64-linux-gnu\n",
        ] {
            assert!(
                parse_sidecar(text, ASSET).is_err(),
                "{text:?} must be rejected"
            );
        }
    }

    /// A tool that exits non-zero without printing anything. `String::from_utf8_lossy(&stderr)`
    /// is empty there, so the refusal built straight from it would read "build provenance could
    /// not be verified: " — a refusal with no reason at all. Unix-only, because `false` is what
    /// makes a silent non-zero exit available without shipping a fixture.
    ///
    /// The `Unavailable` arm has its own coverage through the whole flow, in `mod.rs`'s
    /// `a_missing_gh_is_recorded_rather_than_treated_as_verified`. It is not duplicated here on
    /// purpose: spawning a program that does *not* exist leaves the forked child holding every
    /// inherited descriptor until its failed `execve` returns, and this test binary has other
    /// threads writing files they are about to execute — see [`is_etxtbsy`] below.
    #[cfg(unix)]
    #[test]
    fn a_silent_non_zero_exit_still_says_something() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("artifact");
        std::fs::write(&path, b"bytes").unwrap();
        let Attestation::Failed(why) = attest("false", &path) else {
            panic!("a non-zero exit must be Failed");
        };
        assert!(!why.trim().is_empty(), "the reason must not be empty");
        assert!(why.contains("false"), "it must name the tool: {why}");
    }

    #[test]
    fn the_smoke_check_accepts_only_the_expected_version() {
        assert!(version_matches("1.4.0\n", "1.4.0"));
        assert!(version_matches("glass-mcp 1.4.0\n", "1.4.0"));
        assert!(!version_matches("1.3.0\n", "1.4.0"));
        assert!(!version_matches("", "1.4.0"));
        // A prefix match would accept 1.4.0 for an expected 1.4.0-rc1 and vice versa.
        assert!(!version_matches("1.4.0\n", "1.4.0-rc1"));
        assert!(!version_matches("1.4.0-rc1\n", "1.4.0"));
    }

    /// Is this failure the test harness rather than the code under test?
    ///
    /// Writing a file and then executing it inside one multi-threaded process is racy on Linux
    /// through no fault of either step: `cargo test` runs these tests in threads, a `fork` in any
    /// other thread inherits every descriptor open at that instant — including the write handle
    /// this thread still holds on the fixture — and the child keeps it until its own `execve`
    /// returns. `execve` on a file some process holds open for writing fails with ETXTBSY. The
    /// updater itself is one sequential flow that never forks while its download is open, so this
    /// window exists only inside the test binary — but it is wide enough to fail runs, which is
    /// why [`retry_etxtbsy`] exists.
    #[cfg(unix)]
    fn is_etxtbsy(message: &str) -> bool {
        message.contains("Text file busy") || message.contains("os error 26")
    }

    /// Call `f` until it stops reporting ETXTBSY. Bounded, and every other error is returned on
    /// the first attempt — this retries the harness race described above and nothing else, so a
    /// smoke check that genuinely fails still fails immediately.
    #[cfg(unix)]
    fn retry_etxtbsy(mut f: impl FnMut() -> Result<(), String>) -> Result<(), String> {
        for _ in 0..20 {
            match f() {
                Err(e) if is_etxtbsy(&e) => std::thread::sleep(Duration::from_millis(20)),
                other => return other,
            }
        }
        f()
    }

    /// The spawn half of the smoke check, against a real executable. Unix-only: the fixture is a
    /// shell script. The Windows spawn path is covered by the manual `lotus` run in the close-out.
    #[cfg(unix)]
    #[test]
    fn the_smoke_check_runs_the_binary_and_reads_its_version() {
        use std::os::unix::fs::PermissionsExt;
        let dir = tempfile::tempdir().expect("tempdir");
        let good = dir.path().join("good");
        std::fs::write(&good, "#!/bin/sh\necho 1.4.0\n").unwrap();
        std::fs::set_permissions(&good, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(retry_etxtbsy(|| smoke_check(&good, "1.4.0")).is_ok());

        let bad = dir.path().join("bad");
        std::fs::write(&bad, "#!/bin/sh\nexit 127\n").unwrap();
        std::fs::set_permissions(&bad, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(smoke_check(&bad, "1.4.0").is_err());

        // Exits 0 — a naive check that only looked at the exit status would pass this — but
        // prints the wrong version, which is exactly the mismatch the gate exists to catch.
        let wrong_version = dir.path().join("wrong-version");
        std::fs::write(&wrong_version, "#!/bin/sh\necho 1.3.0\n").unwrap();
        std::fs::set_permissions(&wrong_version, std::fs::Permissions::from_mode(0o755)).unwrap();
        assert!(smoke_check(&wrong_version, "1.4.0").is_err());
    }
}
