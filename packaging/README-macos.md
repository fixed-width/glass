# glass — macOS (universal)

**glass** is an MCP server that lets an AI coding agent drive native GUI apps: it
launches an app, screenshots what's on screen, clicks and types into it, reads its
logs, and detects visual changes — so the agent can build and debug a GUI on its own
instead of asking you "does this look right?".

This is the **macOS universal** build, signed and notarized. It carries both
architectures, though only Apple Silicon is tested — Intel Macs aren't yet verified.
See the project README for the full picture:
<https://github.com/fixed-width/glass>.
The full macOS guide is
[docs/how-to/setup-macos.md](https://github.com/fixed-width/glass/blob/master/docs/how-to/setup-macos.md).

---

## 1. System requirements

macOS 14 or later. Nothing else to install — capture, input, the accessibility tree and
the Seatbelt sandbox that launched apps run under are all built into macOS.

## 2. Install

Drag **`GlassMcp.app`** to **`/Applications`**, then double-click it. macOS may first
ask you to confirm an app downloaded from the internet — that is the quarantine prompt
every download gets, not a permission request.

Unlike the Linux and Windows builds, this one is a **GUI app, not a command-line
binary**: macOS grants screen and input access to an application's signed identity, so
glass ships as one.

## 3. Grant the two permissions

On first launch a **permission checklist** appears, listing **Accessibility** and
**Screen Recording**. Click **Request…** next to each, one at a time. macOS raises its
own consent prompt — use the **Open System Settings** button on it to reach the pane and
turn the permission on. The app asks on its own behalf, so it is already listed there
with nothing to add by hand.

**Granting Screen Recording needs the app to restart.** macOS offers **Quit & Reopen**;
accept it, so the grant takes effect. That restart is expected, not a failure. If a
grant doesn't show up, click **Re-check**.

Once both are on, the checklist is replaced by a **`glass ●`** menu-bar item and glass
starts serving. It also installs a LaunchAgent, so it **starts again at every login** —
**Quit glass** or **Uninstall glass…** from that menu are how you stop it. The grants
are one-time and survive restarts and updates. Why they behave this way is in
[docs/explanation/macos-permissions.md](https://github.com/fixed-width/glass/blob/master/docs/explanation/macos-permissions.md).

## 4. Connect your agent (MCP over HTTP)

The macOS app serves over HTTP at **`http://127.0.0.1:7300/`** — it does not need to be
spawned by the agent the way the Linux and Windows binaries do. Point your client at
that URL; loopback needs no token. See
[docs/how-to/connect-an-agent.md](https://github.com/fixed-width/glass/blob/master/docs/how-to/connect-an-agent.md#over-http).

The **`glass ●`** menu also has **Copy endpoint**, **Restart** (after a fresh grant),
**Quit glass**, and **Uninstall glass…**.

## 5. Check it works

Ask your agent something like:

> "Use glass to launch `/System/Applications/Font Book.app` with `sandbox` set to `off`
> and take a screenshot."

You should get back an image of the app. **Apple's own apps need `sandbox: "off"`**:
macOS requires them to be started through LaunchServices, which glass cannot then place
under Seatbelt, so a contained launch of one fails rather than running it unconfined.
Apps you are developing take the default and stay contained. The tools it now has include `glass_start`,
`glass_screenshot`, `glass_click`, `glass_type`, `glass_wait_stable`, `glass_diff`,
`glass_logs`, `glass_a11y_snapshot`, `glass_click_element`, and `glass_doctor`.

glass captures the real desktop, so a sleeping or locked display has nothing to grab.
On a Mac you aren't sitting at, hold it awake: `caffeinate -d -i -s &`.

## 6. Optional: the `glass-mcp` CLI on your `$PATH`

The binary lives inside the bundle and isn't needed for MCP, but it's useful from a
terminal — `glass-mcp status` and `glass-mcp doctor`:

```bash
sudo ln -s /Applications/GlassMcp.app/Contents/MacOS/glass-mcp /usr/local/bin/glass-mcp
```

## Uninstall

**Uninstall glass…** from the **`glass ●`** menu (or `glass-mcp uninstall`) stops glass
starting at login and quits it. Neither touches the bundle — drag `GlassMcp.app` to the
Trash to finish.

---

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each failed
check. Bug reports and questions: <https://github.com/fixed-width/glass/issues>.
