# Set up glass on Windows

The Windows backend drives apps on the **interactive desktop** (Windows.Graphics.Capture, SendInput,
UI Automation), so run it in a normal logged-in session — not a non-interactive service / Session 0.
It is the default backend on a Windows host, so no `env` block is needed to select it.

## System requirements

Windows 10 or 11, x86-64. Nothing else to install to *run* the binary: `glass-mcp.exe` statically
links the Visual C++ runtime, so **no Visual C++ Redistributable is needed** (the Universal CRT is a
built-in Windows component). Full requirements are in [reference/platforms.md](../reference/platforms.md).

## Containment runtime

glass sandboxes every launched app by default, fail-closed — with no provider available, `glass_start`
errors rather than running the app unconfined (see
[explanation/containment.md](../explanation/containment.md)).

Install [Sandboxie](https://sandboxie-plus.com/downloads) — **Classic** (GPLv3, free) or **Plus** —
with its service running. Classic is the default and is auto-detected at `%ProgramFiles%\Sandboxie`.
Plus installs elsewhere (e.g. `%ProgramFiles%\Sandboxie-Plus`), so set `GLASS_SANDBOXIE_DIR` to its
install directory. `GLASS_WIN_SANDBOX_PROVIDER` (`auto`|`sandboxie`|`none`) selects the provider. Set
`GLASS_SANDBOX=off` to launch apps unconfined (no Sandboxie required) — unless an operator has set
`GLASS_SANDBOX_FLOOR` to forbid `off` on this host, in which case an `off` request is refused.

## Install the binary

Download `glass-mcp-*-x86_64-windows.zip` from the
[Releases page](https://github.com/fixed-width/glass/releases/latest) and extract it. (The full asset
list is in [reference/platforms.md](../reference/platforms.md#release-artifacts).)

Copy `glass-mcp.exe` somewhere on your PATH, e.g. `%USERPROFILE%\bin`:

```powershell
mkdir $env:USERPROFILE\bin -Force
copy glass-mcp.exe $env:USERPROFILE\bin\glass-mcp.exe
```

`%USERPROFILE%\bin` is not on `PATH` by default, so `glass-mcp` won't be found afterwards —
add the directory via Windows' Environment Variables settings and open a new terminal, or
copy `glass-mcp.exe` into a directory that's already on `PATH` instead.

If you want to hack on glass or are on an architecture with no published asset,
[build from source](build-from-source.md) instead.

> **First run — SmartScreen.** The prebuilt `glass-mcp.exe` is Authenticode-signed (publisher: **Fixed
> Width LLC**). Windows may still show Microsoft Defender SmartScreen's "Windows protected your PC" on
> first download until the certificate builds reputation; if it does, click **More info → Run anyway**.
> This is a publisher-trust prompt, not a permission request — glass needs no permission grants on Windows
> ([why](../explanation/windows-permissions.md)).

## Verify

```powershell
glass-mcp doctor
```

Want the `[windows]` section all `✓` and the summary **OK** (exit code 0). A `✗` prints the exact
remedy.

## Connect and test

[Register glass with your agent](connect-an-agent.md) over stdio, restart the agent, then ask it
something like:

> "Use glass to launch `charmap.exe` and take a screenshot."

Use a classic single-process app like `charmap` or `mspaint` for a first test — some packaged Store
apps (including the Windows 11 `notepad`) launch out-of-process and hand off, which glass sees as the
launched process exiting.

## Headless capture — a virtual display driver

glass captures the interactive console session the GPU composes. A box with a physical monitor (or a
dummy/headless display plug) already has a display to capture; a box with **no monitor** — a CI runner,
a server, a remote VM — may have no composited display, so WGC has nothing to grab.

An **indirect-display (IddCx) driver** fixes this by adding a *virtual monitor* to the interactive
session. The recommended one is the community
[Virtual-Display-Driver](https://github.com/VirtualDrivers/Virtual-Display-Driver) (MttVDD; MIT,
signed). Install it (elevated PowerShell, after trusting its signing certificate per its README):

```powershell
pnputil /add-driver C:\path\to\MttVDD.inf /install
```

Once installed, the virtual monitor is part of the interactive session, so **glass picks it up
automatically** — no glass configuration.

> **Headless ≠ isolated.** A virtual display driver makes Windows headless-*capable*; it does **not**
> wall the app off your interactive session the way Linux's private `Xvfb`/`sway` does. For that, use
> the VM tier below.

## VM / strong-isolation tier

For isolation beyond Sandboxie, run **glass-mcp itself inside a VM** alongside the app it drives, and
reach it from your host's agent over glass's [network transport](run-over-the-network.md). Two flavours
(templates in [`packaging/windows-sandbox/README.md`](../../packaging/windows-sandbox/README.md)):

- **Windows Sandbox (`glass.wsb`)** — ephemeral, one-double-click. Requires Windows
  Pro/Enterprise/Education, hardware virtualization, and the "Windows Sandbox" optional feature. One
  Sandbox VM at a time; state wiped on close.
- **Managed VM (persistent)** — any Windows edition, on Hyper-V / VMware / QEMU / a cloud instance.
  Install glass-mcp and the app inside, run `glass-mcp serve --http --addr 0.0.0.0:7300 --token-file
  <path>`. Persists across reboots; supports multiple concurrent VMs.

## Network transport

To run the agent and app on different machines, see [run-over-the-network.md](run-over-the-network.md)
for the token and SSH-tunnel setup.

## Android

The Android backend runs from a Windows host too — see [setup-android.md](setup-android.md) (use the
`adb.exe` / `%LOCALAPPDATA%\Android\Sdk` paths noted there).
