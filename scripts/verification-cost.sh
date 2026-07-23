#!/usr/bin/env bash
# Measure what glass's verification loop costs: drive one fixed task two ways
# (semantic/text-only vs screenshot-every-step) against glass-fixture-egui and
# report round-trips, bytes, and image dimensions. On-demand contributor tooling
# (the test is #[ignore]d); NOT part of the per-PR gate.
#
#   scripts/verification-cost.sh [test_filter]
#
# Skips (exit 0) when prerequisites are missing, mirroring scripts/test-a11y.sh.
set -euo pipefail
cd "$(dirname "$0")/.."

launcher=""
for c in /usr/libexec/at-spi-bus-launcher \
         /usr/lib/at-spi2-core/at-spi-bus-launcher \
         /usr/lib/at-spi2/at-spi-bus-launcher \
         /usr/lib/x86_64-linux-gnu/at-spi2-core/at-spi-bus-launcher; do
    [ -x "$c" ] && launcher="$c" && break
done
if ! command -v dbus-daemon >/dev/null 2>&1 \
   || [ -z "$launcher" ] \
   || ! command -v Xvfb >/dev/null 2>&1; then
    echo "verification-cost: prerequisites missing — needs dbus-daemon,"
    echo "                   at-spi-bus-launcher, and Xvfb. Skipping."
    exit 0
fi

TEST_FILTER="${1:-}"

# Hermetic isolation (test-a11y.sh pattern): a throwaway XDG_RUNTIME_DIR so any
# at-spi-bus-launcher glass spawns writes here, never the operator's real bus.
RT="$(mktemp -d "${TMPDIR:-/tmp}/glass-vcost-rt.XXXXXX")"
chmod 700 "$RT"
trap 'rm -rf "$RT"' EXIT
export XDG_RUNTIME_DIR="$RT"

# --nocapture so the printed summary reaches the operator; single-threaded because
# the tests share one AT-SPI bus per process.
cargo test -p glass-testapp --test verification_cost -- \
    --ignored --nocapture --test-threads=1 "$TEST_FILTER"

echo "verification-cost: artifact at $(pwd)/target/verification-cost.json"
