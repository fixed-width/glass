<!-- KEEP IN SYNC with the env handling in `crates/glass-mcp` (and `glass-mcp env`, which prints the
     live purpose/default/current value for every variable). -->

# Environment variables

Every `GLASS_*` variable glass reads, grouped by concern. Each variable has a sensible default, so a
default install needs none of them; set one only to override a default. `glass-mcp env` prints this
same list with each variable's current value (see [reference/cli.md](cli.md)); the token
(`GLASS_TOKEN`) is reported only as `set`/`(unset)`, never printed.

Most variables also have an equivalent per-call argument on `glass_start` (`backend`, `sandbox`) —
the argument wins when both are present.

## Backend & display

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_BACKEND` | Default backend when `glass_start`'s `backend` is omitted | `windows` on a Windows host, `macos` on a macOS host, else `x11` | all |
| `GLASS_DISPLAY` | X11 display target: unset = spawn a private headless `Xvfb`; `:N` = attach to a display you manage; `:0` = drive your real desktop | unset (private `Xvfb`) | X11 |
| `GLASS_XVFB` | `Xvfb` binary | `Xvfb` (on `PATH`) | X11 |
| `GLASS_XVFB_SCREEN` | Private `Xvfb` screen geometry | `1280x800x24` | X11 |
| `GLASS_SWAY` | `sway` binary; forces this one and skips discovery (fails closed if wrong) | auto-discovered | Wayland |
| `GLASS_WAYLAND_SCREEN` | Headless sway output size (`WxH`, no depth field) | `1280x800` | Wayland |

Backend selection and the display-isolation model are explained in
[explanation/backends.md](../explanation/backends.md). On Android the emulator resolves via the
standard `ANDROID_SDK_ROOT` / `ANDROID_HOME` (see the Android group below).

## Containment

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_SANDBOX` | Default containment level: `default`, `strict`, or `off` | `default` | all |
| `GLASS_BWRAP` | bubblewrap binary | `bwrap` (on `PATH`) | Linux |
| `GLASS_WIN_SANDBOX_PROVIDER` | Windows containment provider: `auto`, `sandboxie`, or `none` | `auto` | Windows |
| `GLASS_SANDBOXIE_DIR` | Sandboxie install directory | `%ProgramFiles%\Sandboxie` | Windows |

`default` and `strict` are fail-closed; the levels and per-OS mechanisms are explained in
[explanation/containment.md](../explanation/containment.md).

## Build & input

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_SH` | Shell used to run `glass_start`'s `build` command | `sh` (on `PATH`) | all |
| `GLASS_TYPE_DWELL_MS` | Per-key dwell for synthetic typing, to stay ahead of the OS input-pipeline race; raise on a slow/loaded host, lower for speed | `60` | Windows |

## Android

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_ADB` | `adb` binary (full path recommended on Windows) | `adb` (on `PATH`) | Android |
| `GLASS_AVD` | Which AVD to boot (needed only with more than one AVD) | first/only AVD | Android |
| `GLASS_ANDROID_SERIAL` | Which running emulator to attach to (when several are online) | — | Android |
| `GLASS_ANDROID_LIFECYCLE` | Set `attach` to force attach-only (never boot an AVD) | attach-or-boot | Android |
| `GLASS_EMULATOR` | `emulator` binary (else resolved via `ANDROID_SDK_ROOT` / `ANDROID_HOME`) | SDK-resolved | Android |
| `GLASS_EMULATOR_ARGS` | Extra flags passed when glass boots an emulator | — | Android |
| `GLASS_EMULATOR_KEEP` | Keep a glass-booted emulator alive past shutdown | off | Android |
| `GLASS_ANDROID_AGENT_JAR` | Override path to `glass-agent.jar` (on-device agent: clipboard + high-fidelity input) | auto-discovered, else off | Android |
| `GLASS_ANDROID_AGENT` | Set `off` to force the `adb` input path even when the jar is present | on when jar set | Android |
| `GLASS_ANDROID_A11Y_APK` | Override path to `glass-a11y.apk` (on-device AccessibilityService) | auto-discovered, else uiautomator | Android |
| `GLASS_ANDROID_A11Y` | Set `off` to force `uiautomator` even when the APK is present | on when APK set | Android |

The companion files are auto-discovered next to the `glass-mcp` binary or in glass's data dir, so the
easiest setup is to drop `glass-agent.jar` / `glass-a11y.apk` there; the `*_JAR` / `*_APK` vars above only
override that with an explicit path. Full setup is in [how-to/setup-android.md](../how-to/setup-android.md).

## iOS

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_IOS_UDID` | Exact Simulator UDID to drive when several are available | the newest booted/available iPhone simulator | iOS |
| `GLASS_IOS_DEVICE` | Device name to boot when none is running, e.g. `iPhone 17` (ignored if `GLASS_IOS_UDID` is set) | the newest available iPhone simulator | iOS |
| `GLASS_SIMULATOR_KEEP` | Leave a glass-booted iOS Simulator running at shutdown instead of stopping it | stop it | iOS |

Attach-or-boot works the same way as the Android group above: glass attaches to an already-booted
Simulator, or boots the newest available iPhone simulator itself. Full setup is in
[how-to/setup-ios.md](../how-to/setup-ios.md).

## Network transport

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_TOKEN` | Bearer token for the HTTP transport (alternative to `--token-file`); reported only as `set`/`(unset)`, never printed | unset | all |

Binding a non-loopback address without a token is refused (fail-closed). See
[how-to/run-over-the-network.md](../how-to/run-over-the-network.md).

## Audit log

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_AUDIT_LOG` | Path to append a JSONL audit record per actuation (also `--audit-log <path>`) | off | all |
| `GLASS_AUDIT_CONTENT` | Content mode: `redacted`, `full`, or `none` | `redacted` | all |
| `GLASS_AUDIT_PREFIX_LEN` | Length of the plaintext content prefix (`0` disables it) | `8` | all |

The record schema and redaction model are in [reference/audit-log.md](audit-log.md).
