#!/usr/bin/env bash
# Build a drag-install .dmg around a signed GlassMcp.app: stage the app + an /Applications symlink,
# then hdiutil a compressed read-only image. Sign/notarize/staple happen separately (notarize.sh).
#
#   ./packaging/macos/make-dmg.sh --app <path.app> --out <dir> --version <X.Y.Z>
set -euo pipefail

app="" out="" version=""
while [ $# -gt 0 ]; do
  case "$1" in
    --app)     app="$2"; shift 2 ;;
    --out)     out="$2"; shift 2 ;;
    --version) version="$2"; shift 2 ;;
    -h|--help) echo "usage: make-dmg.sh --app PATH --out DIR --version X.Y.Z" >&2; exit 1 ;;
    *) echo "error: unknown argument: $1" >&2; exit 1 ;;
  esac
done
[ "$(uname -s)" = "Darwin" ] || { echo "error: make-dmg.sh must run on macOS" >&2; exit 1; }
for v in app out version; do [ -n "${!v}" ] || { echo "error: --${v} is required" >&2; exit 1; }; done
[ -d "$app" ] || { echo "error: app bundle not found: $app" >&2; exit 1; }

mkdir -p "$out"
name="glass-mcp-${version}-universal-apple-darwin"
staging="$(mktemp -d)"
trap 'rm -rf "$staging"' EXIT
# ditto (not cp -R) is the guaranteed-faithful copy for a signed bundle — it preserves the
# bundle layout + code signature, matching release.yml's Package step.
ditto "$app" "$staging/$(basename "$app")"
ln -s /Applications "$staging/Applications"
dmg="$out/${name}.dmg"
rm -f "$dmg"
hdiutil create -volname "glass" -srcfolder "$staging" -ov -format UDZO "$dmg"
echo "==> done: $dmg"
