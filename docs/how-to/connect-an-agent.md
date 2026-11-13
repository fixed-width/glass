# Connect glass to your agent

glass speaks the Model Context Protocol, so you register it with your MCP client once and your agent
gains the glass tools. glass works with any MCP client; the examples below use Claude Code and a
generic JSON config. For the verified-host list and what glass needs from a host, see
[Host compatibility](../reference/host-compatibility.md). There are two transports — pick by how
glass is running.

- If your client launches the binary directly (the Linux default or Windows shared-desktop
  compatibility), use **stdio**.
- If glass runs as a **resident desktop app** (the macOS menu-bar app or Windows system-tray
  tray app), or the agent and app run on different machines, use **HTTP**.

For the [Windows separate-desktop preview](windows-separate-desktop.md), open Glass with
`glass-mcp desktop open`, choose **Off your desktop…** in Application setup, then **Start Environment…**,
and wait for **Ready**. **On your desktop** uses the same tray app and HTTP URL. Register its
HTTP URL even when the agent runs on that same PC. The agent connects to the running tray app;
it does not launch another Glass process or handle Windows sign-in. The saved URL remains the same
across desktop starts and MCP client reconnects. A stdio-only client needs a relay; the current
Windows tray implementation does not include one.

Once connected, your agent has the tools in [reference/tools.md](../reference/tools.md); run the
`glass_doctor` tool to confirm the environment is ready. To have your agent drive glass well from the
first turn, also install the [glass-drive skill](drive-glass-well.md).

## Over stdio

Register the binary; your client spawns it and talks over stdin/stdout.

**Claude Code:**

```bash
claude mcp add glass --scope user -- /absolute/path/to/glass-mcp --tool-profile lean
```

For Windows shared-desktop operation, point at the `.exe`:

```powershell
claude mcp add glass --scope user -- "$env:USERPROFILE\bin\glass-mcp.exe" --tool-profile lean
```

**Generic MCP client (JSON config):**

```json
{
  "mcpServers": {
    "glass": {
      "command": "/absolute/path/to/glass-mcp",
      "args": ["--tool-profile", "lean"]
    }
  }
}
```

**Antigravity:** open **Settings → Customizations → Open MCP Config** to edit its `mcp_config.json`,
add glass under `mcpServers` (the same shape as the generic config above), then reload the MCP
servers (or restart Antigravity).

These new configurations select the 18-tool lean inventory. Lean uses `glass_do` for actions,
including single steps and gestures, `glass_screenshot` annotations for numbered elements, and
`glass_logs` with `wait` for log predicates. Use `--tool-profile full` for standalone tools. See
[tool profiles](../reference/tools.md#tool-profiles) for the included tools and wait semantics.

**Codex CLI:** `codex mcp add glass -- /absolute/path/to/glass-mcp --tool-profile lean` — or add a `[mcp_servers.glass]`
table with `command` / `args` to `~/.codex/config.toml`.

**GitHub Copilot:** add glass under `servers` in the client's MCP config — VS Code via *MCP: Open
User Configuration*, JetBrains via the plugin's `mcp.json` — with `"type": "stdio"`, `"command"`,
`"args"`. You may need to restart the IDE after adding it.

No `env` block is needed: glass uses your host's default backend and, where the host supports it,
gives each session its own isolated display with nothing to set up. Add an `env` block only to
override a default — the specific variables are in [reference/environment.md](../reference/environment.md).

## Over HTTP

When glass runs as a network server (`glass-mcp serve --http`), register its URL instead of a command.
The Windows tray app (`glass-mcp desktop open` or `serve --tray`) serves the same HTTP transport.

For a manually launched HTTP server, use `glass-mcp serve --http --tool-profile lean`.
The client uses the inventory selected by the server; an HTTP registration cannot change it.

Start Glass before connecting the agent. If Glass is not running, the connection fails; start it
and reconnect or reload the MCP connection as required by your client. If the Windows tray is
running but its desktop is not prepared, MCP connects and application requests return
`environment_not_ready` until you complete preparation in Glass.

**Claude Code:**

```bash
claude mcp add --transport http glass http://127.0.0.1:7300/
```

**Generic MCP client (JSON config):**

```json
{
  "mcpServers": {
    "glass": { "type": "http", "url": "http://127.0.0.1:7300/" }
  }
}
```

**Codex CLI:** `codex mcp add glass --url http://127.0.0.1:7300/`.

**GitHub Copilot:** the same `servers` entry with `"type": "http"` and `"url"`.

A loopback endpoint (`127.0.0.1`) needs no token. To reach glass from another machine, or to bind a
non-loopback address, follow [run-over-the-network.md](run-over-the-network.md) for the token and
tunnel setup.

## The macOS exception

For shared-desktop operation on macOS, prefer the HTTP endpoint that the menu-bar LaunchAgent serves (see
[setup-macos.md](setup-macos.md)). You *can* register the app's bundled binary over stdio:

```bash
claude mcp add glass --scope user -- \
  /Applications/GlassMcp.app/Contents/MacOS/glass-mcp --tool-profile lean
```

— but a stdio server is launched by, and attributed to, your MCP client, so the macOS permission
grants must attach to *that* process. The LaunchAgent model exists precisely so glass holds its own
grants; see [the permission model](../explanation/macos-permissions.md) for why.

With the [separate-account preview](setup-macos.md#apps-off-your-desktop-preview), the grant-bearing
worker is already a Glass LaunchAgent in the development account. Your ordinary MCP server can use
stdio; its parent's privacy grants do not determine the private worker's permissions. Run the MCP
client in your normal account and keep the enrolled development account logged in.
