#!/usr/bin/env bash
# Build the role fixture into a Simulator .app bundle.
# Requires the full Xcode + an iOS Simulator runtime. Mirrors examples/ios-fixture/build.sh,
# with the host architecture read rather than assumed — an Intel Mac's Simulator cannot run an
# arm64 slice, and that failure would otherwise surface later as an opaque `simctl launch` error.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
app="$here/build/RoleFixture.app"
sdk="$(xcrun --sdk iphonesimulator --show-sdk-path)"
arch="$(uname -m)"

rm -rf "$app"
mkdir -p "$app"
xcrun --sdk iphonesimulator swiftc \
  -target "$arch-apple-ios16.0-simulator" \
  -sdk "$sdk" \
  -parse-as-library \
  -o "$app/RoleFixture" \
  "$here/RoleFixture.swift"

# The plist names the executable; a rename on one side alone yields a bundle that installs and
# then fails to launch, with the build still reporting success.
plutil -lint "$here/Info.plist" >/dev/null
executable="$(plutil -extract CFBundleExecutable raw -o - "$here/Info.plist")"
[ "$executable" = "RoleFixture" ] || {
  echo "Info.plist CFBundleExecutable is '$executable', but the binary is built as RoleFixture" >&2
  exit 1
}
cp "$here/Info.plist" "$app/Info.plist"

# The web tab loads this page out of the bundle, and shares it with every other platform's web
# reading, so it is copied rather than duplicated.
cp "$here/../web-role-fixture/index.html" "$app/index.html"

echo "built: $app ($arch, $(xcrun --sdk iphonesimulator --show-sdk-version) SDK)"
