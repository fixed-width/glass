# Measure interactions on native hosts

Use the [interaction runner](../../tools/interaction-bench/README.md) on the host being measured.
Build `glass-mcp` and the native fixture on that host before freezing a cohort. Cross-compilation
alone does not establish UI behavior. Python 3.9 or later is required.

For Linux Wayland, set `backend` to `wayland` and `sway` to an absolute sway executable path.
The runner creates a private session bus and Glass starts an owned headless compositor at `display`
size. Firefox receives `MOZ_ENABLE_WAYLAND=1`; Chromium receives `--ozone-platform=wayland`.
Use the same web cases and exact outcome checks as X11. Retain pointer-action failures even when
native form actions succeed on the same backend.

For macOS and Windows, select only `native-form` and configure the built native executable:

```json
{
  "backend": "macos",
  "cases": ["native-form"],
  "applications": {"native": {"executable": "/absolute/path/glass-interaction-native"}},
  "drivers": [{"id": "glass", "adapter": "glass", "command": ["/absolute/path/glass-mcp"]}]
}
```

On Windows use `backend: windows`, executable paths ending in `.exe`, and forward slashes in JSON
paths. Launch the runner in the logged-in interactive session, for example through an interactive
scheduled task. An SSH service session alone may not have access to the desktop.

On macOS launch from the logged-in GUI context, for example through a temporary LaunchAgent.
The executable's signing identity and its responsible launcher must have the required Screen
Recording and Accessibility permissions. A different Python installation can have a different
permission context. Record the actual interpreter and signed executable; check a single diagnostic
before scheduling repetitions. Use a separately built application bundle when an existing Glass
service is running. Clean up only the benchmark's temporary task or LaunchAgent afterward.

The desktop recipe resizes its native window to 600×500. macOS verifies an initial empty-value reset
through `glass_set_value`; entry then uses confirmed focus and typing on both hosts. Native runs use
owned processes in the interactive desktop and should execute serially on each host.

For iOS, build `examples/ios-role-fixture/build.sh` on a Mac with Xcode and an installed Simulator
runtime. Select `backend: ios`, `cases: ["ios-publication"]`, and configure:

```json
{
  "ios": {
    "app": "/absolute/path/RoleFixture.app",
    "runtime": "com.apple.CoreSimulator.SimRuntime.iOS-26-5",
    "device_type": "com.apple.CoreSimulator.SimDeviceType.iPhone-17",
    "companion": "/absolute/path/idb_companion"
  }
}
```

This is the value of `applications`, not a complete runner configuration. Select runtime and device
identifiers installed on the host. Allow enough attempt time for a fresh device to boot, for example
`attempt_timeout_ms: 360000`. The probe launches the native controls and web tabs, captures each,
and performs bounded field/button queries plus separate full snapshots. It does not type or cross
the native/WebView boundary by input. Missing publication is `unsupported`, never a semantic pass;
launch/capture failure is a failed probe. One diagnostic repetition can establish a platform
publication limit, but cannot establish repeatability or a performance distribution.

Copy the complete result directory back for portable `validate` and `summarize`. Preserve native
host results separately; do not pool hosts, configurations or publication probes into form timings.
