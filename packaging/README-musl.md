# glass — Linux x86-64 (static)

**glass** is an MCP server that lets an AI coding agent drive native GUI apps: it
launches an app, screenshots what's on screen, clicks and types into it, reads its
logs, and detects visual changes — so the agent can build and debug a GUI on its own
instead of asking you "does this look right?".

This is the **statically-linked Linux x86-64** build: **no glibc version requirement**
and **no shared-library prerequisites** — it runs on essentially any x86-64 Linux.
(A prebuilt **Windows** build is also available — see
[`packaging/README-windows.md`](README-windows.md). macOS has no prebuilt binary yet;
build from source — see [docs/running-on-macos.md](../docs/running-on-macos.md).)
See the project README for the full picture: <https://github.com/fixed-width/glass>.
For Linux-specific display/compositor and containment setup, see
[docs/running-on-linux.md](../docs/running-on-linux.md).

---

## 1. Install the prerequisites

The default (X11) backend spawns its **own private, headless display** — you don't
run or configure anything, but the headless X server itself must be installed:

```bash
sudo apt-get update && sudo apt-get install -y xvfb
```

(Equivalents: Fedora `sudo dnf install xorg-x11-server-Xvfb`; Arch
`sudo pacman -S xorg-server-xvfb`.)

glass also **sandboxes every launched app by default** (via bubblewrap), and that
default is *fail-closed*: with no sandbox available it errors rather than running the
app unconfined. So also install bubblewrap, or set `GLASS_SANDBOX=off` to run apps
unconfined:

```bash
sudo apt-get install -y bubblewrap        # Fedora/Arch: bubblewrap
```

It also needs unprivileged user namespaces enabled; Ubuntu 23.10+ restricts them via
AppArmor — allow with `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0`.
`glass-mcp doctor` checks this and prints the exact remedy.

That's the entire dependency list.

## 2. Install the binary

```bash
mkdir -p ~/.local/bin
cp glass-mcp ~/.local/bin/glass-mcp
chmod +x ~/.local/bin/glass-mcp
```

## 3. Verify your setup

glass ships a built-in checker. Run it — it confirms everything glass needs and tells
you how to fix anything missing:

```bash
glass-mcp doctor
```

You want the `[x11]` section all `✓` and the summary to say **OK** (exit code 0). A
`✗` prints the exact remedy. (The `[wayland]`/`[macos]` sections only matter if you opt
into those backends.) `glass-mcp doctor --deep` goes further and actually spawns +
tears down the headless display to prove it starts.

## 4. Register it with your agent (MCP over stdio)

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

## 5. Network transport — agent and app on different machines

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

## 6. Check it works

Restart your agent so it picks up the new MCP server, then ask it something like:

> "Use glass to launch `xterm` and take a screenshot."

(Need a quick test app? `sudo apt-get install -y x11-apps` gives you `xclock`,
`xeyes`, etc.)

The agent should get back an image of the app. The tools it now has include
`glass_start`, `glass_screenshot`, `glass_click`, `glass_type`, `glass_wait_stable`,
`glass_diff`, `glass_logs`, `glass_list_windows`, `glass_select_window`, and
`glass_doctor` (the agent can run the setup check itself if something fails).

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
session. (`glass-mcp doctor` with `GLASS_DISPLAY` set will also verify that display is
reachable.)

## Optional: the Wayland backend

glass also has a Wayland (wlroots/sway) backend, selectable per launch
(`backend: "wayland"`) or by default with `GLASS_BACKEND=wayland`. It needs a
**sway ≥ 1.12** it can discover plus Mesa software GL:

```bash
sudo apt-get install -y libegl1 libgl1-mesa-dri
```

Most distros don't ship sway 1.12 yet; build one with the helper tool at
**<https://github.com/fixed-width/sway-build>** (`./build.sh && ./build.sh install`, which
installs to `~/.local/share/glass/sway/`). `GLASS_BACKEND=wayland glass-mcp doctor`
will tell you if it's all in place. The X11 default is the path to start with.
