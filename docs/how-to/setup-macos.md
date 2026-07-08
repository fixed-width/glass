# Set up glass on macOS

On macOS, end users install a **notarized `.dmg`** — no build, no signing, no Terminal. glass drives
the logged-in Aqua session, gated by two one-time macOS privacy grants. (Contributors and anyone
running an unreleased checkout build from source instead — see
[build-from-source.md](build-from-source.md).)

## Install the notarized `.dmg`

Every tagged release attaches a **notarized, universal** `GlassMcp.app` in a `.dmg` to the
[Releases page](https://github.com/fixed-width/glass/releases). Because it's signed and notarized,
macOS opens it without a Gatekeeper detour, and first-run is a double-click:

1. **Download** the `.dmg` from Releases and open it.
2. **Drag `GlassMcp.app` to `/Applications`.**
3. **Double-click `GlassMcp.app`.** A **permission checklist window** appears, listing **Accessibility**
   and **Screen Recording**, each with a ✓/○ status and its own **Open Settings** button.
4. Click **Open Settings** for each permission, **one at a time**. It adds `GlassMcp.app` to that
   permission's pane under **System Settings → Privacy & Security** and opens the pane so you can turn
   it on. Because the app is asking for *itself*, the grant lands on `GlassMcp.app` directly — no manual
   `＋`-add needed. Granting **Screen Recording relaunches `GlassMcp.app` automatically** (macOS quits
   and reopens it to pick up the grant) — that relaunch is expected, not an error. If you grant the two
   out of order, or a grant made elsewhere doesn't show up, click **Re-check**. Grants are **one-time**:
   keyed to the app's signed identity, they survive restarts and updates (see
   [the permission model](../explanation/macos-permissions.md)).
5. Once both are granted, glass relaunches into its ready state — the checklist doesn't reappear, and
   the **`glass ●`** menu bar shows instead. Along the way it **installs its LaunchAgent** (so it keeps
   serving across logins and restarts).

That's the whole setup. The MCP endpoint is `http://127.0.0.1:7300/` — head to
[Connect your agent](connect-an-agent.md#over-http).

## The menu-bar item

Once granted, `GlassMcp.app` runs as a **visible menu-bar app** — look for **`glass ●`** in the menu
bar. It starts automatically at login (via the LaunchAgent) and its dropdown shows:

- the **served endpoint** (or a notice if another glass is already bound to it),
- **Copy endpoint** — copy the MCP endpoint to your clipboard,
- **Restart** — restart the background agent (e.g. after a fresh grant),
- **Quit glass** — stop the agent,
- **Uninstall glass…** — stop glass from starting at login and quit it now (see [Uninstall](#uninstall)).

glass is deliberately never a silent, invisible background process: it holds Screen Recording and
Accessibility, so it stays something you can always see and stop. **Quit glass** actually stops it — the
LaunchAgent won't relaunch it behind your back — until you next log in or relaunch `GlassMcp.app`.

## Keep the Mac awake

glass captures and drives the real desktop, so a sleeping or locked display has nothing to grab. On a
box you're not actively using, hold it awake and unlocked:

```bash
caffeinate -d -i -s &
```

`glass-mcp doctor` checks this (the `display awake` line) and names this exact command if the session
is locked.

## Optional: the `glass-mcp` CLI on your `$PATH`

The `.dmg` doesn't put `glass-mcp` on your `$PATH` — it's inside the app bundle. You don't need it for
MCP, but it's handy from a terminal. Symlink it once:

```bash
sudo ln -s /Applications/GlassMcp.app/Contents/MacOS/glass-mcp /usr/local/bin/glass-mcp
```

Then `glass-mcp status` (is glass running, and at what endpoint) and `glass-mcp doctor` work as plain
commands (see [reference/cli.md](../reference/cli.md)).

## Uninstall

From the **`glass ●`** menu bar, click **Uninstall glass…** (it confirms first, since uninstalling
also quits glass). Or from a terminal, `glass-mcp uninstall`. Either one removes
`~/Library/LaunchAgents/tech.fixedwidth.glass.plist` and boots out the running job, so glass stops
starting at login. Neither touches the app bundle — drag `GlassMcp.app` to the Trash afterward to
remove glass entirely.

## Notes

- **System requirements:** macOS 14+, developed and tested on Apple Silicon (the `.dmg` is universal,
  but Intel Macs aren't yet verified). Full list in [reference/platforms.md](../reference/platforms.md).
- **Permissions:** why the two grants behave the way they do — surviving rebuilds, needing a relaunch to
  take effect — is explained in [the permission model](../explanation/macos-permissions.md).
- **Sandboxing:** launched apps run under Seatbelt by default; the profile and the clipboard-isolation
  behaviour are in [explanation/containment.md](../explanation/containment.md).
- **Building from source** (contributors, unreleased checkouts): [build-from-source.md](build-from-source.md).
- **Android** from a macOS host: [setup-android.md](setup-android.md).
- **iOS** (the Simulator, on this macOS host): [setup-ios.md](setup-ios.md).

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each failed check. Bug reports
and questions: <https://github.com/fixed-width/glass/issues>.
