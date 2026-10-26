# Set up glass on macOS

On macOS, install a **notarized `.dmg`**. Glass uses a real Aqua login and two macOS privacy
grants. The existing shared-desktop setup is below; separate-account setup is available as a preview. (Contributors and anyone
running an unreleased checkout build from source instead — see
[build-from-source.md](build-from-source.md).)

## Apps off your desktop (preview)

This setup keeps development apps in a separate standard account in the same macOS installation.
The ordinary MCP server stays in your account. Its macOS backend owns an on-demand worker in the
selected account; Android and iOS continue through their existing backends. Use the same
`glass_start`, capture, accessibility, input, logs and `glass_stop` tools. No second MCP server is
needed.

Stop any current Glass application with `glass_stop` before changing its environment.

1. Install the signed, hardened `GlassMcp.app` in `/Applications`. Use that one app for both your
   normal account and the development account. This signing requirement applies to Glass.
2. Open Glass. On first run, choose **Off your desktop…**; if the menu-bar app is already running,
   choose **glass → Application setup…**. Choose **Create account…** at the top of the window to create a dedicated **Standard**
   account: enter its short name and a new password twice. The default name is `glassdev`; new names
   start with an ASCII letter and use at most 64 letters, digits or underscores. Glass does not save
   the password. You will use it for the account's first login; its display name is
   **Glass development (NAME)**, with your chosen short name in parentheses.
   Earlier installations can leave account-creation history after uninstall; this does not enroll that
   account again or select it for a fresh setup.
   To reuse an existing standard account, enter its short name under **Or use an existing standard
   macOS account** and click **Use existing account**. Existing
   names may contain 1–64 ASCII letters, digits or underscores. Glass never resets an existing account's password.
3. Authenticate locally when macOS asks for administrator approval. This is separate from the new
   account password. Glass creates the account if requested, then registers its worker and permission
   helper from the same installed app. If the account's standard `/Users/NAME`
   home is missing, Glass asks macOS to initialise that selected home. Existing homes are retained;
   custom home locations must already be prepared.
4. When Glass asks, use [Fast User Switching](https://support.apple.com/guide/mac-help/mchlp2439/mac)
   to log into the development account and finish macOS's first-login setup. Glass checks existing
   permissions automatically and opens **glass permissions** if a grant is missing. Already allowed
   permissions are reused; when both are allowed, the helper exits without opening that window.
   No Terminal command or second Glass installation is needed in that account. Complete only the
   missing grants, in this order:

   - Click **Request…** for **Accessibility**, then enable Glass in **System Settings → Privacy &
     Security → Accessibility**. [Apple's Accessibility instructions](https://support.apple.com/guide/mac-help/mh43185/mac).
   - Click **Request…** for **Screen Recording**, then enable Glass under **Privacy & Security →
     Screen Recording**, called **Screen & System Audio Recording** on newer versions.
     [Apple's recording instructions](https://support.apple.com/guide/mac-help/mchld6aa7d23/mac).

   If Glass is missing from a list, use **+** to add `/Applications/GlassMcp.app`. Choose **Quit & Reopen**
   if macOS offers it. Glass checks grants in fresh processes automatically. These waits have no user
   deadline; the **Re-check** button remains available for a failed check.
5. When the permission window says it is ready, switch back to your normal account, **leaving the
   development account logged in**. If no permission window appeared, return to your normal account
   and check **Application setup…**; an absent window alone does not establish readiness.
   Glass continues setup automatically, checks workspace recovery
   and verifies the actual worker's readiness before reporting **Ready**. Registration repair may
   request administrator authentication. A failed repair stops with its cause and retains completed
   registration and grants; **Use existing account** retries the outstanding work. **Cancel setup** stops waiting and
   retains completed setup steps. If the account is already logged in but its worker is missing,
   Glass checks startup and attempts one registration repair before reporting the failure. Time
   spent approving administrator authentication does not count against worker startup. While native registration, repair or disable is in progress, finish
   or cancel the macOS authentication dialogue before closing Glass; its cleanup completes as one step.
6. Connect the agent to the normal Glass endpoint, normally `http://127.0.0.1:7300/`; see
   [Connect glass to your agent](connect-an-agent.md#over-http). Existing menu-bar integration keeps
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

Routine launches and rebuilds require no account switching or extra display in your VNC view. If
login, permissions or attachment are unavailable, the macOS backend refuses work; it does not launch
on your normal desktop. Only one Glass macOS backend can use the enrolled environment at a time.
Enrollment applies to the normal macOS user who ran setup; other users retain their shared-desktop
workflow. Each development account belongs to one enrollment. To select another development account,
unenroll the current one first.

After enrollment, opening Glass starts or reuses its menu-bar app without reopening setup or
restarting existing agent work. Choose **glass → Application setup…** to check the environment
or continue interrupted setup. Existing menu-bar integration keeps its saved launch settings.
An account created through Glass remains assigned to the normal user who created it, including after
unenrollment; another normal user's setup cannot claim that creation record.

The requested workspace is shared read/write for the run. Existing executable search paths and
existing file/directory arguments receive read access; unrelated environment values are not treated
as file-sharing requests. Temporary access is recorded before changes and restored after owned work
has stopped. Generated files may belong to the development account; Glass preserves your edit
access in the selected workspace, including files atomically moved in from temporary storage.
File contents and modes are preserved. Unreadable files, explicit access denials, surviving work
or an unavailable filesystem can defer cleanup; Glass reports that failure and keeps its recovery
journal for retry. Account homes, keychains and credentials are not copied.
Default Seatbelt containment and protected artifact/trace paths still apply.

Use `glass-mcp status` to inspect enrollment and whether the environment is in use. Use `glass_stop`
in the owning MCP client to stop its app, or quit that client's Glass process. To inspect the desktop
in person, switch to the selected account: Glass refuses further private input while it is the console
account. Return to your normal account before resuming agent work.

The guided account/permission flow and account creation are under development and have not yet been
tested end to end through the production app. Initial macOS login and privacy consent still require
user action. If creation is interrupted before password preparation is confirmed, **Create account…**
with the same name resumes only the record whose identity Glass saved before creation. Supply the
original chosen password. Once Glass confirms that the account and password are prepared, choose
**Use existing account** to finish registration without entering that development-account password again.
Worker and permission-helper jobs are saved for the account's first graphical login.
Glass keeps created accounts and homes after a failure; it refuses a different account,
changed identity or preexisting home instead of overwriting them. A deleted completed account is not
recreated from its old receipt; select a new name or an existing account.
If Glass exits during setup, reopen **glass → Application setup…** to continue. If no account
has been enrolled yet, opening Glass returns to setup directly. For the same enabled enrollment
with its permission-helper registration saved, Glass continues
worker and permission checks automatically with the saved integration and address. Incomplete
registration waits for **Use existing account** and any required administrator authentication. An unfinished account creation reopens the secure password
dialog for the same account; enter the original chosen password. Glass stores only setup metadata,
never that password. Missing, disabled or changed enrollment pauses for explicit action instead
of being enabled automatically. Closing or cancelling setup clears its pending request while
retaining accounts, files and grants; **Disable isolation** also clears that request.

Reopening setup restores the creating user's last saved name and progress. If a rejected attempt only
reserved a name belonging to a different existing account, **Use existing account** can retire that user's unused
reservation after administrator authentication without changing the existing account or its password.

This is a preview. Setup, worker re-registration, and changed-build development loops through both
stdio and HTTP have been tested on an Apple Silicon Mac mini without a physical monitor connected,
using an existing initialized login. Physical keyboard/mouse coexistence has also been checked
in that retained-login setup.
The worker requires that account's graphical login. Guided post-reboot startup and subsequent
build/interact/rebuild/stop loops in an indexed workspace have been checked with existing grants.
Guided first-time onboarding still needs qualification; unattended startup after a reboot is not implemented.

### Start a stopped development desktop

Keep the development account logged in between ordinary application runs. Stopping an application
does not log that account out. When its desktop is missing, open **glass → Application setup…**
in your normal account and choose **Start environment…** for the enrolled account.

Stop ends Glass-owned app and build processes and restores temporary workspace permissions.
Verified shared macOS services remain part of the development desktop; requests already sent
to those services may finish after the application's processes stop.

Enter that development account's password in Glass's secure dialog, then complete any separate
macOS administrator approval. Neither prompt has a deadline. Glass does not save the password,
change it, re-enroll the account or repeat existing privacy grants. A running desktop is reused
without requesting its account password. Wait for **Ready** before starting application work.

This preview startup path has been tested after a real reboot with an initialized development
account and existing permissions, through Ready and ordinary build/interact/rebuild/stop loops
in an indexed workspace. It required no account switching, reenrollment or repeated privacy grants.
First-time onboarding remains under validation. It requires **Screen Sharing** to already be enabled
under **System Settings → General → Sharing**. Glass does not enable that
host-wide service automatically. It connects locally and requests only a separate desktop; an
unsupported or denied private selection stops with an error. Existing credentials longer than
63 UTF-8 bytes are not supported by this startup protocol and are never truncated or reset.
First-time macOS setup and privacy consent may still require signing into that account directly.

If startup is interrupted, open setup and explicitly choose **Start environment…** again. Glass
first finishes recorded temporary Screen Sharing access cleanup without an account password. If
the desktop is still missing afterward, choose **Start environment…** once more for a fresh login
request. Glass never repeats authentication automatically and preserves pre-existing access.
If interruption made ownership of an access entry uncertain, Glass retains it and the recovery
record. Review that account under **Screen Sharing → Allow access for**, remove its entry if
appropriate, then reopen setup to finish cleanup. Unconfirmed cleanup blocks new application work
and enrollment removal; completed account data, files and privacy grants are retained.

### Return to shared-desktop use

Stop active application work with `glass_stop`. In **glass → Application setup…**, choose
**Disable isolation**. Glass retains the development account/files/grants and restarts its matching
managed HTTP/menu-bar server. Client-spawned stdio connections must reconnect.

Alternatively, run this from your normal account:

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
   and possibly `disabled`, `worker.sock` and `tech.fixedwidth.glass.onboarding.plist`. If that permission
   helper record exists, verify its copy in the development account too:

   ```bash
   glass_onboarding_plist="$glass_development_home/Library/LaunchAgents/tech.fixedwidth.glass.onboarding.plist"
   sudo /usr/bin/plutil -p "$glass_enrollment_root/tech.fixedwidth.glass.onboarding.plist"
   sudo /usr/bin/plutil -p "$glass_onboarding_plist"
   sudo /usr/bin/cmp "$glass_enrollment_root/tech.fixedwidth.glass.onboarding.plist" "$glass_onboarding_plist"
   ```

   They must match and name the same Glass executable, `macos-onboarding`, and your normal account's
   UID. If a copy is missing, differs or points elsewhere, restore Glass and use `unenroll` to check
   the interrupted registration. The older shared root may also contain the `accounts`
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

   If step 2 verified a permission helper, unload it too:

   ```bash
   sudo /bin/launchctl bootout "gui/$glass_development_uid/tech.fixedwidth.glass.onboarding"
   sudo /bin/launchctl print "gui/$glass_development_uid/tech.fixedwidth.glass.onboarding"
   ```

   Apply the same absence/error checks before removing its verified persistent plist:

   ```bash
   sudo rm -f "$glass_onboarding_plist"
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
           "$glass_retired_root/disabled" "$glass_retired_root/account.plist" \
           "$glass_retired_root/tech.fixedwidth.glass.onboarding.plist" &&
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

   Clear this normal user's pending setup request, if present, so reopening Glass does not offer
   to continue it. This removes only setup metadata; keep the permission-recovery journal:

   ```bash
   rm -f "$HOME/Library/Application Support/glass/macos/permission-recovery/pending-setup.json"
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
`worker.requirement`, `disabled`, and verified `tech.fixedwidth.glass.onboarding.plist` files. Then
remove those known files and the empty directory:

```bash
sudo /bin/test ! -e "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "/Library/Application Support/Glass/accounts/$glass_owner_uid" &&
    sudo /bin/test ! -L "$glass_retired_root" &&
    sudo rm -f "$glass_retired_root/worker.requirement" \
        "$glass_retired_root/disabled" "$glass_retired_root/account.plist" \
        "$glass_retired_root/tech.fixedwidth.glass.onboarding.plist" &&
    sudo rmdir "$glass_retired_root" &&
    /bin/sync
```

If a new enrollment exists or unexpected files remain, stop and inspect them. Keep the permission
journal, and leave other directories intact.

### Recover a refused worker connection

Stop active Glass work, then open **glass → Application setup… → Use existing account**. The coordinator attempts
identity-checked repair of an idle registered worker and its socket, requesting administrator
authentication if needed. A running or unbound job is retained and reported. Finish the reported
work or resolve the registration mismatch before retrying.

If automatic repair still cannot complete, use the following reenrollment sequence from your normal
account. First remove the registration:

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
Use this manual sequence only when the guided repair could not complete.
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
