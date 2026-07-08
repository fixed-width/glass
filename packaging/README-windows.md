# glass — Windows (x86-64)

**glass** is an MCP server that lets an AI coding agent drive native GUI apps: it
launches an app, screenshots what's on screen, clicks and types into it, reads its
logs, and detects visual changes — so the agent can build and debug a GUI on its own
instead of asking you "does this look right?".

This is the **Windows x86-64** build. See the project README for the full picture:
<https://github.com/fixed-width/glass>.
For Windows-specific setup (containment runtime, network transport, Android-on-Windows),
see [docs/how-to/setup-windows.md](../docs/how-to/setup-windows.md).

---

## 1. System requirements

- **Windows 10 or 11, x86-64.** Nothing else to install: `glass-mcp.exe` statically
  links the Visual C++ runtime, so **no Visual C++ Redistributable is needed** (the
  Universal CRT it also relies on is a built-in Windows component). The
  capture/input/accessibility features use built-in Windows APIs
  (Windows.Graphics.Capture for screenshots, SendInput for input, UI Automation for
  the accessibility tree).
- glass drives apps on the **interactive desktop**, so run it in a normal logged-in
  session (not a non-interactive service / Session 0).
- **Containment:** glass **sandboxes every launched app by default**, and that default
  is *fail-closed* — with no in-OS provider available, `glass_start` errors rather than
  running the app unconfined. Install [Sandboxie Classic](https://sandboxie-plus.com/downloads)
  (with its service running) for containment, or set `GLASS_SANDBOX=off` to launch apps
  unconfined. `glass-mcp doctor`'s `sandbox` section reports this posture.

## 2. Install the binary

Copy `glass-mcp.exe` somewhere on your PATH, e.g. `%USERPROFILE%\bin`:

```powershell
mkdir $env:USERPROFILE\bin -Force
copy glass-mcp.exe $env:USERPROFILE\bin\glass-mcp.exe
```

## 3. Verify your setup

glass ships a built-in checker:

```powershell
glass-mcp doctor
```

You want the `[windows]` section all `✓` and the summary to say **OK** (exit code 0).
A `✗` prints the exact remedy.

## 4. Register it with your agent (MCP over stdio)

**Claude Code:**

```powershell
claude mcp add glass --scope user -- "$env:USERPROFILE\bin\glass-mcp.exe"
```

**Claude Desktop / a project `.mcp.json`:** use the absolute path —

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

## 5. Check it works

Restart your agent so it picks up the new MCP server, then ask it something like:

> "Use glass to launch `charmap.exe` and take a screenshot."

(Use a classic single-process app like `charmap` or `mspaint` for a first test —
some packaged Store apps, including the Windows 11 `notepad`, launch out-of-process
and hand off, which glass sees as the launched process exiting.)

The agent should get back an image of the app. The tools it now has include
`glass_start`, `glass_screenshot`, `glass_click`, `glass_type`, `glass_wait_stable`,
`glass_diff`, `glass_logs`, `glass_a11y_snapshot`, `glass_click_element`, and
`glass_doctor`.

## 6. Network transport — agent and app on different machines

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

---

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each failed
check. Bug reports and questions: <https://github.com/fixed-width/glass/issues>.
