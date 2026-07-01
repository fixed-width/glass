#!/usr/bin/env bash
# Assemble and codesign GlassMcp.app: a macOS app bundle wrapping the release
# `glass-mcp` binary, so it can be granted Screen Recording / Accessibility once
# and run as a `gui/<uid>` LaunchAgent (see docs/running-on-macos.md and
# packaging/macos/README.md for the full setup).
#
#   ./packaging/macos/build-app.sh --identity "<signing identity>" [options]
#
# Required:
#   --identity NAME     codesign identity (a Keychain "Common Name") to sign with.
#                        (env: GLASS_SIGN_IDENTITY)
#
# Optional:
#   --bundle-id ID       CFBundleIdentifier (default: tech.fixedwidth.glass — the
#                        production identifier). (env: GLASS_BUNDLE_ID)
#   --keychain PATH      keychain to resolve --identity from, passed straight to
#                        `codesign --keychain` (default: the keychain search list).
#                        (env: GLASS_SIGN_KEYCHAIN)
#   --out DIR            output directory for GlassMcp.app (default:
#                        target/macos-app under the repo root).
#   --version X.Y.Z      CFBundleShortVersionString (default: the template's).
#   --build N            CFBundleVersion (default: the template's).
#   --skip-build         reuse the existing target/release/glass-mcp binary
#                        instead of rebuilding — for iterating on packaging only.
#
# TCC (Screen Recording / Accessibility) grants key on the bundle's Designated
# Requirement — bundle id + signing certificate — not on the binary's cdhash. So
# re-running this script with the SAME --identity/--bundle-id after a code change
# re-signs a new binary WITHOUT losing a previously-granted permission; changing
# either the bundle id or the identity produces a new Designated Requirement and
# starts the grant over.
set -euo pipefail

usage() {
  cat >&2 <<'EOF'
usage: build-app.sh --identity NAME [--bundle-id ID] [--keychain PATH]
                     [--out DIR] [--version X.Y.Z] [--build N] [--skip-build]
EOF
  exit 1
}

SCRIPT_DIR="$(cd "$(dirname "${BASH_SOURCE[0]}")" && pwd)"
REPO_ROOT="$(cd "$SCRIPT_DIR/../.." && pwd)"

bundle_id="${GLASS_BUNDLE_ID:-tech.fixedwidth.glass}"
sign_identity="${GLASS_SIGN_IDENTITY:-}"
sign_keychain="${GLASS_SIGN_KEYCHAIN:-}"
out_dir="$REPO_ROOT/target/macos-app"
version=""
build_num=""
skip_build=0

while [ $# -gt 0 ]; do
  case "$1" in
    --identity)   sign_identity="$2"; shift 2 ;;
    --bundle-id)  bundle_id="$2"; shift 2 ;;
    --keychain)   sign_keychain="$2"; shift 2 ;;
    --out)        out_dir="$2"; shift 2 ;;
    --version)    version="$2"; shift 2 ;;
    --build)      build_num="$2"; shift 2 ;;
    --skip-build) skip_build=1; shift ;;
    -h|--help)    usage ;;
    *) echo "error: unknown argument: $1" >&2; usage ;;
  esac
done

if [ "$(uname -s)" != "Darwin" ]; then
  echo "error: build-app.sh must run on macOS (codesign is macOS-only)" >&2
  exit 1
fi

if [ -z "$sign_identity" ]; then
  echo "error: --identity (or GLASS_SIGN_IDENTITY) is required." >&2
  echo "       There is no default, so a build never lands ad-hoc-signed by accident:" >&2
  echo "       an ad-hoc signature (-s -) has no stable Designated Requirement, so" >&2
  echo "       Screen Recording / Accessibility grants would NOT survive a rebuild." >&2
  echo "       See docs/running-on-macos.md for how to create a signing identity." >&2
  exit 1
fi

if ! command -v /usr/libexec/PlistBuddy >/dev/null 2>&1; then
  echo "error: /usr/libexec/PlistBuddy not found (expected on every macOS install)" >&2
  exit 1
fi

echo "==> building glass-mcp (release)"
bin="$REPO_ROOT/target/release/glass-mcp"
if [ "$skip_build" -eq 0 ]; then
  ( cd "$REPO_ROOT" && cargo build --release -p glass-mcp )
fi
if [ ! -x "$bin" ]; then
  echo "error: $bin not found or not executable (run without --skip-build first)" >&2
  exit 1
fi

app="$out_dir/GlassMcp.app"
echo "==> assembling $app"
rm -rf "$app"
mkdir -p "$app/Contents/MacOS"
install -m 0755 "$bin" "$app/Contents/MacOS/glass-mcp"
cp "$SCRIPT_DIR/Info.plist" "$app/Contents/Info.plist"

/usr/libexec/PlistBuddy -c "Set :CFBundleIdentifier $bundle_id" "$app/Contents/Info.plist"
if [ -n "$version" ]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleShortVersionString $version" "$app/Contents/Info.plist"
fi
if [ -n "$build_num" ]; then
  /usr/libexec/PlistBuddy -c "Set :CFBundleVersion $build_num" "$app/Contents/Info.plist"
fi

echo "==> codesigning (identity: $sign_identity, bundle id: $bundle_id)"
codesign_args=(--force --options runtime -s "$sign_identity")
if [ -n "$sign_keychain" ]; then
  codesign_args+=(--keychain "$sign_keychain")
fi
codesign "${codesign_args[@]}" "$app"

echo "==> verifying"
codesign --verify --strict "$app"
codesign -dv "$app"

echo "==> done: $app"
