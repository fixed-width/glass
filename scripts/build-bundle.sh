#!/usr/bin/env bash
# Build the beta distribution bundle: a release `glass-mcp` for two Linux x86-64
# targets — glibc-dynamic ("gnu") and static ("musl-static") — each stripped and
# zipped with its beta README (sources in rust/packaging/).
# Produces two variants per target: the default (network-enabled) build and a
# stdio-only ("free") build (--no-default-features, no network transport linked).
#
#   ./scripts/build-bundle.sh [OUTPUT_DIR]      # default OUTPUT_DIR=/tmp/glass-beta
#
# Produces, under OUTPUT_DIR:
#   glass-mcp-beta-{gnu,musl-static}/{glass-mcp,README.md}
#   glass-mcp-beta-linux-x86_64-{gnu,musl-static}.zip
#   glass-mcp-beta-{gnu,musl-static}-stdio/{glass-mcp,README.md}
#   glass-mcp-beta-linux-x86_64-{gnu,musl-static}-stdio.zip
set -euo pipefail

RUST_DIR="$(cd "$(dirname "$0")/.." && pwd)"   # the rust/ workspace root
cd "$RUST_DIR"
OUT="${1:-/tmp/glass-beta}"
PKG="$RUST_DIR/packaging"                        # checked-in README sources
MUSL="x86_64-unknown-linux-musl"

command -v zip >/dev/null   || { echo "error: 'zip' is required"   >&2; exit 1; }
command -v strip >/dev/null || { echo "error: 'strip' is required" >&2; exit 1; }

# Ensure the musl std target is present ("for if we don't have one"); no-op if so.
rustup target add "$MUSL"

mkdir -p "$OUT"

# bundle <name> <cargo-target-or-empty> <readme-file> <built-binary-path> [extra-cargo-flags...]
bundle() {
  local name="$1" tgt="$2" readme="$3" bin="$4"
  shift 4
  echo "==> building $name"
  if [ -n "$tgt" ]; then
    cargo build --release --target "$tgt" -p glass-mcp "$@"
  else
    cargo build --release -p glass-mcp "$@"
  fi
  local dir="$OUT/glass-mcp-beta-$name"
  rm -rf "$dir"; mkdir -p "$dir"
  install -m 0755 "$bin" "$dir/glass-mcp"
  strip "$dir/glass-mcp"
  cp "$PKG/$readme" "$dir/README.md"
  ( cd "$OUT" \
      && rm -f "glass-mcp-beta-linux-x86_64-$name.zip" \
      && zip -qr "glass-mcp-beta-linux-x86_64-$name.zip" "glass-mcp-beta-$name" )
  echo "    -> $OUT/glass-mcp-beta-linux-x86_64-$name.zip"
}

# Network-enabled (default) builds.
bundle gnu         ""      README-gnu.md  "target/release/glass-mcp"
bundle musl-static "$MUSL" README-musl.md "target/$MUSL/release/glass-mcp"

# Stdio-only ("free") builds: no network transport linked.
bundle gnu-stdio         ""      README-gnu.md  "target/release/glass-mcp"         --no-default-features
bundle musl-static-stdio "$MUSL" README-musl.md "target/$MUSL/release/glass-mcp"  --no-default-features

echo "Done. Bundles in $OUT"
