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

/// Verify the artifact's build provenance with the GitHub CLI, when it is available.
///
/// Any non-success is [`Attestation::Failed`], and the caller treats that as a refusal. That is
/// deliberate: `gh attestation verify` queries GitHub's attestations API, so a GitHub outage makes
/// it fail for a reason unrelated to the artifact — and telling "bad signature" apart from
/// "couldn't reach GitHub" by scraping another tool's stderr is exactly the brittleness worth not
/// building. Fail closed; `--skip-attestation` is the conscious override.
pub(crate) fn attest(path: &Path) -> Attestation {
    let out = Command::new("gh")
        .args(["attestation", "verify"])
        .arg(path)
        .args(["--repo", "fixed-width/glass"])
        .output();
    match out {
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => Attestation::Unavailable,
        Err(e) => Attestation::Failed(format!("could not run gh: {e}")),
        Ok(o) if o.status.success() => Attestation::Verified,
        Ok(o) => Attestation::Failed(String::from_utf8_lossy(&o.stderr).trim().to_string()),
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
            // 65 hex chars. Without an over-length case, weakening `len() != 64` to `len() < 64`
            // survives the whole suite — and this crate is mutation-gated in CI.
            "9f86d081884c7d659a2feaa0c55ad015a3bf4f1b2b0b822cd15d6c15b0f00a08a  glass-mcp-v1.4.0-x86_64-linux-gnu\n",
        ] {
            assert!(
                parse_sidecar(text, ASSET).is_err(),
                "{text:?} must be rejected"
            );
        }
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
        assert!(smoke_check(&good, "1.4.0").is_ok());

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
