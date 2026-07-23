#!/usr/bin/env bash
# Build the Greeter SwiftUI demo into a Simulator .app bundle.
# Requires the full Xcode + an iOS Simulator runtime. See docs/how-to/drive-an-ios-app.md.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
app="$here/build/Greeter.app"
sdk="$(xcrun --sdk iphonesimulator --show-sdk-path)"

rm -rf "$app"
mkdir -p "$app"
xcrun --sdk iphonesimulator swiftc \
  -target arm64-apple-ios16.0-simulator \
  -sdk "$sdk" \
  -parse-as-library \
  -o "$app/Greeter" \
  "$here/Greeter.swift"
cp "$here/Info.plist" "$app/Info.plist"
echo "built: $app"
