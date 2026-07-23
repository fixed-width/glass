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
| `GLASS_SANDBOX_FLOOR` | Operator-enforced minimum containment level; raises an omitted request, refuses an explicit one below it | `off` (no floor) | all |
| `GLASS_BWRAP` | bubblewrap binary | `bwrap` (on `PATH`) | Linux |
| `GLASS_WIN_SANDBOX_PROVIDER` | Windows containment provider: `auto`, `sandboxie`, or `none` | `auto` | Windows |
| `GLASS_SANDBOXIE_DIR` | Sandboxie install directory | `%ProgramFiles%\Sandboxie` | Windows |
| `GLASS_CLIP_HOOK_DLL` | Private-clipboard hook DLL (`glass_clip_hook.dll`) injected into a Sandboxie-boxed app | next to `glass-mcp`, else Layer-2 clipboard isolation is unavailable | Windows |

`default` and `strict` are fail-closed; the levels and per-OS mechanisms are explained in
[explanation/containment.md](../explanation/containment.md).

## Rendering under containment (Linux)

When glass launches an app **under containment** (`sandbox` is not `off`) on Linux, it sets
software-render environment defaults so GPU / shared-memory rendering paths the sandbox blocks
don't leave the window black:

| Variable | Value | Toolkit |
|---|---|---|
| `GSK_RENDERER` | `cairo` | GTK4 |
| `QT_X11_NO_MITSHM` | `1` | Qt (X11 widgets) |
| `QT_QUICK_BACKEND` | `software` | Qt Quick / QML |

The sandbox isolates the SysV IPC namespace, so X11 MIT-SHM can't attach to glass's X server — and
GTK4's GL renderer, even the software fallback it lands on without a GPU, needs MIT-SHM to present,
so an unset default leaves the window black. These defaults select a renderer that presents without
MIT-SHM; each is ignored by toolkits that don't read it. Unlike the `GLASS_*` variables elsewhere in
this file, glass *sets* these for the launched app rather than reading them, so they don't appear in
`glass-mcp env`.

To override one — for example to force a GPU renderer against a display that has one — pass the
variable explicitly in `glass_start`'s `env`; an explicit value always wins. `sandbox: off` launches
receive none of these defaults.

## Build & input

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_SH` | Shell used to run `glass_start`'s `build` command | `sh` (on `PATH`) | all |
| `GLASS_TYPE_DWELL_MS` | Per-key dwell for synthetic typing, to stay ahead of the OS input-pipeline race; raise on a slow/loaded host, lower for speed | `60` | Windows |

## Linux accessibility

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_DBUS_DAEMON` | `dbus-daemon` binary for the private AT-SPI bus | `dbus-daemon` (on `PATH`) | Linux |
| `GLASS_ATSPI_LAUNCHER` | `at-spi-bus-launcher` binary; forces this one and skips discovery (fails closed if wrong) | auto-discovered (well-known install paths) | Linux |

Only used when a launch requests `a11y: true` (see [reference/tools.md](tools.md)): glass spawns a
private D-Bus session bus and AT-SPI bus per launch rather than touching your desktop's shared
accessibility bus.

## Android

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_ADB` | `adb` binary (full path recommended on Windows) | `adb` (on `PATH`) | Android |
| `GLASS_AVD` | Which AVD to boot (needed only with more than one AVD) | first/only AVD | Android |
| `GLASS_ANDROID_SERIAL` | Which running emulator to attach to (when several are online) | — | Android |
| `GLASS_ANDROID_LIFECYCLE` | Set `attach` to force attach-only (never boot an AVD) | attach-or-boot | Android |
| `GLASS_EMULATOR` | `emulator` binary (else resolved via `ANDROID_SDK_ROOT` / `ANDROID_HOME`) | SDK-resolved | Android |
| `GLASS_EMULATOR_ARGS` | Extra flags passed when glass boots an emulator | — | Android |
| `GLASS_EMULATOR_BOOT_TIMEOUT_MS` | Max wait for a booting emulator to reach `sys.boot_completed` | `120000` | Android |
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
| `GLASS_IOS_DEVICE` | Device name to boot when none is running, e.g. `iPhone 17` or `iPad Pro 13-inch` (ignored if `GLASS_IOS_UDID` is set) | the newest available iPhone simulator | iOS |
| `GLASS_SIMULATOR_KEEP` | Leave a glass-booted iOS Simulator running at shutdown instead of stopping it | stop it | iOS |
| `GLASS_IDB_COMPANION` | Path to the `idb_companion` binary (input + accessibility for the Simulator backend) | `idb_companion` (found on `PATH`) | iOS |

Attach-or-boot works the same way as the Android group above: glass attaches to an already-booted
Simulator, or boots the newest available iPhone simulator itself. `GLASS_IOS_DEVICE` names any
iOS-family simulator (iPhone or iPad); watchOS, tvOS, and visionOS simulators are never eligible,
whether attaching to an already-booted one or booting one by name. Full setup is in
[how-to/setup-ios.md](../how-to/setup-ios.md).

## macOS clipboard

| Variable | Purpose | Default | Scope |
|---|---|---|---|
| `GLASS_CLIP_SHIM_DYLIB` | Override discovery of the injected clipboard-isolation shim (`libglass_clip_shim_macos.dylib`) | auto-discovered (bundled `Frameworks/`, next to `glass-mcp`, or the build's target dir) | macOS |

Only affects `default`/`strict` containment on an injectable (non-hardened-runtime) app; see
[explanation/containment.md](../explanation/containment.md) for how the shim isolates the clipboard.

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
