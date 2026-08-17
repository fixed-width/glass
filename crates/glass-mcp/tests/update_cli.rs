//! `update` through the real binary. Deliberately narrow: any test that let the command resolve
//! a release would reach github.com, and nothing in this suite touches the network. What is
//! provable here is the refusal ORDER — that a from-source build is turned away before any
//! request is made — and CI builds glass-mcp from a git checkout, so `crate::VERSION` carries a
//! `git describe` suffix and this is exactly that case.

use std::process::Command;

const GLASS: &str = env!("CARGO_BIN_EXE_glass-mcp");

#[test]
fn update_refuses_a_from_source_build_without_touching_the_network() {
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
