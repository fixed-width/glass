# Set up glass on macOS

On macOS, install a **notarized `.dmg`**. Glass uses a real Aqua login and two one-time macOS privacy
grants. The existing shared-desktop setup is below; separate-account setup is available as a preview. (Contributors and anyone
running an unreleased checkout build from source instead — see
[build-from-source.md](build-from-source.md).)

## Apps off your desktop (preview)

This setup keeps development apps in a separate standard account in the same macOS installation.
The ordinary MCP server stays in your account. Its macOS backend owns an on-demand worker in the
selected account; Android and iOS continue through their existing backends. Use the same
`glass_start`, capture, accessibility, input, logs and `glass_stop` tools. No second MCP server is
needed.

1. Install the signed `GlassMcp.app` in `/Applications`. Account setup requires Glass's hardened
   runtime signature; a source build with debugging or library-validation exceptions cannot serve
   as the private worker. This requirement applies to Glass, not your development applications.
2. In **System Settings → Users & Groups**, create a standard account for development, or select
   an existing standard account dedicated to that work. You own its password, home and app data;
   Glass does not create or delete the account or store its password. Log into it once to finish
   macOS's first-login setup, then switch back without logging it out.
3. From Terminal in your normal account, run the following, replacing `glassdev` with its short name:

   ```bash
   /Applications/GlassMcp.app/Contents/MacOS/glass-mcp setup --account glassdev --no-launchagent
   ```

   Administrator authentication registers only Glass's worker. A pending-login or missing-grant
   error means registration is saved and the next step remains; it does not mean the account must
   be recreated. Authentication and setup have no test timer.
4. Log into the selected account, complete macOS's first-login setup, and open `/Applications/GlassMcp.app`.
   Use the existing checklist to enable **Accessibility** and **Screen Recording** for Glass there.
   Accept a requested restart. Switch back to your normal account **without logging the development
   account out**. Fast User Switching changes the account you are viewing while keeping both logged in.
5. Run the same setup command again to verify readiness. A healthy existing registration is reused
   without another administrator request. After logging the development account out and back in,
   run setup again; restoring access to its recreated worker socket may require administrator
   authentication. Register the bundle binary with your stdio MCP client, or
   use `--launchagent` instead for the existing HTTP/menu-bar integration.

Routine launches and rebuilds require no account switching or extra display in your VNC view. If
login, permissions or attachment are unavailable, the macOS backend refuses work; it does not launch
on your normal desktop. Only one Glass macOS backend can use the enrolled environment at a time. This preview has one
owner/account registration per Mac and does not support changing the selected account.

The requested workspace is shared read/write for the run. Existing executable search paths and
existing file/directory arguments receive read access; unrelated environment values are not treated
as file-sharing requests. Temporary access is recorded before changes and restored after owned work
has stopped. Generated files may belong to the development account; inherited permissions retain
your read/write access. A surviving process, new background activity in that account or an unavailable
filesystem can defer recovery; Glass reports that failure and preserves its journal for retry. Account homes, keychains and credentials are not copied.
Default Seatbelt containment and protected artifact/trace paths still apply.

Use `glass-mcp status` to inspect enrollment and whether the environment is in use. Use `glass_stop`
in the owning MCP client to stop its app, or quit that client's Glass process. To inspect the desktop
in person, switch to the selected account: Glass refuses further private input while it is the console
account. Return to your normal account before resuming agent work.

This is a preview. The underlying development loop and automated desktop coexistence have been
exercised on Apple Silicon; the supported installer and account lifecycle still need native end-to-end
qualification. Physical concurrent input, unattended cold-boot login and monitor-free operation are
not qualified. Keep the account logged in; after a restart, log into it again. This does not change
Windows's shared-desktop behavior.

## Shared-desktop setup

### Install the notarized `.dmg`

Every tagged release attaches a **notarized, universal** `GlassMcp.app` in a `.dmg` to the
[Releases page](https://github.com/fixed-width/glass/releases). Because it's signed and notarized,
macOS opens it without a Gatekeeper detour, and first-run is a double-click:

1. **Download** the `.dmg` from Releases and open it.
2. **Drag `GlassMcp.app` to `/Applications`.**
3. **Double-click `GlassMcp.app`.** A **permission checklist window** appears, listing **Accessibility**
   and **Screen Recording**, each with a ✓/○ status and its own **Request…** button.
4. Click **Request…** for each permission, **one at a time**. macOS raises its own consent prompt,
   whose **Open System Settings** button navigates to that permission's pane under **System Settings →
   Privacy & Security**. Because the app is asking for *itself*, `GlassMcp.app` is already listed there
   — no manual `＋`-add needed. glass deliberately does not force the pane open itself, which fought
   the system's own prompt. Granting **Screen Recording needs `GlassMcp.app` to restart** — macOS
   offers **Quit & Reopen** — and that restart is expected, not an error. If you grant the two
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

With shared-desktop setup, glass captures and drives the real desktop, so a sleeping or locked display has nothing to grab. On a
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
also quits glass). Or from a terminal, `glass-mcp uninstall`. Stop active application work first.
For an enrolled account, uninstall restores recoverable workspace permissions and requests administrator
authentication to remove its worker registration. It retains the account, home, permissions and app
data. A disabled registration marker prevents clients from silently returning to shared-desktop
control; rerun `setup --account ACCOUNT` to enroll again.

Either path also removes
`~/Library/LaunchAgents/tech.fixedwidth.glass.plist` and boots out the running job, so glass stops
starting at login. Neither touches the app bundle — drag `GlassMcp.app` to the Trash afterward to
remove glass entirely.

## Notes

- **System requirements:** macOS 14+, developed and tested on Apple Silicon (the `.dmg` is universal,
  but Intel Macs aren't yet verified). Full list in [reference/platforms.md](../reference/platforms.md).
- **Desktop interaction:** current native input can activate the target and move your pointer.
  `glass_capabilities` with `scope: "session"` reports support and desktop interference. The existing
  Accessibility grant authorizes control; no additional input-mode setting or permission is needed.
- **Permissions:** why the two grants behave the way they do — surviving rebuilds, needing a relaunch to
  take effect — is explained in [the permission model](../explanation/macos-permissions.md).
- **Sandboxing:** launched apps run under Seatbelt by default; the profile and the clipboard-isolation
  behaviour are in [explanation/containment.md](../explanation/containment.md).
- **Checking for updates:** `glass-mcp update --check` reports whether a newer release exists, but
  upgrading itself means downloading and opening a newer `.dmg` — there is no in-place update on macOS.
- **Building from source** (contributors, unreleased checkouts): [build-from-source.md](build-from-source.md).
- **Android** from a macOS host: [setup-android.md](setup-android.md).
- **iOS** (the Simulator, on this macOS host): [setup-ios.md](setup-ios.md).

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each failed check. Bug reports
and questions: <https://github.com/fixed-width/glass/issues>.
