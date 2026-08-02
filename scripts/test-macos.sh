#!/usr/bin/env bash
# Run the glass-macos suite. Skips (exit 0) when not on macOS, so it is safe to call
# from any CI matrix leg — mirroring scripts/test-x11.sh / test-wayland.sh.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "test-macos.sh: not macOS (uname=$(uname -s)) — skipping."
  exit 0
fi

# Run from the repo root so `cargo -p` resolves regardless of the caller's cwd
# (mirrors scripts/test-x11.sh / test-wayland.sh / test-windows.sh).
cd "$(dirname "$0")/.."

# process::tests' SandboxLevel::Default tests (landed 2026-07-01) spawn this fixture and assert
# the clip shim (injection support added 2026-07-02) was actually injected into it — see
# src/bin/sandbox_probe.rs and process.rs's spawn doc. Neither the fixture nor the shim is a
# Cargo dependency of glass-macos, so `cargo test -p glass-macos --lib` below builds neither on
# its own: build both explicitly rather than relying on an earlier, separate `cargo build
# --workspace` step having happened to build them first. That exact incidental ordering — the
# shim dylib not existing when a scoped `-p glass-macos` test run built nothing else — is what
# let the tests pass for about a month (2026-07-02 until this fix) without the shim ever
# resolving, so injection was silently skipped every time.
#
# Two separate invocations, not one `-p glass-macos --bin ... -p glass-clip-shim-macos`: a
# `--bin NAME` filter applies across every `-p` package in the same command, and
# glass-clip-shim-macos has no bin target named `glass-macos-sandbox-probe` — combined, cargo
# silently builds nothing for it rather than erroring (verified: 0.04s, no `Compiling` line, no
# dylib on disk afterward).
cargo build -p glass-macos --bin glass-macos-sandbox-probe
cargo build -p glass-clip-shim-macos

# Pure + macOS unit tests only (`--lib`), minus the `#[ignore]`d ones that need a window
# server (GLASS_MACOS_ONBOX below runs those). `crates/glass-macos/tests/capture.rs` is now a
# real `[[test]]` target (see Cargo.toml), so a plain `cargo test -p glass-macos` with no
# `--lib` filter would also try to build+run it — and it needs a granted, WindowServer
# -connected context (a gui/501 LaunchAgent) that a plain on-box or CI run doesn't have,
# so it would fail every ungranted run. `--lib` keeps this default invocation to exactly
# the unit tests; see GLASS_MACOS_ONBOX below for the capture test.
cargo test -p glass-macos --lib "${1:-}"

# GLASS_MACOS_ONBOX=1: also build the harness=false capture integration test
# (crates/glass-macos/tests/capture.rs) — the first-real-pixels proof of the whole
# MacosPlatform::start_app -> capture_frame path via ScreenCaptureKit, using the native
# fixture/quadrants.swift known-color window. Building it here just confirms it compiles
# and links; it needs the Screen Recording TCC grant to actually PASS, which only a
# signed, granted app bundle holds — so the real run happens out-of-band, copying this
# built binary into the granted GlassProbe.app bundle, re-signing, and launching via a
# gui/501 LaunchAgent so it inherits the grant. Plain `./scripts/test-macos.sh` (no env
# set) never touches this.
if [[ "${GLASS_MACOS_ONBOX:-0}" == "1" ]]; then
  # The `--ignore`d unit tests that need a real Mac to mean anything: they build a Swift
  # fixture and launch it into the window server, so a plain `cargo test` (and CI) must skip
  # them. Unlike the integration binaries below, these DO run here — this is the only
  # invocation that runs them, and it must be from a session with a window server.
  echo "GLASS_MACOS_ONBOX=1: running the on-device --lib tests..."
  cargo test -p glass-macos --lib -- --ignored "${1:-}"

  echo "GLASS_MACOS_ONBOX=1: building the capture integration test binary..."
  cargo test -p glass-macos --test capture --no-run

  # Same story as `capture` above, for crates/glass-macos/tests/input.rs (the send_key/
  # send_pointer end-to-end proof) — building here just confirms it compiles and links; the
  # granted run needs both Screen Recording and Accessibility TCC grants, so it happens
  # out-of-band via the same GlassProbe.app LaunchAgent procedure.
  echo "GLASS_MACOS_ONBOX=1: building the input integration test binary..."
  cargo test -p glass-macos --test input --no-run

  # Same story again, for crates/glass-macos/tests/windows.rs (the list_windows/
  # select_window/window(op) end-to-end proof, incl. the private CGWindowID<->AXUIElement
  # correlation) — building here just confirms it compiles and links; the granted run needs
  # both TCC grants (same as `input` above) plus an unlocked screen session, so it happens
  # out-of-band via the same GlassProbe.app LaunchAgent procedure.
  echo "GLASS_MACOS_ONBOX=1: building the window integration test binary..."
  cargo test -p glass-macos --test windows --no-run

  # Same story for crates/glass-macos/tests/bundle_launch.rs — building it here is what keeps
  # its assertions from rotting between granted runs, which are rare and manual.
  echo "GLASS_MACOS_ONBOX=1: building the bundle-launch integration test binary..."
  cargo test -p glass-macos --test bundle_launch --no-run
fi
