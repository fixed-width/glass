#!/usr/bin/env bash
# Build the role fixture into an installable APK, without Gradle: javac → d8 → aapt2 → apksigner,
# which is all this single-Activity app needs. Requires a JDK (javac, keytool), `zip`, and an
# Android SDK with build-tools and a platform android.jar.
#
# ANDROID_HOME (or ANDROID_SDK_ROOT) points at the SDK; the newest installed build-tools and
# platform are used unless BUILD_TOOLS / ANDROID_PLATFORM name one. What was chosen is printed,
# because the tree an app reports depends on what it was built against.
set -euo pipefail
here="$(cd "$(dirname "$0")" && pwd)"
out="$here/build"
apk="$out/role-fixture.apk"

# The signing key lives beside the build directory, not inside it: `rm -rf "$out"` below would
# otherwise mint a new key every build, and Android rejects an update signed with a different
# key ("signatures do not match newer version") — turning the README's `adb install -r` into a
# uninstall-first dance from the second build onwards.
keystore="$here/.signing-key.jks"

# Drop a stale APK before anything can fail: a build that aborts must not leave the previous
# artifact sitting where the README says to install from, or the next reading comes from an
# app that predates the change being tested.
rm -f "$apk"

need() {
  command -v "$1" >/dev/null 2>&1 || { echo "$2 needs $1 on PATH" >&2; exit 1; }
}
need javac "building the fixture"
need keytool "signing the fixture"
need zip "packaging the fixture"

sdk="${ANDROID_HOME:-${ANDROID_SDK_ROOT:-}}"
if [ -z "$sdk" ]; then
  for candidate in "$HOME/Android/Sdk" "$HOME/Library/Android/sdk" "$HOME/android-sdk"; do
    [ -d "$candidate" ] && { sdk="$candidate"; break; }
  done
fi
[ -n "$sdk" ] && [ -d "$sdk" ] || {
  echo "no Android SDK found — set ANDROID_HOME (tried \$ANDROID_HOME, \$ANDROID_SDK_ROOT, \
~/Android/Sdk, ~/Library/Android/sdk, ~/android-sdk)" >&2
  exit 1
}

# Newest *numeric* entry: `sort -V` would otherwise rank a preview (36.0.0-rc1, android-Baklava)
# above every released one. `|| true` keeps a missing directory out of `set -e`'s hands so the
# named diagnostic below runs instead of the script exiting mute.
pick_newest() {
  { ls "$1" 2>/dev/null || true; } | grep -E "$2" | sort -V | tail -1
}
build_tools="${BUILD_TOOLS:-$(pick_newest "$sdk/build-tools" '^[0-9]+(\.[0-9]+)*$')}"
platform="${ANDROID_PLATFORM:-$(pick_newest "$sdk/platforms" '^android-[0-9]+$')}"
[ -n "$build_tools" ] || { echo "no numbered build-tools in $sdk/build-tools" >&2; exit 1; }
[ -n "$platform" ] || { echo "no numbered platform in $sdk/platforms" >&2; exit 1; }

tools="$sdk/build-tools/$build_tools"
android_jar="$sdk/platforms/$platform/android.jar"
for tool in aapt2 d8 zipalign apksigner; do
  [ -x "$tools/$tool" ] || { echo "no $tool in $tools" >&2; exit 1; }
done
[ -f "$android_jar" ] || { echo "no platform jar at $android_jar" >&2; exit 1; }
echo "building against $platform with build-tools $build_tools"

rm -rf "$out"
mkdir -p "$out/classes"

javac --release 17 -classpath "$android_jar" -d "$out/classes" \
  "$here"/src/tech/fixedwidth/glassrolefixture/*.java
# -exec rather than $(find ...): the class list must survive a path containing spaces.
find "$out/classes" -name '*.class' -exec "$tools/d8" --lib "$android_jar" --output "$out" {} +
[ -f "$out/classes.dex" ] || { echo "d8 produced no dex" >&2; exit 1; }

# The shared page travels as an asset, copied at build time so there is one copy to edit.
mkdir -p "$out/assets"
cp "$here/../web-role-fixture/index.html" "$out/assets/index.html"

"$tools/aapt2" link -o "$out/unsigned.apk" -I "$android_jar" -A "$out/assets" \
  --manifest "$here/AndroidManifest.xml" --min-sdk-version 24 --target-sdk-version 34
# classes*.dex, not classes.dex: d8 splits past the method limit, and a silently half-packaged
# APK still installs and runs — until it reaches the missing code.
(cd "$out" && zip -q unsigned.apk classes*.dex)

# A throwaway key: nothing here is distributed, so it is generated on first build rather than
# checked in. PKCS12 is keytool's own default format, which keeps it from warning on every run.
if [ ! -f "$keystore" ]; then
  keytool -genkeypair -keystore "$keystore" -storetype PKCS12 \
    -storepass android -keypass android -alias fixture -keyalg RSA -validity 3650 \
    -dname "CN=glass role fixture" >/dev/null
fi

"$tools/zipalign" -f 4 "$out/unsigned.apk" "$out/aligned.apk"
"$tools/apksigner" sign --ks "$keystore" --ks-pass pass:android --key-pass pass:android \
  --out "$apk" "$out/aligned.apk"
"$tools/apksigner" verify "$apk"

echo "built: $apk"
