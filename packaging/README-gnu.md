# glass — Linux x86-64 (glibc)

**glass** is an MCP server that lets an AI coding agent drive native GUI apps: it
launches an app, screenshots what's on screen, clicks and types into it, reads its
logs, and detects visual changes — so the agent can build and debug a GUI on its own
instead of asking you "does this look right?".

This is the **Linux x86-64 (glibc)** build. (A Windows build is also available; macOS
is not yet built.) See the project README for the full picture:
<https://github.com/fixed-width/glass>.

> **Prefer the static build if you can.** A statically-linked build is also available
> that needs no glibc version and no shared libs at all — only `xvfb`. Use this glibc
> build only if you specifically want it.

---

## 1. System requirements

- **Linux, x86-64**, with **glibc ≥ 2.39** (Ubuntu 24.04+, Debian 13+, Fedora 40+,
  or newer). Check yours:

  ```bash
  ldd --version | head -1
  ```

  If that prints **2.38 or lower**, this prebuilt binary won't run (you'll get a
  `version 'GLIBC_2.39' not found` error) — use the static build instead.

## 2. Install the prerequisite

The default (X11) backend spawns its **own private, headless display** — the only
thing to install is the headless X server:

```bash
sudo apt-get update && sudo apt-get install -y xvfb
```

(Equivalents: Fedora `sudo dnf install xorg-x11-server-Xvfb`; Arch
`sudo pacman -S xorg-server-xvfb`.)

## 3. Install the binary

```bash
mkdir -p ~/.local/bin
cp glass-mcp ~/.local/bin/glass-mcp
chmod +x ~/.local/bin/glass-mcp
```

## 4. Verify your setup

glass ships a built-in checker. Run it — it confirms everything glass needs and tells
you how to fix anything missing:

```bash
glass-mcp doctor
```

You want the `[x11]` section all `✓` and the summary to say **OK** (exit code 0). A
`✗` prints the exact remedy. (`--deep` additionally spawns + tears down the headless
display to prove it starts.)

## 5. Register it with your agent (MCP over stdio)

**Claude Code:**

```bash
claude mcp add glass --scope user -- ~/.local/bin/glass-mcp
```

**Claude Desktop / a project `.mcp.json`:** use the absolute path —

```json
{
  "mcpServers": {
    "glass": {
      "command": "/home/YOU/.local/bin/glass-mcp"
    }
  }
}
```

No `env` block is needed — glass spawns its own headless display automatically.

## 6. Network transport — agent and app on different machines

By default the agent spawns glass-mcp directly (stdio). If the agent runs on one
machine and the app runs on another, use the network transport instead:

| Use | Transport | How |
|---|---|---|
| Agent + app on the same machine (default) | stdio | Register `glass-mcp`; the client spawns it. Zero config. |
| Agent and app on different machines | network | Run `glass-mcp serve --http --addr …` on the app's machine; point the client at the URL with the bearer token. |

**Token setup** (built-in CSPRNG, cross-platform):

```bash
mkdir -p ~/.glass
glass-mcp gen-token --out ~/.glass/token
glass-mcp serve --http --addr 0.0.0.0:7300 --token-file ~/.glass/token
```

The MCP client supplies the token as `Authorization: Bearer <token>` (check your
client's docs for the bearer-token / headers field). Binding a non-loopback address
without a token is refused at startup (fail-closed).

**Secure over a trusted LAN via SSH tunnel** (no TLS required):

```bash
# On the agent's machine — forward the remote port locally:
ssh -L 7300:127.0.0.1:7300 user@appbox
# On the app's machine — bind loopback only (no token needed for loopback):
glass-mcp serve --http
```

Then point the client at `http://127.0.0.1:7300`. The connection is encrypted by
SSH; glass itself does not own TLS.

## 7. Check it works

Restart your agent so it picks up the new MCP server, then ask it something like:

> "Use glass to launch `xterm` and take a screenshot."

(Need a quick test app? `sudo apt-get install -y x11-apps` gives you `xclock`,
`xeyes`, etc.)

The agent should get back an image of the app. The tools it now has include
`glass_start`, `glass_screenshot`, `glass_click`, `glass_type`, `glass_wait_stable`,
`glass_diff`, `glass_logs`, `glass_list_windows`, `glass_select_window`, and
`glass_doctor`.

---

## Optional: watch what glass is doing (VNC)

By default the display is private and invisible. To watch it live, point glass at a
display you run yourself, then VNC into it:

```bash
Xvfb :42 -screen 0 1280x800x24 &          # a display you control
x11vnc -display :42 &                       # then connect any VNC viewer to it
```

Register glass with `GLASS_DISPLAY=:42`:

```bash
claude mcp add glass --scope user --env GLASS_DISPLAY=:42 -- ~/.local/bin/glass-mcp
```

glass **never** reads your ambient `$DISPLAY`, so it can't accidentally grab your real
desktop — set `GLASS_DISPLAY=:0` only if you deliberately want it to drive your live
session.

## Optional: the Wayland backend

glass also has a Wayland (wlroots/sway) backend, selectable per launch
(`backend: "wayland"`) or by default with `GLASS_BACKEND=wayland`. It needs a
**sway ≥ 1.12** it can discover plus Mesa software GL (`apt install libegl1
libgl1-mesa-dri`); most distros don't ship sway 1.12 yet, so build one with
**<https://github.com/fixed-width/sway-build>**. `GLASS_BACKEND=wayland glass-mcp doctor`
verifies it. The X11 default is the place to start.

---

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each failed
check. Bug reports and questions: <https://github.com/fixed-width/glass/issues>.
