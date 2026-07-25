#!/usr/bin/env bash
# Build the role fixture into an installable APK, without Gradle: javac → d8 → aapt2 → apksigner,
# which is all this single-Activity app needs. Requires a JDK and an Android SDK with build-tools
# and a platform android.jar.
#
# ANDROID_HOME (or ANDROID_SDK_ROOT) points at the SDK; the newest installed build-tools and
# platform are used unless BUILD_TOOLS / ANDROID_PLATFORM name one.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/build"

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-$HOME/android-sdk}}"
[ -d "$sdk" ] || { echo "no Android SDK at $sdk — set ANDROID_HOME" >&2; exit 1; }

pick_newest() { ls "$1" 2>/dev/null | sort -V | tail -1; }
build_tools="${BUILD_TOOLS:-$(pick_newest "$sdk/build-tools")}"
platform="${ANDROID_PLATFORM:-$(pick_newest "$sdk/platforms")}"
tools="$sdk/build-tools/$build_tools"
android_jar="$sdk/platforms/$platform/android.jar"
[ -x "$tools/aapt2" ] || { echo "no build-tools in $sdk/build-tools" >&2; exit 1; }
[ -f "$android_jar" ] || { echo "no platform jar at $android_jar" >&2; exit 1; }

rm -rf "$out"
mkdir -p "$out/classes"

javac --release 17 -classpath "$android_jar" -d "$out/classes" \
  "$here/src/tech/fixedwidth/glassrolefixture/MainActivity.java"
"$tools/d8" --lib "$android_jar" --output "$out" $(find "$out/classes" -name '*.class')

"$tools/aapt2" link -o "$out/unsigned.apk" -I "$android_jar" \
  --manifest "$here/AndroidManifest.xml" --min-sdk-version 24 --target-sdk-version 34
(cd "$out" && zip -q unsigned.apk classes.dex)

# A throwaway signing key: Android refuses to install an unsigned APK, and nothing here is
# distributed, so the keystore is regenerated with the build rather than checked in.
keytool -genkeypair -keystore "$out/debug.jks" -storepass android -keypass android \
  -alias fixture -keyalg RSA -validity 3650 -dname "CN=glass role fixture" >/dev/null 2>&1

"$tools/zipalign" -f 4 "$out/unsigned.apk" "$out/aligned.apk"
"$tools/apksigner" sign --ks "$out/debug.jks" --ks-pass pass:android --key-pass pass:android \
  --out "$out/role-fixture.apk" "$out/aligned.apk"

echo "built: $out/role-fixture.apk"
