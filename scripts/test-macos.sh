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

# Pure + macOS unit tests. (Capture/input integration tests are #[ignore]d and added in
# later plans; they need a granted, WindowServer-connected context — a gui/501 LaunchAgent.)
cargo test -p glass-macos "${1:-}"
