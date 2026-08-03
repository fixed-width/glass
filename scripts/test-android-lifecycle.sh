#!/usr/bin/env bash
# Run the managed-AVD lifecycle test: glass boots an emulator itself, reuses it on a second
# resolve, and kills it on cleanup. Requires the Android SDK and an AVD definition, and requires
# that NO emulator is already running — the test's whole subject is the boot.
#
#   ANDROID_SDK_ROOT=$HOME/android-sdk GLASS_ADB=$ANDROID_SDK_ROOT/platform-tools/adb \
#     GLASS_AVD=glass ./scripts/test-android-lifecycle.sh
#
# Kept out of scripts/test-android.sh because that script's tests need a booted device, which is
# exactly the precondition this one requires to be absent.
set -euo pipefail
cd "$(dirname "$0")/.."
exec cargo test -p glass-android --test managed_avd -- --ignored --test-threads=1 "$@"
