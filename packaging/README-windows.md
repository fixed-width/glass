# Glass — Windows x86-64

Glass is an MCP server for driving external native GUI apps. It needs Windows 10 version 1903
(build 18362) or later, or Windows 11, and a usable
signed-in graphical session. The executable statically links the Visual C++ runtime; no separate
Visual C++ Redistributable is needed.

Full instructions: [Windows setup](https://github.com/fixed-width/glass/blob/master/docs/how-to/setup-windows.md).

## Install and check

From the extracted release directory, copy the executable to a directory on your `PATH`, or use
its absolute path. For example:

```powershell
New-Item -ItemType Directory -Force "$env:USERPROFILE\bin"
Copy-Item glass-mcp.exe "$env:USERPROFILE\bin\glass-mcp.exe"
& "$env:USERPROFILE\bin\glass-mcp.exe" doctor
```

`%USERPROFILE%\bin` is not on `PATH` by default. Add it through Windows Environment Variables and
open a new terminal if you want to use `glass-mcp` as a short command.

Install Sandboxie Classic or Plus with its service running for default containment. Classic is
auto-detected at `%ProgramFiles%\Sandboxie`; use `GLASS_SANDBOXIE_DIR` for another location.
Without a provider, default/strict launches fail. `sandbox:"off"` explicitly runs an app unconfined.
Sandboxie virtualises writes; it does not hide files your account can read. Desktop choice is
separate from containment.

Release signing depends on the release build configuration. Inspect the downloaded executable's
signature if Windows displays a publisher or SmartScreen prompt. A distribution prompt is separate
from runtime access; Windows has no Glass-specific capture or input privacy grants.

## Prepare applications and connect the agent

```powershell
& "$env:USERPROFILE\bin\glass-mcp.exe" desktop open
```

In **Application setup…**, choose **On your desktop** or **Off your desktop…**, then **Continue setup**.
On-desktop input can move your pointer and change focus. The off-desktop preview uses your account
in a local child session and requires supported Windows hosting, **Start Environment…** and native
Windows sign-in. Setup reports **Ready** only after the selected environment is usable.

Copy the MCP endpoint, normally `http://127.0.0.1:7300/`, and register it with an HTTP-capable MCP
client. Keep Glass running in the system tray. Closing its window hides it; **Quit Glass** stops it.
**Start with Windows** is optional. An unprepared off-desktop environment refuses app work and never
falls back to your visible desktop.

Ask your agent to launch `charmap.exe`, take a screenshot, then stop the app. Packaged Store apps
can hand off to another process; a classic single-process app is a more predictable first check.

## Other configurations and updates

- [Agent connection](https://github.com/fixed-width/glass/blob/master/docs/how-to/connect-an-agent.md):
  HTTP examples and shared-desktop stdio compatibility with `--desktop shared`.
- [Separate-desktop preview](https://github.com/fixed-width/glass/blob/master/docs/how-to/windows-separate-desktop.md):
  readiness, viewer controls, recovery and qualification limits.
- [Network setup](https://github.com/fixed-width/glass/blob/master/docs/how-to/run-over-the-network.md):
  bearer tokens and tunnels. Non-loopback binds require a token; Glass does not provide TLS.

`glass-mcp update` updates a release executable; restart a running server afterwards. If capture or
input fails, run `glass-mcp doctor` in the same signed-in session as Glass. A bare SSH/service
session may have no usable desktop. A virtual monitor enables headless rendering but does not
isolate input. Report problems with the version and failed checks in a
[GitHub issue](https://github.com/fixed-width/glass/issues).
