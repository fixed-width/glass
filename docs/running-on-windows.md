Running glass on a Windows host.

← [Back to README](../README.md)

## System requirements

- **Windows 10 or 11, x86-64.** Nothing else to install: `glass-mcp.exe` statically
  links the Visual C++ runtime, so **no Visual C++ Redistributable is needed** (the
  Universal CRT it also relies on is a built-in Windows component). The
  capture/input/accessibility features use built-in Windows APIs
  (Windows.Graphics.Capture for screenshots, SendInput for input, UI Automation for
  the accessibility tree).
- glass drives apps on the **interactive desktop**, so run it in a normal logged-in
  session (not a non-interactive service / Session 0).

## Containment runtime

glass **sandboxes every launched app by default**, and that default is *fail-closed* —
with no in-OS provider available, `glass_start` errors rather than running the app
unconfined.

Install [Sandboxie](https://sandboxie-plus.com/downloads) — **Classic** (GPLv3, free)
or **Plus** — with its service running. Classic is the default and is auto-detected at
`%ProgramFiles%\Sandboxie`. Plus installs to a different directory (e.g.
`%ProgramFiles%\Sandboxie-Plus`), so auto-detection won't find it — set
**`GLASS_SANDBOXIE_DIR`** to its install directory explicitly. Plus's commercial
"Business Certificate" is required for some use cases.

`GLASS_WIN_SANDBOX_PROVIDER=auto|sandboxie|none` (default `auto`) and
`GLASS_SANDBOXIE_DIR` (default `%ProgramFiles%\Sandboxie`) are configurable. Like
Linux, `default`/`strict` are **fail-closed**: if Sandboxie is absent or its service
not running (or `provider=none`), `glass_start` errors — `off` is the explicit escape
hatch.

Set `GLASS_SANDBOX=off` to launch apps unconfined (no Sandboxie required).

## Install the binary

Copy `glass-mcp.exe` somewhere on your PATH, e.g. `%USERPROFILE%\bin`:

```powershell
mkdir $env:USERPROFILE\bin -Force
copy glass-mcp.exe $env:USERPROFILE\bin\glass-mcp.exe
```

## Verify your setup

```powershell
glass-mcp doctor
```

You want the `[windows]` section all `✓` and the summary to say **OK** (exit code 0).
A `✗` prints the exact remedy.

## Register it with your agent (MCP over stdio)

**Claude Code:**

```powershell
claude mcp add glass --scope user -- "$env:USERPROFILE\bin\glass-mcp.exe"
```

**Claude Desktop / a project `.mcp.json`:**

```json
{
  "mcpServers": {
    "glass": {
      "command": "C:\\Users\\YOU\\bin\\glass-mcp.exe"
    }
  }
}
```

The windows backend is the default on a Windows host — no `env` block needed.

## Check it works

Restart your agent so it picks up the new MCP server, then ask it something like:

> "Use glass to launch `charmap.exe` and take a screenshot."

(Use a classic single-process app like `charmap` or `mspaint` for a first test —
some packaged Store apps, including the Windows 11 `notepad`, launch out-of-process
and hand off, which glass sees as the launched process exiting.)

The agent should get back an image of the app. The tools it now has include
`glass_start`, `glass_screenshot`, `glass_click`, `glass_type`, `glass_wait_stable`,
`glass_diff`, `glass_logs`, `glass_a11y_snapshot`, `glass_click_element`, and
`glass_doctor`.

## Network transport — agent and app on different machines

By default the agent spawns `glass-mcp.exe` directly (stdio). If the agent runs on one
machine and the Windows app runs on another, use the network transport instead:

| Use | Transport | How |
|---|---|---|
| Agent + app on the same machine (default) | stdio | Register `glass-mcp`; the client spawns it. Zero config. |
| Agent and app on different machines | network | Run `glass-mcp serve --http --addr …` on the Windows machine; point the client at the URL with the bearer token. |

**Token setup** (built-in CSPRNG):

```powershell
mkdir $env:USERPROFILE\.glass -Force
glass-mcp gen-token --out $env:USERPROFILE\.glass\token
glass-mcp serve --http --addr 0.0.0.0:7300 --token-file $env:USERPROFILE\.glass\token
```

The MCP client supplies the token as `Authorization: Bearer <token>` (check your
client's docs for the bearer-token / headers field). Binding a non-loopback address
without a token is refused at startup (fail-closed).

> **Token-file permissions on Windows.** Unlike Linux — where glass forces the token
> file to owner-only (`0600`) — on Windows the file inherits the **permissions of the
> folder you write it into**; glass does not yet set an explicit owner-only ACL. Keep
> `--out` inside your per-user profile (e.g. `%USERPROFILE%\.glass`), whose default
> permissions already restrict it to you, SYSTEM, and Administrators. **Don't** write
> the token to a shared, world-readable, or cloud-synced (e.g. OneDrive-backed) folder
> where other users could read it — or skip the file and pass the token via the
> `GLASS_TOKEN` environment variable instead.

**Secure over a trusted LAN via SSH tunnel** (Windows 10/11 ship the OpenSSH client):

```powershell
# On the agent's machine — forward the remote port to your local loopback:
ssh -L 7300:127.0.0.1:7300 user@windowsbox
# On the Windows machine — bind loopback only (no token needed for loopback):
glass-mcp serve --http
```

Then point the client at `http://127.0.0.1:7300`. The connection is encrypted by SSH;
glass itself does not own TLS.

## VM / strong-isolation tier

For stronger isolation beyond Sandboxie, run **glass-mcp itself inside a VM** alongside
the app it drives, and reach it from your host's AI agent over glass's existing
`serve --http` network transport. This moves the whole stack (glass-mcp, the app, and
its display) off your desktop into a separate VM, so nothing touches your interactive
session.

Two flavors (see [`packaging/windows-sandbox/README.md`](../packaging/windows-sandbox/README.md) for the templates):

**Windows Sandbox (`glass.wsb`)** — ephemeral, one-double-click. Requires Windows
Pro/Enterprise/Education, hardware virtualization, and the "Windows Sandbox" optional
feature. Only one Sandbox VM runs at a time; state is wiped on close.

**Managed VM (persistent)** — any Windows edition, on Hyper-V / VMware / QEMU / a
cloud instance. Install glass-mcp and the app inside the VM, run
`glass-mcp serve --http --addr 0.0.0.0:7300 --token-file <path>`. Persists across
reboots; supports multiple concurrent VMs.

Both reuse glass's network transport — no glass code changes required.

---

## Android on Windows

The Android backend is **host-OS-agnostic** — it shells out to `adb.exe`, so it runs
from a Windows host just as from Linux (macOS is planned — glass-mcp doesn't build on
macOS yet; see [running-on-macos.md](running-on-macos.md)).

### Install the Android SDK tools

Install `adb.exe` and `emulator.exe` from the Android SDK. Two routes:

**Via Android Studio** (canonical): the SDK manager installs `platform-tools` and
`emulator` automatically.

**Via the command-line tools** (headless): download the
[Android command-line tools](https://developer.android.com/studio#command-tools),
then:

```powershell
sdkmanager "platforms;android-34" "platform-tools" "emulator"
```

Point glass at `adb.exe` with **`GLASS_ADB`** (full path recommended on Windows):

```powershell
$env:GLASS_ADB = "$env:LOCALAPPDATA\Android\Sdk\platform-tools\adb.exe"
```

Set `ANDROID_SDK_ROOT` so glass can find the emulator alongside `adb`:

```powershell
$env:ANDROID_SDK_ROOT = "$env:LOCALAPPDATA\Android\Sdk"
```

### Managed AVD (attach-or-boot)

Like Android Studio, glass prefers to attach: if an emulator is already online it uses
it (**`GLASS_ANDROID_SERIAL`** picks one when several are running). If none is running,
glass boots a **headless** AVD itself and stops it on shutdown — choose it with
**`GLASS_AVD`** (needed only when you have more than one AVD). Force attach-only with
**`GLASS_ANDROID_LIFECYCLE=attach`**.

The `emulator` binary resolves from **`GLASS_EMULATOR`** / **`ANDROID_SDK_ROOT`** /
`ANDROID_HOME`; pass extra boot flags via **`GLASS_EMULATOR_ARGS`**; keep a
glass-booted emulator alive past shutdown with **`GLASS_EMULATOR_KEEP`**.

### Optional on-device agent (clipboard + high-fidelity input)

Over plain `adb`, glass types with `input text`/`keyevent` and can't reach the system
clipboard. A small companion — **[glass-android-agent](https://github.com/fixed-width/glass-android-agent)**,
a separate Apache-2.0 repo — closes both gaps: it runs on the device as a shell-uid
`app_process` server and gives glass real `MotionEvent`/`KeyEvent` injection (faithful
Unicode) and clipboard get/set.

Point **`GLASS_ANDROID_AGENT_JAR`** at its `glass-agent.jar`:

- Download the prebuilt jar from the agent repo's [Releases](https://github.com/fixed-width/glass-android-agent/releases).
- Or build it yourself: `./gradlew dex` in the agent repo produces `glass-agent.jar`.

glass pushes, launches, and tears the agent down for you. Without it, glass uses the
`adb` input path and `glass_clipboard_*` report unsupported. Set
**`GLASS_ANDROID_AGENT=off`** to force the `adb` paths even when the jar is present.

### Check the setup

```powershell
$env:GLASS_BACKEND = "android"
glass-mcp doctor
# or with --deep to actually launch + ping the agent:
glass-mcp doctor --deep
```

Reports `adb`, the emulator + AVDs, the online/attachable device, and the agent status.

---

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each failed
check. Bug reports and questions: <https://github.com/fixed-width/glass/issues>.
