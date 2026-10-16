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
   without another administrator request. Earlier preview registrations migrate to the per-user
   layout during setup, with administrator authentication. After logging the development account out and back in,
   run setup again to check readiness. Restoring access to a recreated worker socket may require
   administrator authentication; a connection refusal needs the recovery sequence below.
   Register the bundle binary with your stdio MCP client, or
   use `--launchagent` instead for the existing HTTP/menu-bar integration. Setup reloads that
   LaunchAgent from the current executable and verifies that its HTTP server uses the enrolled
   account. An unrelated process using the address causes setup to fail; stop it and rerun setup.

Routine launches and rebuilds require no account switching or extra display in your VNC view. If
login, permissions or attachment are unavailable, the macOS backend refuses work; it does not launch
on your normal desktop. Only one Glass macOS backend can use the enrolled environment at a time.
Enrollment applies to the normal macOS user who ran setup; other users retain their shared-desktop
workflow. Each development account belongs to one enrollment. To select another development account,
unenroll the current one first.

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

This is a preview. Setup, worker re-registration, and changed-build development loops through both
stdio and HTTP have been tested on an Apple Silicon Mac mini without a physical monitor connected,
using an existing initialized login. Simultaneous physical keyboard/mouse use and unattended startup
after a reboot have not been tested.
Keep the account logged in; after a restart, log into it again. This does not change Windows's
shared-desktop behavior.

### Return to shared-desktop use

Stop active application work with `glass_stop`, then run this from your normal account:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp unenroll
```

Administrator authentication removes your worker registration after checking that owned work and
permission recovery have completed. The app, development account, home, privacy grants and normal
HTTP/menu-bar startup integration remain. Restart existing Glass MCP servers before starting
shared-desktop work. The normal account needs its usual Accessibility and Screen Recording grants.
`--non-interactive` requires existing administrator authorization and fails instead of prompting.

A failure before worker and permission cleanup completes retains your enrollment and reports the
problem. Glass does not silently move work onto your desktop. Another user's enrollment is unaffected.

### Unenroll by hand

Use this procedure if the app bundle has already been removed and `glass-mcp unenroll` is unavailable.
It removes the account's Glass worker and your enrollment while retaining the development account,
home, privacy grants and normal HTTP/menu-bar startup configuration. Administrator authorisation is
needed for the worker and enrollment files.

Manual file removal cannot restore unfinished workspace permission changes. Preserve the permission
journal and enrollment record until step 4 confirms recovery has finished. If recovery is pending,
restore a compatible Glass app at its original path and run `unenroll` instead.

Run these commands in the same Terminal window, from the normal account that enrolled Glass.
Stop if a command fails, except for the explicitly described already-absent jobs or files.
Keep other Glass setup, unenrollment and uninstall operations stopped during this procedure.

1. **Stop Glass work and identify your enrollment.** Disconnect Glass in MCP clients so they cannot
   restart it during cleanup, and quit the normal account's Glass menu-bar app. Save and close the
   development applications in the selected account; log that account out if needed to stop its work.
   Return to your normal account before continuing.

   ```bash
   glass_owner_uid="$(id -u)"
   printf 'Normal account UID: %s\n' "$glass_owner_uid"
   glass_enrollment_root="/Library/Application Support/Glass/accounts/$glass_owner_uid"
   sudo /usr/libexec/PlistBuddy -c Print "$glass_enrollment_root/account.plist"
   ```

   Only if that record is absent, try the earlier preview location:

   ```bash
   glass_enrollment_root="/Library/Application Support/Glass"
   sudo /usr/libexec/PlistBuddy -c Print "$glass_enrollment_root/account.plist"
   ```

   Confirm `owner_uid` equals the UID printed by `id -u`. Check that `name`, `uid`, `record_uuid` and
   `home` still identify your chosen development account. If the record is missing or incomplete,
   stop rather than guessing which account or files to remove.

   Check the current account identity against that record:

   ```bash
   glass_development_account="$(sudo /usr/libexec/PlistBuddy -c 'Print :name' "$glass_enrollment_root/account.plist")"
   /usr/bin/dscl /Local/Default -read "/Users/$glass_development_account" UniqueID GeneratedUID NFSHomeDirectory
   ```

   `UniqueID`, `GeneratedUID` and `NFSHomeDirectory` must match `uid`, `record_uuid` and `home` in the
   enrollment. Stop if the account has been deleted or its identity has changed.

   If both layouts contain your records, complete the procedure for the per-user directory first,
   then repeat it for the older location. Inspect `owner_uid` each time; leave records belonging to
   other users intact.

2. **Check the worker registration.** Read its location from the enrollment record:

   ```bash
   glass_development_uid="$(sudo /usr/libexec/PlistBuddy -c 'Print :uid' "$glass_enrollment_root/account.plist")"
   glass_development_home="$(sudo /usr/libexec/PlistBuddy -c 'Print :home' "$glass_enrollment_root/account.plist")"
   glass_worker_plist="$glass_development_home/Library/LaunchAgents/tech.fixedwidth.glass.worker.plist"
   sudo /usr/bin/plutil -p "$glass_worker_plist"
   sudo ls -la "$glass_enrollment_root"
   ```

   The worker's arguments must name your Glass executable, `macos-worker`, and your normal account's
   UID; its socket path must be `$glass_enrollment_root/worker.sock`. Older preview jobs omit the UID
   argument and use the older enrollment root. If the plist is already absent, confirm in step 3
   that its job is also absent. Stop if the job points to another enrollment or the enrollment
   directory contains unexpected files. Its expected entries are `account.plist`, `worker.requirement`,
   and possibly `disabled` and `worker.sock`. The older shared root may also contain the `accounts`
   directory for newer registrations; leave that directory intact. The enrollment root must be a
   root-owned directory, not a symbolic link; its metadata files must be regular root-owned files
   with a single link. Stop on unexpected ownership, types or permission errors.

3. **Disable the worker and remove its startup job.** The marker prevents a remaining worker from
   accepting new work while cleanup is incomplete:

   ```bash
   glass_disabled_marker="$(mktemp -t glass-unenroll)" &&
       printf '%s\n' 'Glass account registration removed' > "$glass_disabled_marker" &&
       sudo /usr/bin/install -S -o root -g wheel -m 644 "$glass_disabled_marker" "$glass_enrollment_root/disabled" &&
       rm "$glass_disabled_marker" &&
       /bin/sync
   ```

   This installs the marker atomically and flushes it before the worker is unloaded. After that
   block succeeds:

   ```bash
   sudo /bin/launchctl bootout "gui/$glass_development_uid/tech.fixedwidth.glass.worker"
   sudo /bin/launchctl print "gui/$glass_development_uid/tech.fixedwidth.glass.worker"
   ```

   `bootout` may report that the job or GUI domain is already absent. The final `print` must report
   that it cannot find the job/domain; if it still prints a loaded job, stop and resolve that first.
   An authentication or permission error is not confirmation that the job is gone. Then remove only
   this worker's persistent plist:

   ```bash
   sudo rm -f "$glass_worker_plist"
   ```

   Confirm the worker and development applications have exited before removing enrollment metadata.
   Unloading a job alone does not prove that all applications it launched have stopped.

4. **Check permission recovery before deleting the enrollment record.** In your normal account:

   ```bash
   glass_journal="$HOME/Library/Application Support/glass/macos/permission-recovery/pending.jsonl"
   ls -l "$glass_journal"
   if [ -s "$glass_journal" ]; then
       tail -n 1 "$glass_journal"
   fi
   ```

   Continue only if the journal is absent or empty, or its final record is `{"kind":"Closed"}`.
   A `No such file or directory` message is expected if no journal was created. Permission and I/O
   errors do not establish that the journal is absent; stop if it cannot be inspected.
   Otherwise, leave the disabled enrollment and journal in place. Restore Glass at its original
   path and run `unenroll` to replay the recorded permission changes. Removing the worker plist
   in step 3 does not prevent that recovery. Keep the journal; Glass needs its recorded filesystem
   identities to restore the correct objects, including after renames.

5. **Remove only the verified enrollment files.** For the per-user layout, move the directory out
   of the active location before deleting its contents. The commands stop if a previous retired
   directory already exists:

   ```bash
   glass_retired_root="/Library/Application Support/Glass/accounts/.unenroll-$glass_owner_uid"
   test "$glass_enrollment_root" = "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
       sudo /bin/test ! -e "$glass_retired_root" &&
       sudo /bin/test ! -L "$glass_retired_root" &&
       sudo rm -f "$glass_enrollment_root/worker.sock" &&
       sudo mv "$glass_enrollment_root" "$glass_retired_root" &&
       /bin/sync &&
       sudo rm -f "$glass_retired_root/worker.requirement" \
           "$glass_retired_root/disabled" "$glass_retired_root/account.plist" &&
       sudo rmdir "$glass_retired_root" &&
       /bin/sync
   ```

   For the older layout, remove its known files in place; its directory may also hold other users'
   registrations:

   ```bash
   test "$glass_enrollment_root" = "/Library/Application Support/Glass" &&
       sudo rm -f "$glass_enrollment_root/worker.sock" \
           "$glass_enrollment_root/worker.requirement" "$glass_enrollment_root/disabled" &&
       sudo rm "$glass_enrollment_root/account.plist" &&
       /bin/sync
   ```

   Leave `/Library/Application Support/Glass`, its `accounts` directory, lock files and other users'
   registrations in place. Check for an older record belonging to you as described in step 1, then
   restart your MCP clients or normal Glass integration when the app is available again.

If interrupted after the per-user directory was moved, its original path is absent and
`accounts/.unenroll-<your UID>` may remain. The work and journal checks above must already have
completed. Confirm the original path is still absent and inspect the retired directory:

```bash
glass_owner_uid="$(id -u)"
glass_retired_root="/Library/Application Support/Glass/accounts/.unenroll-$glass_owner_uid"
sudo /bin/test ! -e "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "$glass_retired_root" &&
    sudo ls -la "$glass_retired_root"
```

Continue only if it is a root-owned directory containing just the remaining `account.plist`,
`worker.requirement` and `disabled` files. Then remove those known files and the empty directory:

```bash
sudo /bin/test ! -e "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "$glass_retired_root" &&
    sudo rm -f "$glass_retired_root/worker.requirement" \
        "$glass_retired_root/disabled" "$glass_retired_root/account.plist" &&
    sudo rmdir "$glass_retired_root" &&
    /bin/sync
```

If a new enrollment exists or unexpected files remain, stop and inspect them. Keep the permission
journal, and leave other directories intact.

### Recover a refused worker connection

If setup reports `Connection refused` after logging the development account out and back in or
changing its registration, stop active Glass work and run this from your normal account:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp unenroll
```

This removes your worker registration and old socket while retaining the normal-account HTTP/menu-bar
integration, development account, home, privacy grants and app data. Register again
using your previous integration. For stdio:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp setup --account glassdev --no-launchagent
```

For HTTP/menu-bar integration:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp setup --account glassdev --launchagent
```

Replace `glassdev` with the enrolled account's short name. If HTTP used a custom address, include
the same `--addr` value. These registration changes request administrator authentication.
This preview cannot repair a loaded registration with a stale socket by rerunning setup alone.
Keep the development account logged in during ordinary work.

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
data. Your enrollment is removed. To return to shared-desktop use while keeping Glass's startup
integration, use `glass-mcp unenroll` instead.

Either path also removes
`~/Library/LaunchAgents/tech.fixedwidth.glass.plist` and boots out the running job, so glass stops
starting at login. Neither touches the app bundle — drag `GlassMcp.app` to the Trash afterward to
remove glass entirely.

Run uninstall before deleting the app bundle. If you deleted it while an account was still enrolled,
follow [unenrolling by hand](#unenroll-by-hand) for the account worker. To also remove the normal
account's HTTP/menu-bar startup integration, remove its plist and unload its job from that normal account:

```bash
rm -f "$HOME/Library/LaunchAgents/tech.fixedwidth.glass.plist" &&
    launchctl bootout "gui/$(id -u)/tech.fixedwidth.glass"
```

An already-absent job needs no further unloading; other errors should be resolved before continuing.

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
