# Set up glass for Android

The Android backend drives a native Android app in an AVD emulator over `adb`. It is
**host-OS-agnostic** — it shells out to `adb`, so it runs from a Linux, Windows, or macOS host — and
the emulator VM *is* the sandbox, so there is no separate containment step. This guide is the same for
every host; the few host-specific differences (the `adb` path, the SDK location) are called out
inline.

Select the backend per launch with `glass_start`'s `backend: "android"`, or make it the default with
`GLASS_BACKEND=android`.

## Supported Android versions

glass is developed and tested against **Android 14 (API 34)** — use an `android-34` Google APIs image
when in doubt. The `adb` backend (capture, input, windows, logs) assumes no particular version and
runs on older releases too. The optional companions (below) declare an Android 7.0 (API 24) `minSdk`
floor, but API 34 is what's exercised. The agent reaches non-public framework internals by reflection,
with a fallback for the one that moved at API 34 (input injection), so newer releases should keep
working; wide cross-API testing isn't done.

## Install the Android SDK tools

You need `adb` and `emulator` from the Android SDK — via Android Studio (its SDK manager installs
`platform-tools` and `emulator`) or the command-line tools:

```bash
sdkmanager "platforms;android-34" "platform-tools" "emulator"
```

Point glass at `adb` with `GLASS_ADB`, or put it on `PATH`:

```bash
export GLASS_ADB=~/android-sdk/platform-tools/adb          # Linux/macOS
```

> On **Windows**, use the full path to `adb.exe`, e.g.
> `$env:GLASS_ADB = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"`.

Set `ANDROID_SDK_ROOT` (or `ANDROID_HOME`) so glass can find the `emulator` alongside `adb`:

```bash
export ANDROID_SDK_ROOT=~/android-sdk                       # Windows: %LOCALAPPDATA%\Android\Sdk
```

## Create an AVD

If you don't have an emulator image yet, install a system image and create one (named `glass` here,
which `GLASS_AVD=glass` then selects):

```bash
sdkmanager "system-images;android-34;google_apis;x86_64"
avdmanager create avd -n glass -k "system-images;android-34;google_apis;x86_64" --device pixel_6
```

## Managed AVD (attach-or-boot)

Like Android Studio, glass prefers to attach: if an emulator is already online it uses it
(`GLASS_ANDROID_SERIAL` picks one when several are running). If none is running, glass boots a
**headless** AVD itself and stops it on shutdown — choose it with `GLASS_AVD` (needed only when you
have more than one AVD). Force attach-only with `GLASS_ANDROID_LIFECYCLE=attach`.

The `emulator` binary resolves from `GLASS_EMULATOR` / `ANDROID_SDK_ROOT` / `ANDROID_HOME`; pass extra
boot flags via `GLASS_EMULATOR_ARGS`; keep a glass-booted emulator alive past shutdown with
`GLASS_EMULATOR_KEEP`. (All variables: [reference/environment.md](../reference/environment.md#android).)

## Optional: on-device agent (clipboard + high-fidelity input)

Over plain `adb`, glass types with `input text`/`keyevent` and can't reach the system clipboard. A
small companion — [glass-android-agent](https://github.com/fixed-width/glass-android-agent), a separate
Apache-2.0 repo — closes both gaps: it runs on the device as a shell-uid `app_process` server and gives
glass real `MotionEvent`/`KeyEvent` injection (faithful Unicode, plus multi-touch gestures via
`glass_gesture`) and clipboard get/set.

Point `GLASS_ANDROID_AGENT_JAR` at its `glass-agent.jar`:

- Download the prebuilt jar from the agent repo's
  [Releases](https://github.com/fixed-width/glass-android-agent/releases), or
- build it yourself: `./gradlew dex` in the agent repo produces `glass-agent.jar`.

glass pushes, launches, and tears the agent down for you. Without it, glass uses the `adb` input path
and `glass_clipboard_*` report unsupported. Set `GLASS_ANDROID_AGENT=off` to force the `adb` paths even
when the jar is present.

## Optional: on-device a11y service (Compose-rich tree + high-fidelity `set_value`)

A second optional companion — also from
[glass-android-agent](https://github.com/fixed-width/glass-android-agent) — sharpens semantic
addressing. `glass_a11y_snapshot` works over plain `adb` via `uiautomator`, but `uiautomator` tends to
flatten Jetpack Compose UIs, and `glass_set_value` falls back to keystroke simulation. The on-device
**AccessibilityService** reads the live `AccessibilityNodeInfo` tree (so Compose semantics come
through) and sets editable fields via the real `ACTION_SET_TEXT`.

Point `GLASS_ANDROID_A11Y_APK` at its `glass-a11y.apk`:

- Download the prebuilt APK from the agent repo's
  [Releases](https://github.com/fixed-width/glass-android-agent/releases), or
- build it yourself: `./gradlew :a11y:assembleDebug` in the agent repo.

glass installs the APK, enables the service, connects, and restores the device's prior accessibility
state on teardown — all automatically. Without it, glass uses the `uiautomator` reader. Set
`GLASS_ANDROID_A11Y=off` to force `uiautomator` even when the APK is present.

The service backs the **accessibility tree + `glass_set_value`**. Element *clicks* stay coordinate taps
(precise, using the service's bounds) — Android's `ACTION_CLICK` is unreliable on Compose, so glass
doesn't route clicks through it.

## Check the setup

```bash
GLASS_BACKEND=android glass-mcp doctor
# or with --deep to actually launch + ping the agent:
GLASS_BACKEND=android glass-mcp doctor --deep
```

Reports `adb`, the emulator + AVDs, the online/attachable device, and the agent + a11y-service status.
Then [connect your agent](connect-an-agent.md).
