//! `--color` end to end through the real binary — the tty detection and palette wiring that a
//! unit test calling `render_styled` directly cannot reach.
//!
//! `env` rather than `doctor` for most cases: it runs no probes, so it is fast and its output
//! does not depend on what the host has installed.

use std::process::Command;

const GLASS: &str = env!("CARGO_BIN_EXE_glass-mcp");

/// Run glass-mcp and return its stdout, having first confirmed the command actually produced its
/// report. `banner` is text the subcommand always prints, colored or not — asserting it stops the
/// escape assertions below from passing vacuously on the empty output of a crashed or
/// silently-exiting binary.
///
/// `NO_COLOR` is cleared so a developer who sets it in their own shell doesn't quietly turn these
/// assertions into no-ops.
fn stdout_of(args: &[&str], banner: &str) -> String {
    let out = Command::new(GLASS)
        .args(args)
        .env_remove("NO_COLOR")
        .output()
        .expect("run glass-mcp");
    let stdout = String::from_utf8(out.stdout).expect("stdout is utf-8");
    assert!(
        stdout.contains(banner),
        "expected {banner:?} in stdout; got {stdout:?}"
    );
    stdout
}

#[test]
fn color_always_emits_escapes() {
    assert!(stdout_of(&["env", "--color", "always"], "glass environment").contains('\x1b'));
}

#[test]
fn color_never_emits_none() {
    assert!(!stdout_of(&["env", "--color", "never"], "glass environment").contains('\x1b'));
}

#[test]
fn the_default_is_plain_when_stdout_is_not_a_terminal() {
    // The harness captures stdout through a pipe — exactly the redirected case auto must not
    // color, and the reason `glass-mcp env > file` stays machine-readable.
    assert!(!stdout_of(&["env"], "glass environment").contains('\x1b'));
}

#[test]
fn an_explicit_always_overrides_no_color() {
    let out = Command::new(GLASS)
        .args(["env", "--color", "always"])
        .env("NO_COLOR", "1")
        .output()
        .expect("run glass-mcp");
    assert!(String::from_utf8(out.stdout).unwrap().contains('\x1b'));
}

#[test]
fn json_is_never_colored_even_when_color_is_forced() {
    // `serde_json::from_str` below already fails on empty input, so this call doesn't need the
    // `stdout_of` banner guard to avoid a vacuous pass — but the `"glass"` top-level key is real
    // JSON output, so it doubles as a cheap sanity check on top of that.
    let out = stdout_of(&["env", "--json", "--color", "always"], "\"glass\"");
    assert!(!out.contains('\x1b'), "{out}");
    serde_json::from_str::<serde_json::Value>(&out).expect("still valid json");
}

#[test]
fn doctor_is_wired_to_the_same_flag() {
    // Only that the aggregator path colors — not the exit code, which depends on what this host
    // has installed.
    assert!(stdout_of(&["doctor", "--color", "always"], "glass doctor").contains('\x1b'));
}
