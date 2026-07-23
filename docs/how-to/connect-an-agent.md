# Connect glass to your agent

glass speaks the Model Context Protocol, so you register it with your MCP client once and your agent
gains the glass tools. glass works with any MCP client; the examples below use Claude Code and a
generic JSON config. For the verified-host list and what glass needs from a host, see
[Host compatibility](../reference/host-compatibility.md). There are two transports — pick by how
glass is running.

- If glass runs on the **same machine** as your agent (the Linux and Windows default), use **stdio**.
- If glass runs as a **network server** (the macOS menu-bar LaunchAgent, or the agent and app on
  different machines), use **HTTP**.

Once connected, your agent has the tools in [reference/tools.md](../reference/tools.md); run the
`glass_doctor` tool to confirm the environment is ready. To have your agent drive glass well from the
first turn, also install the [glass-drive skill](drive-glass-well.md).

## Over stdio

Register the binary; your client spawns it and talks over stdin/stdout.

**Claude Code:**

```bash
claude mcp add glass --scope user -- /absolute/path/to/glass-mcp
```

On Windows, point at the `.exe`:

```powershell
claude mcp add glass --scope user -- "$env:USERPROFILE\bin\glass-mcp.exe"
```

**Generic MCP client (JSON config):**

```json
{
  "mcpServers": {
    "glass": {
      "command": "/absolute/path/to/glass-mcp"
    }
  }
}
```

**Antigravity:** open **Settings → Customizations → Open MCP Config** to edit its `mcp_config.json`,
add glass under `mcpServers` (the same shape as the generic config above), then reload the MCP
servers (or restart Antigravity).

**Codex CLI:** `codex mcp add glass -- /absolute/path/to/glass-mcp` — or add a `[mcp_servers.glass]`
table with `command` / `args` to `~/.codex/config.toml`.

No `env` block is needed: glass uses your host's default backend and, where the host supports it,
gives each session its own isolated display with nothing to set up. Add an `env` block only to
override a default — the specific variables are in [reference/environment.md](../reference/environment.md).

## Over HTTP

When glass runs as a network server (`glass-mcp serve --http`), register its URL instead of a command.

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

A loopback endpoint (`127.0.0.1`) needs no token. To reach glass from another machine, or to bind a
non-loopback address, follow [run-over-the-network.md](run-over-the-network.md) for the token and
tunnel setup.

## The macOS exception

On macOS, prefer the HTTP endpoint that the menu-bar LaunchAgent already serves (see
[setup-macos.md](setup-macos.md)). You *can* register the app's bundled binary over stdio:

```bash
claude mcp add glass --scope user -- \
  /Applications/GlassMcp.app/Contents/MacOS/glass-mcp
```

— but a stdio server is launched by, and attributed to, your MCP client, so the macOS permission
grants must attach to *that* process. The LaunchAgent model exists precisely so glass holds its own
grants; see [the permission model](../explanation/macos-permissions.md) for why.
