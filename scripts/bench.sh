#!/usr/bin/env bash
# Run the glass benchmarks, or flamegraph-profile one bench.
#
#   scripts/bench.sh                          # run all benches (core + x11/windows/wayland)
#   scripts/bench.sh <bench> [filter] [pkg]   # flamegraph one bench (needs cargo-flamegraph + perf)
#
# Examples:
#   scripts/bench.sh                              # full run
#   scripts/bench.sh diff "identical/1920x1080"
#   scripts/bench.sh pixels "" glass-windows     # 'pixels' is in 3 crates — name one with pkg
set -euo pipefail
cd "$(dirname "$0")/.."

if [ $# -eq 0 ]; then
    exec cargo bench -p glass-core -p glass-x11 -p glass-windows -p glass-wayland
fi

bench="$1"
filter="${2:-}"
pkg="${3:-}"
pkg_arg=()
[ -n "$pkg" ] && pkg_arg=(-p "$pkg")
command -v cargo-flamegraph >/dev/null 2>&1 \
    || { echo "cargo-flamegraph not installed: cargo install flamegraph"; exit 1; }
# perf needs kernel.perf_event_paranoid <= 1 for user-space sampling without root.
# Writes flamegraph.svg to the current directory.
if [ -n "$filter" ]; then
    exec cargo flamegraph "${pkg_arg[@]}" --bench "$bench" -- "$filter"
else
    exec cargo flamegraph "${pkg_arg[@]}" --bench "$bench"
fi
