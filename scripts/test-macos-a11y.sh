#!/usr/bin/env bash
# Compile/link gate for the macOS accessibility-reader integration test
# (crates/glass-macos/tests/a11y.rs, the `a11y` [[test]] target) and its Swift fixture
# (crates/glass-macos/fixture/a11y_fixture.swift). Skips (exit 0) when not on macOS,
# mirroring scripts/test-macos.sh / test-macos-mcp.sh, so it's safe to call from any CI
# matrix leg.
set -euo pipefail

if [[ "$(uname -s)" != "Darwin" ]]; then
  echo "test-macos-a11y.sh: not macOS (uname=$(uname -s)) — skipping."
  exit 0
fi

# Run from the repo root so `cargo -p` resolves regardless of the caller's cwd (mirrors
# scripts/test-macos.sh / test-x11.sh / test-wayland.sh / test-windows.sh).
cd "$(dirname "$0")/.."

TMP="$(mktemp -d)"
trap 'rm -rf "$TMP"' EXIT

# `a11y_fixture.swift` uses a top-level `@main` type rather than unadorned top-level
# statements, so it needs `-parse-as-library` — same gotcha `quadrants.swift` has, covered
# in docs/how-to/build-from-source.md's "headless / SSH setup" troubleshooting section. Building it
# here only proves the fixture source is valid Swift; it is never executed by this script.
echo "test-macos-a11y.sh: building a11y_fixture.swift..."
swiftc -parse-as-library -o "$TMP/a11y_fixture" crates/glass-macos/fixture/a11y_fixture.swift

# Build (never run) the harness=false `a11y` integration test binary — confirms it compiles
# and links against `glass-a11y-macos`'s AXUIElement reader, the same `--no-run` compile/
# link gate scripts/test-macos.sh's GLASS_MACOS_ONBOX block applies to `capture`/`input`/
# `windows`.
echo "test-macos-a11y.sh: building the a11y test binary..."
cargo test -p glass-macos --test a11y --no-run

# The GRANTED run — actually exercising the AXUIElement snapshot/set_value/click path
# against the fixture built above — needs the Accessibility (and Screen Recording, for
# MacosPlatform::new's preflight) TCC grants, which only the signed, granted GlassProbe.app
# bundle holds on this project's dev Mac. That happens out-of-band from this script: copy
# the `a11y` test binary this script just built into the GlassProbe.app bundle, re-sign,
# and launch it via a `gui/501` LaunchAgent so it inherits the grant — the same recipe
# test-macos.sh's GLASS_MACOS_ONBOX block documents for `capture`/`input`/`windows`. Point
# the launched binary at a fixture built the same way as above via
# `GLASS_A11Y_FIXTURE_BIN=/path/to/a11y_fixture` — crates/glass-macos/tests/a11y.rs falls
# back to building its own copy with `swiftc` when the var is unset, but the granted run
# pre-builds one so the granted process doesn't need a writable `$TMPDIR` / working
# `swiftc` invocation in that context. No machine-specific paths are hardcoded here; the
# out-of-band recipe supplies them at run time.
echo "test-macos-a11y.sh: OK (compile/link only — the granted run happens out-of-band; see comments above)."
