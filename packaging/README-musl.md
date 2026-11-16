# Glass — Linux x86-64 (static)

Glass is an MCP server for building, capturing, driving and debugging external native GUI apps.
This README accompanies the extracted release archive. Full documentation:
[Glass docs](https://github.com/fixed-width/glass/blob/master/docs/README.md).

## Requirements

This binary is statically linked and has no glibc or shared C++ runtime requirement.
External display, accessibility and containment tools are still required. Install your distribution's
CA certificates for HTTPS release checks and updates (`ca-certificates` on Alpine).

The default X11 backend creates a private headless display. Default launches also use bubblewrap
containment and a private accessibility bus. On Debian/Ubuntu:

```bash
sudo apt-get install -y xvfb bubblewrap dbus-bin at-spi2-core
```

Use the equivalent packages on your distribution. Bubblewrap needs unprivileged user namespaces;
`glass-mcp doctor` reports restrictions and remedies. A failed default sandbox refuses launch.
The explicit `sandbox:"off"` option disables containment; `a11y:false` skips the private bus for
pixel-only apps. Neither is needed for the normal setup.

## Install and connect

From the extracted directory:

```bash
mkdir -p ~/.local/bin
cp glass-mcp ~/.local/bin/glass-mcp
chmod +x ~/.local/bin/glass-mcp
~/.local/bin/glass-mcp doctor
```

Add `~/.local/bin` to `PATH` if it is not already there. Register the absolute binary path with your
MCP client. For a client using `mcpServers` JSON:

```json
{
  "mcpServers": {
    "glass": {
      "command": "/home/YOU/.local/bin/glass-mcp",
      "args": ["--tool-profile", "lean"]
    }
  }
}
```

Reload the MCP connection. Lean exposes actions through `glass_do`; the full profile adds
standalone action tools. Check your binary's inventory with `glass-mcp tools --json`.

Ask the agent to launch an installed X11 app and take a screenshot, then stop it. For example,
install `x11-apps` and ask it to launch `xclock`. The app runs on a private display; Glass does not
use your ambient `DISPLAY`. Set `GLASS_DISPLAY` only to attach a display you deliberately selected.

## Other configurations and updates

- [Linux setup](https://github.com/fixed-width/glass/blob/master/docs/how-to/setup-linux.md):
  Wayland needs a Glass-discoverable sway ≥ 1.12, Xwayland and Mesa software rendering.
- [Connect an agent](https://github.com/fixed-width/glass/blob/master/docs/how-to/connect-an-agent.md):
  client-specific stdio and resident HTTP configurations.
- [Network setup](https://github.com/fixed-width/glass/blob/master/docs/how-to/run-over-the-network.md):
  `serve --http`, bearer tokens and SSH tunnels. Non-loopback binds require a token; Glass does not provide TLS.

Release builds can use `glass-mcp update`. A running server needs restarting after a binary update.
`--no-default-features` builds omit network transport and self-update. For problems, run
`glass-mcp doctor` and report the version and failed checks in a
[GitHub issue](https://github.com/fixed-width/glass/issues).
