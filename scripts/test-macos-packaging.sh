#!/usr/bin/env bash
# Exercise drag-install permissions without a GUI, signing identity or privacy grants.
set -euo pipefail

[[ "$(uname -s)" == Darwin ]] || { echo "macOS packaging test: skipping on this host"; exit 0; }
cd "$(dirname "$0")/.."
work="$(mktemp -d)"
mounted=0
cleanup() {
  if [[ "$mounted" == 1 ]]; then hdiutil detach "$work/mount"; fi
  chmod -RN "$work"
  rm -rf "$work"
}
trap cleanup EXIT

umask 077
app="$work/GlassPackagingProbe.app"
mkdir -p "$app/Contents/MacOS" "$app/Contents/Resources" "$work/mount"
cp /usr/bin/true "$app/Contents/MacOS/glass-mcp"
cp packaging/macos/Info.plist "$app/Contents/Info.plist"
printf 'packaging regression\n' > "$app/Contents/Resources/probe.txt"
codesign --force --sign - --timestamp=none "$app"
chmod -R go-rwx "$app"
chmod +a 'user:nobody deny read,execute' "$app"
# Simple test commands honor errexit in macOS's system Bash too.
test "$(ls -lde "$app" | wc -l | tr -d ' ')" = 2
find "$app" -type d -print0 > "$work/input-directories"
test -s "$work/input-directories"
while IFS= read -r -d '' directory; do
  test "$(stat -f '%Lp' "$directory")" = 700
done < "$work/input-directories"
find "$app" -type f -print0 > "$work/input-files"
test -s "$work/input-files"
while IFS= read -r -d '' file; do
  if [[ "$file" == "$app/Contents/MacOS/glass-mcp" ]]; then
    test "$(stat -f '%Lp' "$file")" = 700
  else
    test "$(stat -f '%Lp' "$file")" = 600
  fi
done < "$work/input-files"

bash packaging/macos/make-dmg.sh --app "$app" --out "$work/dist" --version permissions-test
image="$work/dist/glass-mcp-permissions-test-universal-apple-darwin.dmg"
hdiutil attach -readonly -owners on -nobrowse -noautoopen -mountpoint "$work/mount" "$image"
mounted=1
installed="$work/mount/GlassPackagingProbe.app"
test "$(stat -f '%Lp' "$work/mount")" = 755
find "$installed" -type d -print0 > "$work/output-directories"
test -s "$work/output-directories"
while IFS= read -r -d '' directory; do
  test "$(stat -f '%Lp' "$directory")" = 755
  test "$(ls -lde "$directory" | wc -l | tr -d ' ')" = 1
done < "$work/output-directories"
find "$installed" -type f -print0 > "$work/output-files"
test -s "$work/output-files"
while IFS= read -r -d '' file; do
  relative="${file#"$installed/"}"
  cmp "$file" "$app/$relative"
  test "$(ls -le "$file" | wc -l | tr -d ' ')" = 1
  if [[ "$relative" == Contents/MacOS/glass-mcp ]]; then
    test "$(stat -f '%Lp' "$file")" = 755
  else
    test "$(stat -f '%Lp' "$file")" = 644
  fi
done < "$work/output-files"
codesign --verify --deep --strict "$installed"
test -L "$work/mount/Applications"
test "$(readlink "$work/mount/Applications")" = /Applications
test -f "$work/mount/README.md"
test "$(stat -f '%Lp' "$app")" = 700
test "$(ls -lde "$app" | wc -l | tr -d ' ')" = 2
echo "macOS packaging test: private input produces an accessible, unchanged signed app"
