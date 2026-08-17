//! `update` through the real binary. Deliberately narrow: any test that let the command resolve
//! a release would reach github.com, and nothing in this suite touches the network. What is
//! provable here is the refusal ORDER — that a from-source build is turned away before any
//! request is made — and CI builds glass-mcp from a git checkout, so `crate::VERSION` carries a
//! `git describe` suffix and this is ordinarily exactly that case. The one test that exercises it
//! checks the built binary's actual version first and skips rather than assume, since a checkout
//! sitting exactly on a tag would otherwise drive a real request.

use std::process::Command;

const GLASS: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
fn update_refuses_a_from_source_build_without_touching_the_network() {
    // A checkout sitting exactly on a released tag reports a plain MAJOR.MINOR.PATCH with no
    // `git describe` suffix — the one shape `update` treats as a real release, which would send
    // it resolving a real latest tag over the network. Every other shape (a commit count,
    // `-dirty`, or a bare SHA) carries a dash and is guaranteed to hit the from-source refusal
    // before any request. Skip rather than let a from-a-tag checkout reach github.com.
    let version_out = Command::new(GLASS)
        .arg("--version")
        .output()
        .expect("run glass-mcp");
    let version = String::from_utf8_lossy(&version_out.stdout)
        .trim()
        .to_string();
    if !version.contains('-') {
        eprintln!(
            "skipping: version {version:?} carries no git-describe suffix (checkout is exactly \
             on a tag) — running `update` here would reach github.com"
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
