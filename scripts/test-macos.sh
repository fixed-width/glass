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

# Pure + macOS unit tests only (`--lib`). `crates/glass-macos/tests/capture.rs` is now a
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
# gui/501 LaunchAgent so it inherits the grant (see
# .superpowers/sdd/objc2-spike-report.md and .superpowers/sdd/task-6-brief.md for the
# exact procedure). Plain `./scripts/test-macos.sh` (no env set) never touches this.
if [[ "${GLASS_MACOS_ONBOX:-0}" == "1" ]]; then
  echo "GLASS_MACOS_ONBOX=1: building the capture integration test binary..."
  cargo test -p glass-macos --test capture --no-run
fi
