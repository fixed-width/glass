#!/usr/bin/env bash
# Run the X11 integration suite (the #[ignore]d tests in tests/integration.rs, the over-HTTP
# e2e in tests/network.rs, and the ignore-regions MCP e2e in tests/ignore_regions_e2e.rs).
# Each test starts its own private Xvfb, so this only requires Xvfb to be installed. (The
# Wayland tests live in tests/wayland.rs and tests/wayland_ignore_regions_e2e.rs, run via
# scripts/test-wayland.sh — kept separate so the Wayland tests' Xwayland and the X11 tests'
# Xvfb don't contend.)
#
# NOTE: the sandbox_* tests (sandbox_default_app_still_runs_and_captures,
# sandbox_default_build_step_cannot_write_real_home, etc.) require 'bubblewrap'
# to be installed (sudo apt-get install -y bubblewrap on Debian/Ubuntu) AND unprivileged
# user namespaces enabled. Ubuntu 24.04 restricts them via AppArmor
# (kernel.apparmor_restrict_unprivileged_userns=1) — the CI workflow re-enables them.
# The GLASS_SANDBOX env var controls containment for glass-mcp-launched apps generally
# (off / default / strict); it has no effect on integration tests, which set their
# sandbox level explicitly in the AppSpec.
set -euo pipefail
cd "$(dirname "$0")/.."
# host_conformance spawns the glass-mcp *binary* as a stdio child. `cargo test -p glass-testapp`
# builds glass-mcp only as a library dependency, not its binary, so build the binary explicitly
# — otherwise the test can't find it in a clean checkout (e.g. CI, which runs only this script).
cargo build -p glass-mcp --bin glass-mcp
exec cargo test -p glass-testapp --test integration --test network --test ignore_regions_e2e --test host_conformance --test software_render -- --ignored --test-threads=1 "$@"
