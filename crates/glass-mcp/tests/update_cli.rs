//! `update` through the real binary. Deliberately narrow: any test that let the command resolve
//! a release would reach github.com, and nothing in this suite touches the network. What is
//! provable here is the refusal ORDER — that a from-source build is turned away before any
//! request is made — and CI builds glass-mcp from a git checkout, so `crate::VERSION` carries a
//! `git describe` suffix and this is ordinarily exactly that case. The one test that exercises it
//! skips a checkout sitting exactly on a tag, which would drive a real request.

use std::process::Command;

const GLASS: &str = env!("CARGO_BIN_EXE_glass-mcp");

/// Is this the version shape `build.rs` emits for a real release build?
///
/// Mirrors `update::version::Version::parse_released`, which an integration test cannot reach —
/// it is `pub(crate)`. Deliberately conservative: anything that might be a released version
/// causes a skip, because being wrong here means a real network request from the test suite.
fn looks_released(version: &str) -> bool {
    if version == "0.0.0" {
        return false;
    }
    let (core, pre) = match version.split_once('-') {
        Some((core, pre)) => (core, Some(pre)),
        None => (version, None),
    };
    let mut parts = core.split('.');
    let triple_ok = (0..3).all(|_| {
        parts
            .next()
            .is_some_and(|p| !p.is_empty() && p.bytes().all(|b| b.is_ascii_digit()))
    }) && parts.next().is_none();
    let pre_ok = match pre {
        None => true,
        Some(p) => p != "dirty" && p.starts_with(|c: char| c.is_ascii_alphabetic()),
    };
    triple_ok && pre_ok
}

/// The guard this whole suite depends on to never reach the network: a guard whose own logic is
/// untested is how the `.contains('-')` version of it shipped inert (`glass-mcp` itself has a
/// hyphen in its name, so that check was unconditionally true against the whole `--version` line).
#[test]
fn looks_released_matches_the_release_shape() {
    // Not released: every shape build.rs's local-build path can actually produce.
    assert!(
        !looks_released("1.3.0-18-g5579f99"),
        "a git-describe commit-count suffix"
    );
    assert!(!looks_released("1.3.0-dirty"), "a dirty working tree");
    assert!(!looks_released("0.0.0"), "the no-VCS fallback");
    assert!(
        !looks_released("5579f99"),
        "a bare SHA with no reachable tag"
    );
    // Released: what a CI tag build, or a checkout sitting exactly on a tag, reports.
    assert!(looks_released("1.3.0"), "a plain released version");
    assert!(looks_released("1.2.0-rc1"), "a real prerelease tag");
}

#[test]
fn update_refuses_a_from_source_build_without_touching_the_network() {
    // A checkout sitting exactly on a released tag reports a plain MAJOR.MINOR.PATCH (or a real
    // prerelease tag like `1.2.0-rc1`) — the one shape `update` treats as a real release, which
    // would send it resolving a real latest tag over the network. Skip rather than let that reach
    // github.com; every other shape (a commit count, `-dirty`, a bare SHA) is guaranteed to hit
    // the from-source refusal before any request.
    //
    // `--version` renders as `glass-mcp <version>`, so take the LAST whitespace-separated token
    // rather than matching against the whole line.
    let version_out = Command::new(GLASS)
        .arg("--version")
        .output()
        .expect("run glass-mcp");
    let stdout = String::from_utf8_lossy(&version_out.stdout);
    let version = stdout.split_whitespace().last().unwrap_or_default();
    if looks_released(version) {
        eprintln!(
            "skipping: this build reports a released version ({version}), so `update` would \
             resolve a real release — see looks_released"
        );
        return;
    }

    let out = Command::new(GLASS)
        .arg("update")
        .output()
        .expect("run glass-mcp");
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    let all = format!("{stdout}{stderr}");
    assert!(
        all.contains("built from source"),
        "expected the from-source refusal, got: {all}"
    );
    assert_eq!(out.status.code(), Some(1), "a refusal exits 1");
}

#[test]
fn update_help_lists_the_flags() {
    let out = Command::new(GLASS)
        .args(["update", "--help"])
        .output()
        .expect("run glass-mcp");
    let stdout = String::from_utf8_lossy(&out.stdout);
    for flag in [
        "--check",
        "--yes",
        "--skip-attestation",
        "--json",
        "--color",
    ] {
        assert!(
            stdout.contains(flag),
            "{flag} missing from update --help: {stdout}"
        );
    }
}
