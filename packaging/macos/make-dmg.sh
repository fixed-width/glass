#!/usr/bin/env bash
# Build a drag-install .dmg around a signed GlassMcp.app: stage the app, a README and an
# /Applications symlink, then hdiutil a compressed read-only image. Sign/notarize/staple happen
# separately (notarize.sh).
#
#   ./packaging/macos/make-dmg.sh --app <path.app> --out <dir> --version <X.Y.Z>
set -euo pipefail

app="" out="" version=""
while [ $# -gt 0 ]; do
  case "$1" in
    --app)     [ $# -ge 2 ] || { echo "error: --app requires a value" >&2; exit 1; };     app="$2"; shift 2 ;;
    --out)     [ $# -ge 2 ] || { echo "error: --out requires a value" >&2; exit 1; };     out="$2"; shift 2 ;;
    --version) [ $# -ge 2 ] || { echo "error: --version requires a value" >&2; exit 1; }; version="$2"; shift 2 ;;
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
# Preserve signed bytes and the stapled ticket without inheriting machine-specific ACLs.
staged_app="$staging/$(basename "$app")"
ditto --noacl "$app" "$staged_app"
# Private build directories must not make the installed app inaccessible to other accounts.
chmod 755 "$staging"
find "$staged_app" -type d -exec chmod 755 {} +
find "$staged_app" -type f -exec chmod a+rX,go-w {} +
codesign --verify --deep --strict "$staged_app"
# Beside the bundle, never inside it — a file added within the .app would invalidate its
# signature — and resolved from this script's own location rather than passed in, so no
# caller can pass the wrong one.
readme="$(cd "$(dirname "$0")/.." && pwd)/README-macos.md"
[ -f "$readme" ] || { echo "error: README not found: $readme" >&2; exit 1; }
cp "$readme" "$staging/README.md"
ln -s /Applications "$staging/Applications"
dmg="$out/${name}.dmg"
rm -f "$dmg"
hdiutil create -volname "glass" -srcfolder "$staging" -ov -format UDZO "$dmg"
echo "==> done: $dmg"
