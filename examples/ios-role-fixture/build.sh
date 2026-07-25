#!/usr/bin/env bash
# Build the role fixture into a Simulator .app bundle.
# Requires the full Xcode + an iOS Simulator runtime. Mirrors examples/ios-fixture/build.sh.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
app="$here/build/RoleFixture.app"
sdk="$(xcrun --sdk iphonesimulator --show-sdk-path)"

rm -rf "$app"
mkdir -p "$app"
xcrun --sdk iphonesimulator swiftc \
  -target arm64-apple-ios16.0-simulator \
  -sdk "$sdk" \
  -parse-as-library \
  -o "$app/RoleFixture" \
  "$here/RoleFixture.swift"
cp "$here/Info.plist" "$app/Info.plist"
echo "built: $app"
