# Glass documentation

Choose a path by what you need: learn through a tutorial, complete a task with a how-to guide,
look up a contract in reference, or understand the model in an explanation.

These docs describe the source on this branch, including changes that may not be in your installed
release. Check `glass-mcp --version` and the [changelog](../CHANGELOG.md) when an option is missing.
The live tool schemas are available with `glass-mcp tools --json`.

Installing for the first time? Set up your platform below, then
[connect your agent](how-to/connect-an-agent.md). The tutorial follows a single Linux/X11 path.

> **Driving Glass with an agent?** Install the [glass-drive skill](how-to/drive-glass-well.md) so your
> agent arrives already knowing the cheap-verify loop, instead of spending its first turns
> rediscovering it.

## Tutorial — learn by doing

- [Your first drive](tutorial/first-drive.md) — build, capture, click and verify a test app (Linux/X11).

## How-to guides — get something done

**Set up Glass for your host**

- [Linux](how-to/setup-linux.md) — X11 and Wayland
- [Windows](how-to/setup-windows.md)
- [Use a separate Windows desktop](how-to/windows-separate-desktop.md) — preview, same account
- [macOS](how-to/setup-macos.md)
- [Manage the macOS environment](how-to/manage-macos-environment.md) — restart, recover, unenroll or uninstall
- [Audit and manually manage macOS account setup](how-to/audit-macos-account-setup.md)
- [Remove a macOS enrollment manually](how-to/remove-macos-enrollment-manually.md) — recovery when the app is unavailable
- [Android](how-to/setup-android.md) — an AVD emulator, from any host
- [iOS](how-to/setup-ios.md) — the Simulator, macOS host only

**Connect and drive**

- [Connect Glass to your agent](how-to/connect-an-agent.md) — stdio and HTTP
- [Drive Glass well — the glass-drive skill](how-to/drive-glass-well.md)
- [Run Glass over the network](how-to/run-over-the-network.md) — agent and app on different machines
- [Capture a smaller image](how-to/capture-a-smaller-image.md) — size bounds and native coordinate mapping
- [Record and export session evidence](how-to/record-session-evidence.md) — retain requested results for offline review
- [Record a workflow video](how-to/record-workflow-video.md) — named clips on every backend and rolling desktop history
- [Drive a native iOS app in the Simulator](how-to/drive-an-ios-app.md) — launch, drive, and verify an iOS app

**Contribute**

- [Build from source](how-to/build-from-source.md) — all platforms; macOS signing + LaunchAgent
- [Verify a change](how-to/verify-a-change.md) — the gates, and how to cover platform code your
  host cannot run
- [Run the macOS native diagnostics](how-to/run-macos-diagnostics.md) — event and owned-window inspection
- [Verify an MCP host](how-to/verify-an-mcp-host.md) — check a client's tools, images and resource reads
- [Benchmark and profile](how-to/benchmarking.md)
- [Measure what the verification loop costs](how-to/verification-cost.md) — semantic vs
  screenshot-every-step, round-trips and tokens
- [Measure application boundaries](how-to/measure-application-boundaries.md) — packaged Electron,
  Android native/WebView, native forms and transfer between two applications.
- [Measure repeated application interactions](how-to/measure-interactions.md) — web conformance,
  exact outcomes, preserved evidence and phase costs
- [Measure interactions on native hosts](how-to/measure-native-hosts.md) — Wayland, macOS, Windows and iOS publication probes

## Reference — look up a fact

- [Tools](reference/tools.md) — every `glass_*` tool, its parameters, and platform support
- [Host compatibility](reference/host-compatibility.md) — which MCP hosts are verified, and what Glass needs from any host
- [Stability and versioning](reference/stability.md) — the semver promise: what's covered from 1.0
- [Environment variables](reference/environment.md) — operator-facing `GLASS_*` configuration
- [CLI](reference/cli.md) — `glass-mcp` subcommands
- [macOS administrator access](reference/macos-administrator-access.md) — privileged commands, password handling and on-disk records
- [Platform support](reference/platforms.md) — the capability matrix and system requirements
- [Accessibility roles by platform](reference/a11y-roles.md) — which roles each backend can produce
- [Audit log](reference/audit-log.md) — the JSONL record schema and redaction
- [Session trace](reference/session-trace.md) — capture scope, bounds, format and offline commands

## Explanation — understand why

- [The build → see → interact → debug loop](explanation/the-loop.md)
- [Backends and display isolation](explanation/backends.md)
- [Containment and sandboxing](explanation/containment.md)
- [The macOS permission model](explanation/macos-permissions.md)
- [The Windows access model](explanation/windows-permissions.md)
- [Web content](explanation/web-content.md)
