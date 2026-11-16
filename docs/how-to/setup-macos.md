# Set up Glass on macOS

Install the signed, notarised app, choose where applications run, grant macOS privacy access, then
connect your agent. Glass needs macOS 14 or later. The release app is universal; Apple Silicon is
verified and Intel Macs are not yet verified. Contributors should [build from source](build-from-source.md).

<a id="install-the-notarized-dmg"></a>

## Install Glass

1. Download the universal `.dmg` from [Releases](https://github.com/fixed-width/glass/releases/latest).
2. Open it and drag **GlassMcp.app** to **/Applications**.
3. Open the installed app. In **Application setup…**, choose **On your desktop** or **Off your desktop…**.

**On your desktop** uses your visible Aqua login and can change focus and move your pointer.
**Off your desktop…** is a preview using a separate standard account with a real graphical login.
Containment applies separately to the launched app. See the [platform reference](../reference/platforms.md)
for preview qualification and [the permission model](../explanation/macos-permissions.md) for grant behaviour.

## Shared-desktop setup

1. **Open `GlassMcp.app`.** If the environment choice appears, select **On your desktop**
   for shared-desktop use. Glass sets up its login integration and opens **glass permissions** when
   a grant is missing. Existing permissions are reused.
2. Complete **Accessibility** first, then **Screen Recording**, using **Open Settings…** on each
   missing permission. Each button opens its exact Settings pane. The first click also requests
   native registration so Glass can appear in the list; macOS may show a consent prompt.
   Enable Glass, close Settings, and return to the checklist. If Glass is absent, use **+** to add
   `/Applications/GlassMcp.app`. Already allowed permissions need no further action.
3. After granting the missing permissions, choose **Re-check**. This restarts Glass to read the
   updated grants. If macOS offers **Quit & Reopen** for Screen Recording, choose it instead.
   Once both grants are available, the checklist is replaced by the **glass ●** menu bar.
   Closing the Glass checklist itself quits that instance; reopen `GlassMcp.app` to continue.
   Compatible signed updates reuse existing grants (see [the permission model](../explanation/macos-permissions.md)).

That's the whole setup. The MCP endpoint is `http://127.0.0.1:7300/` — head to
[Connect your agent](connect-an-agent.md#over-http).

## Apps off your desktop (preview)

Before approving administrator access, see the [command and password-handling reference](../reference/macos-administrator-access.md).
For inspection commands and account creation/login through macOS itself, see
[Audit and manually manage macOS account setup](audit-macos-account-setup.md).

This setup keeps development apps in a separate standard account in the same macOS installation.
The ordinary MCP server stays in your account. Its macOS backend owns an on-demand worker in the
selected account; Android and iOS continue through their existing backends. Use the same
`glass_start`, capture, accessibility, input, logs and `glass_stop` tools. No second MCP server is
needed.

Each run temporarily shares the requested workspace read/write with the development account.
Generated files may belong to that account; Glass preserves your edit access and restores temporary
access after owned work stops. See [workspace access](../explanation/macos-permissions.md#workspace-access-in-the-separate-account).

Stop any current Glass application with `glass_stop` before changing its environment.

**Start Environment…** requires macOS **Screen Sharing** to be enabled already under
**System Settings → General → Sharing**. Glass does not enable that host-wide service. Its startup
protocol accepts development-account passwords up to 63 UTF-8 bytes. To create and log into the
account through macOS without entering its password in Glass, use the [manual setup route](audit-macos-account-setup.md#keep-the-development-account-password-outside-glass).

1. Install the signed, hardened `GlassMcp.app` in `/Applications`. Use that one app for both your
   normal account and the development account. This signing requirement applies to Glass.
2. Open Glass. On first run, choose **Off your desktop…**; if the menu-bar app is already running,
   choose **glass → Application setup…**. Choose **Create account…** to create a dedicated **Standard**
   account: enter its short name and a new password twice. The default name is `glassdev`; new names
   start with an ASCII letter and use at most 64 letters, digits or underscores. Glass does not save
   the password. You will use it for the account's first login; its display name is
   **Glass development (NAME)**, with your chosen short name in parentheses.
   Earlier installations can leave account-creation history after uninstall; this does not enroll that
   account again or select it for a fresh setup.
   If another account is enrolled, the wizard shows its name and disables **Create account…**.
   Choose **Disable isolation** first to change accounts; this keeps the existing account and files.
   Reinstalling Glass retains enrollment.
   To reuse an existing standard account, enter its short name under **Or use an existing standard
   macOS account** and click **Use existing account**. Existing
   names may contain 1–64 ASCII letters, digits or underscores. Glass never resets an existing account's password.
3. Authenticate locally when macOS asks for administrator approval. This is separate from the new
   account password. Glass creates the account if requested, then registers its worker and permission
   helper from the same installed app. If the account's standard `/Users/NAME`
   home is missing, Glass asks macOS to initialise that selected home. Existing homes are retained;
   custom home locations must already be prepared.
4. Follow the wizard's manual login step: choose **Apple menu → Lock Screen**, then use
   **Switch User** or the login screen's account selector to log in to the development account.
   Use this login-screen route with VNC too. Finish macOS's first-login setup. Complete the macOS
   welcome screens until the account's desktop appears. A successful account login or existing
   privacy grants do not mean those screens have finished. Glass reports this remaining step
   instead of **Ready** while Apple Setup Assistant owns the development desktop.
   Your work in the development account is now complete. No Glass instructions or permission
   window needs to appear there. Choose **Apple menu → Log Out**, then log back in to your normal
   account. You do not need to keep the development account logged in after this first visit.
5. Back in **glass → Application setup…**, choose **Start Environment…** for the selected account.
   Enter its password locally and complete any separate macOS administrator prompt. Glass starts
   its private desktop without switching your visible desktop. Wait for **Ready**.
   The wizard's permission step checks app-wide approvals in fresh normal-account Glass processes,
   independently of whether the development desktop is running. In the wizard, step 4 is privacy
   access and step 5 starts the environment; these are the wizard's step numbers.
   The active step is highlighted, with progress and errors beneath its instructions. All five
   steps remain available; checking keeps the current step highlighted.
   Step 4 always shows app-wide permission status and one **Open Settings…** button per permission,
   independently of the selected account or current step. Each click opens that
   exact pane; the first click also requests native registration and may show macOS consent. Approve only missing grants from your normal account, then return to this
   wizard. Keep it open: while setup is checking, fresh worker checks detect grants and continue
   automatically. If checking is paused, choose **Continue setup**.
   If macOS offers **Quit & Reopen**, choose **Later** during this wizard; its permission checks use
   fresh workers, so the wizard does not need to restart. The same signed app reuses approvals across
   accounts. If Glass is absent from the Settings list, add `/Applications/GlassMcp.app` with **+**.
   Choose **Continue setup** if checking is paused with the development desktop running. Glass checks workspace recovery
   and verifies the actual worker's readiness before reporting **Ready**. Registration repair may
   request administrator authentication. A failed repair stops with its cause and retains completed
   registration and grants; **Use existing account** retries the outstanding work. **Cancel setup** stops waiting and
   retains completed setup steps. If the account is already logged in but its worker is missing,
   Glass checks startup and attempts one registration repair before reporting the failure. Time
   spent approving administrator authentication does not count against worker startup. While native registration, repair or disable is in progress, finish
   or cancel the macOS authentication dialogue before closing Glass; its cleanup completes as one step.
6. Connect the agent to the normal Glass endpoint, normally `http://127.0.0.1:7300/`; see
   [Connect Glass to your agent](connect-an-agent.md#over-http). Existing menu-bar integration keeps
   the address it is serving. The agent uses the ordinary build, capture, accessibility, input, logs
   and stop tools while you stay in your normal account.

For a client that launches Glass over stdio, run this once from Terminal in your normal account,
then register the bundled binary with the client:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp setup --account glassdev --no-launchagent
```

Replace `glassdev` with the account's short name. The command follows the same guided flow and waits
for login/consent and your return automatically. A Terminal invocation uses `sudo` authentication.
If your normal account cannot use `sudo`, append `< /dev/null` to request a macOS administrator dialogue.
`--non-interactive` reports pending steps without opening onboarding or waiting for user action.

For clients using `mcpServers` JSON:

```json
{
  "mcpServers": {
    "glass": {
      "command": "/Applications/GlassMcp.app/Contents/MacOS/glass-mcp"
    }
  }
}
```

See [stdio client registration](connect-an-agent.md#over-stdio). Reconnect an existing MCP client after
upgrading its Glass binary. To install HTTP/menu-bar integration from the CLI, use `--launchagent`
instead of `--no-launchagent`; keep the same `--addr` when using a custom address. Plain `setup` resumes
an existing enrollment and retains its selected integration unless you supply an integration flag.

For later starts, interrupted setup or worker failures, use [Manage the macOS environment](manage-macos-environment.md).
The [platform reference](../reference/platforms.md) records the preview's tested scope and remaining limits.

## The menu-bar item

Once granted, `GlassMcp.app` runs as a **visible menu-bar app** — look for **`glass ●`** in the menu
bar. It starts automatically at login (via the LaunchAgent) and its dropdown shows:

- the **served endpoint** (or a notice if another Glass is already bound to it),
- **Application setup…** — choose/check the application environment or resume setup,
- **Copy endpoint** — copy the MCP endpoint to your clipboard,
- **Restart** — restart the background agent (e.g. after a fresh grant),
- **Quit glass** — stop the agent,
- **Uninstall glass…** — stop Glass from starting at login and quit it now (see [Uninstall](#uninstall)).

Glass is deliberately never a silent, invisible background process: it holds Screen Recording and
Accessibility, so it stays something you can always see and stop. **Quit glass** actually stops it — the
LaunchAgent won't relaunch it behind your back — until you next log in or relaunch `GlassMcp.app`.

## Keep the Mac awake

With shared-desktop setup, Glass captures and drives the real desktop, so a sleeping or locked display has nothing to grab. On a
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

Then `glass-mcp status` (is Glass running, and at what endpoint) and `glass-mcp doctor` work as plain
commands (see [reference/cli.md](../reference/cli.md)).

## Manage the environment

[Manage the macOS environment](manage-macos-environment.md) covers status, interrupted setup,
workspace recovery and removal. Administrator operations and password handling are documented in
[macOS administrator access](../reference/macos-administrator-access.md).

### Start a stopped development desktop

Follow [Start a stopped development desktop](manage-macos-environment.md#start-a-stopped-development-desktop).

### Return to shared-desktop use

Follow [Return to shared-desktop use](manage-macos-environment.md#return-to-shared-desktop-use).

### Recover a refused worker connection

Follow [Recover a refused worker connection](manage-macos-environment.md#recover-a-refused-worker-connection).

### Unenroll by hand

If the app is unavailable, follow [Remove enrollment by hand](remove-macos-enrollment-manually.md).

## Uninstall

[Uninstall Glass](manage-macos-environment.md#uninstall) before deleting the app bundle, so Glass can
finish workspace recovery and remove its jobs. The development account, home and data are retained.

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
