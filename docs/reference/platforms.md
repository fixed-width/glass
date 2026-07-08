<!-- KEEP IN SYNC with the code (and README.md's compact matrix, and CLAUDE.md) whenever capabilities change. -->

# Platform support

Where glass stands by OS. **✓** supported · **◑** partial · **–** not supported · **🚧** planned.

| Capability | Linux (X11 + Wayland) | Windows | Android (AVD) | iOS (Simulator) | macOS |
|---|:--:|:--:|:--:|:--:|:--:|
| Capture · input · windows · clipboard · logs | ✓ | ✓ | ✓ † | ◑ § | ✓ ‡ |
| Accessibility (semantic addressing) | ✓ AT-SPI | ✓ UI Automation | ✓ UIAutomator | – | ✓ AX |
| Containment / sandboxing | ✓ bubblewrap | ✓ Sandboxie Classic | ✓ the emulator VM | ✓ the Simulator | ✓ ‡ |
| Display isolation (app off your desktop) | ✓ headless Xvfb / sway | ◑ virtual display · VM tier | ✓ headless emulator | ✓ headless simctl boot | 🚧 |

**Transport:** MCP over **stdio** (default, all platforms) or **network HTTP** (`glass-mcp serve
--http`, all platforms) — the network transport is behind the default-on `network` cargo feature (a
`--no-default-features` build is stdio-only).

Per-tool platform notes live in [reference/tools.md](tools.md); the mechanisms behind each column are
explained in [explanation/backends.md](../explanation/backends.md) and
[explanation/containment.md](../explanation/containment.md). Setup is per host:
[Linux](../how-to/setup-linux.md) · [Windows](../how-to/setup-windows.md) ·
[macOS](../how-to/setup-macos.md) · [Android](../how-to/setup-android.md) ·
[iOS](../how-to/setup-ios.md).

## System requirements

- **Linux** — X11 needs `Xvfb`; Wayland needs a discoverable `sway ≥ 1.12 / wlroots ≥ 0.20` plus Mesa
  software GL. Containment needs bubblewrap with unprivileged user namespaces. See
  [how-to/setup-linux.md](../how-to/setup-linux.md).
- **Windows** — Windows 10 or 11, x86-64. No Visual C++ Redistributable needed (the binary statically
  links the VC++ runtime; the Universal CRT is built in). Drives apps on the interactive desktop, so
  it needs a logged-in session. No permission grants are required — see
  [explanation/windows-permissions.md](../explanation/windows-permissions.md). Setup:
  [how-to/setup-windows.md](../how-to/setup-windows.md).
- **macOS** — macOS 14 or later, developed and tested on Apple Silicon; the shipped `.dmg` is a
  universal binary (arm64 + x86_64), but Intel Macs aren't yet verified. Drives the logged-in Aqua
  session and is gated by the two TCC permissions. See [how-to/setup-macos.md](../how-to/setup-macos.md).
- **Android** — emulator-only; developed and tested against **Android 14 (API 34)**. The `adb` backend
  assumes no particular version; the optional on-device companions declare an Android 7.0 (API 24)
  floor. See [how-to/setup-android.md](../how-to/setup-android.md).
- **iOS** — Simulator-only; **macOS host required** (`xcrun`/`simctl` ship with Xcode). No particular
  iOS runtime version is assumed — whichever runtime Xcode has downloaded works; real iPhones are out
  of scope, matching the Android backend's emulator-only stance. See
  [how-to/setup-ios.md](../how-to/setup-ios.md).

## Notes

**† Android** is emulator-only. Capture, multi-window, input, and logs work over `adb`, and glass
manages the AVD (attach a running one, or boot a headless one). Clipboard, high-fidelity input, and
multi-touch gestures (`glass_gesture`) use the optional on-device agent, and an optional on-device
AccessibilityService sharpens the a11y tree (Compose) and `glass_set_value`. Without the agent, input
falls back to `adb`'s `input` (single-pointer only) and clipboard is unavailable; without the service,
a11y falls back to `uiautomator`. Window resize/move (apps are full-screen) and physical devices are
non-goals.

**§ iOS** is Simulator-only — macOS host required (`xcrun`/`simctl` ship with Xcode). Capture,
clipboard, and logs are implemented; the on-box path is exercised by a manual smoke test on a macOS
host with a booted Simulator (no CI wiring on the macOS runner yet, so only its host-independent
logic — device resolution, `simctl` argument construction, doctor checks — runs in CI). Window support
is geometry/focus only: resize and move are unsupported, since Simulator apps, like a real device, are
always fullscreen. Pointer/keyboard input and the accessibility tree are not implemented yet;
`glass_click`/`glass_type`/`glass_key` and the a11y tools return an unsupported error on this backend.
Containment has no separate glass-managed step, the same as Android — the Simulator's per-app data
container is the isolation boundary.

**‡ macOS** capture, input, windows, clipboard, and logs are built and CI-tested (ScreenCaptureKit
capture, CGEvent input, AXUIElement windows). Containment is Seatbelt (`sandbox_init`): filesystem and
process are contained at `default`/`strict`, and `strict` additionally blocks outbound network. Under
containment the clipboard is isolated **and working** for an app not built with Apple's hardened
runtime; a hardened-runtime app (App Store / notarized) returns `Unsupported`. Display isolation (the
app fully off your desktop) is planned. Details in
[explanation/containment.md](../explanation/containment.md).

## Status

The Linux feature set is implemented and tested across **both** Linux backends (X11 and
Wayland/wlroots). The **Windows** backend (WGC capture, SendInput, UI Automation) is built and
CI-tested. An **Android** backend drives native apps in an AVD emulator over `adb` — capture, input,
logcat, multi-window, a `uiautomator` accessibility tree, a managed AVD (attach-or-boot), and two
optional on-device companions (an agent for clipboard + high-fidelity input, and an
AccessibilityService for a Compose-rich a11y tree + high-fidelity `set_value`); built and unit-tested
in CI and validated on-device. An **iOS** backend drives native apps on an iOS Simulator over `xcrun
simctl` — capture, clipboard, logs, and a managed Simulator (attach-or-boot, matching the Android
emulator's model); window support is geometry/focus only, and pointer/keyboard input and the
accessibility tree are not implemented yet. Its host-independent logic is unit-tested in CI; the
on-box path against a real Simulator is exercised manually on a macOS host. The **macOS** backend
(ScreenCaptureKit capture, CGEvent input, AXUIElement windows/logs, an AXUIElement accessibility tree,
and Seatbelt process containment) is built and CI-tested.
