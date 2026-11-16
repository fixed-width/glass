# Glass — macOS universal

Glass is an MCP server for driving external native GUI apps. This archive contains the signed,
notarised `GlassMcp.app` for macOS 14 or later, with arm64 and x86-64 binaries. Apple Silicon is
verified; Intel Macs are not yet verified.

Full instructions: [macOS setup](https://github.com/fixed-width/glass/blob/master/docs/how-to/setup-macos.md).

## Install and choose where apps run

Drag **GlassMcp.app** to **/Applications**, then open it. **Application setup…** offers:

- **On your desktop**: Glass uses your visible Aqua login; input can change focus and move your pointer.
- **Off your desktop…**: a preview using a separate standard macOS account with a real graphical login.
  Follow the wizard for account creation or reuse, initial macOS login, privacy access and
  **Start Environment…**. Desktop startup uses an already enabled Screen Sharing service and local
  account credentials. Glass does not save the account password. Initial login and missing grants
  require user action; startup after reboot is user-assisted.

Both choices need **Accessibility** and **Screen Recording** access for the signed Glass app.
Use **Open Settings…** beside a missing permission; approve it from your normal account. If Glass
is absent, add `/Applications/GlassMcp.app` with **+** in that privacy pane.

In the shared-desktop permission checklist, choose **Re-check** to relaunch after granting access,
or accept macOS's **Quit & Reopen**. During the separate-account wizard, keep the wizard open and
choose **Later** if macOS offers to quit; its fresh worker checks detect grants without restarting
setup. The same signed app reuses approvals across accounts and compatible updates.

## Connect and verify

The menu-bar app serves MCP at `http://127.0.0.1:7300/` by default. Keep Glass running and register
that URL with your agent. Copy the actual endpoint from the **glass ●** menu if it differs.
Loopback needs no bearer token. See
[Connect your agent](https://github.com/fixed-width/glass/blob/master/docs/how-to/connect-an-agent.md#over-http).

For a first check, ask the agent to launch your development app, take a screenshot and stop it.
Apps run under Seatbelt containment by default. Apple's system apps may require LaunchServices
and an explicit `sandbox:"off"` request; a failed contained launch does not run them unconfined.

The CLI lives inside the bundle:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp status
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp doctor
```

The resident menu-bar server holds its own privacy grants; a command run from Terminal can have a
different responsible process. Diagnose the running service through your agent's `glass_doctor` too.

## Stop, update or uninstall

Use **Quit glass** to stop the resident app and owned work. It starts again at your next login or
when you reopen Glass. `glass_stop` ends application work and retains an off-desktop environment.
For recovery and desktop changes, open **glass → Application setup…**.

To update, download and install the newer `.dmg`; `glass-mcp update --check` can check versions,
but macOS has no in-place binary update. Reconnect clients after replacing the app.

Choose **Uninstall glass…** or run the bundled binary's `uninstall` command before deleting the app.
This removes Glass startup/worker integration after cleanup. It retains the development account,
home, app data and privacy grants. Then move **GlassMcp.app** to the Trash. To keep Glass and return
to shared-desktop operation, use `unenroll` instead.

See [Manage the macOS environment](https://github.com/fixed-width/glass/blob/master/docs/how-to/manage-macos-environment.md)
for exact recovery/removal steps and [GitHub issues](https://github.com/fixed-width/glass/issues) for help.
