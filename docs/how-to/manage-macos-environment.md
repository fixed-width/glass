# Manage the macOS environment

Use this guide after [setting up Glass on macOS](setup-macos.md) to start a stopped environment,
resume setup, recover a worker connection, return to your desktop or uninstall Glass. Run commands
from your normal account. Stop active application work before changing enrollment.

## Inspect the current environment

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

Use `glass-mcp status` to inspect enrollment and whether the environment is in use. Use `glass_stop`
in the owning MCP client to stop its app, or quit that client's Glass process. To inspect the desktop
in person, switch to the selected account: Glass refuses further private input while it is the console
account. Return to your normal account before resuming agent work.

## Resume interrupted setup

If creation is interrupted before password preparation is confirmed, **Create account…**
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

## Start a stopped development desktop

Glass keeps the development desktop running between ordinary application runs. Stopping an application
does not log that account out. You can log out after completing the manual first-login screens;
**Start Environment…** starts the initialized desktop again. When its desktop is missing, open **glass → Application setup…**
in your normal account and choose **Start Environment…** for the enrolled account.

Stop ends Glass-owned app and build processes and restores temporary workspace permissions.
Verified shared macOS services remain part of the development desktop; requests already sent
to those services may finish after the application's processes stop.

Enter that development account's password in Glass's secure dialog, then complete any separate
macOS administrator approval. Neither prompt has a deadline. Glass does not save the password,
change it, re-enroll the account or repeat existing privacy grants. A running desktop is reused
without requesting its account password. Wait for **Ready** before starting application work.

This startup path requires **Screen Sharing** to already be enabled
under **System Settings → General → Sharing**. Glass does not enable that
host-wide service automatically. It connects locally and requests only a separate desktop; an
unsupported or denied private selection stops with an error. Existing credentials longer than
63 UTF-8 bytes are not supported by this startup protocol and are never truncated or reset.
First-time macOS welcome screens require one manual login to that account. You can log out
after completing them; approve any missing Glass privacy grants from your normal account.

If startup is interrupted, open setup and explicitly choose **Start Environment…** again. Glass
first finishes recorded temporary Screen Sharing access cleanup without an account password. If
the desktop is still missing afterward, choose **Start Environment…** once more for a fresh login
request. Glass never repeats authentication automatically and preserves pre-existing access.
If interruption made ownership of an access entry uncertain, Glass retains it and the recovery
record. Review that account under **Screen Sharing → Allow access for**, remove its entry if
appropriate, then reopen setup to finish cleanup. Unconfirmed cleanup blocks new application work
and enrollment removal; completed account data, files and privacy grants are retained.

## Return to shared-desktop use

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

The development account can be logged out when you disable isolation or uninstall. Glass checks
that its own jobs and processes are gone before removing registration; macOS background services
and the account are retained. Setup failures stay visible even if you edited the account-name field.
The menu's **Uninstall glass…** action shows a dialog if cleanup fails.

A failure before worker and permission cleanup completes retains your enrollment and reports the
problem. Glass does not silently move work onto your desktop. Another user's enrollment is unaffected.

## Recover a refused worker connection

Stop active Glass work, then open **glass → Application setup… → Use existing account**. The coordinator attempts
identity-checked repair of an idle registered worker and its socket, requesting administrator
authentication if needed. Registration repair retains and reports a running or unbound job.
Desktop startup can reuse a validated waiting worker after restoring the normal user's connection
access. Let pending worker checks finish, stop active Glass work, or resolve the registration mismatch
before retrying.

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

## Uninstall

From the **`glass ●`** menu bar, click **Uninstall glass…** (it confirms first, since uninstalling
also quits Glass). Or from a terminal, `glass-mcp uninstall`. Stop active application work first.
For an enrolled account, uninstall restores recoverable workspace permissions and requests administrator
authentication to remove its worker registration. It retains the account, home, permissions and app
data. Your enrollment is removed. To return to shared-desktop use while keeping Glass's startup
integration, use `glass-mcp unenroll` instead.

Either path also removes
`~/Library/LaunchAgents/tech.fixedwidth.glass.plist` and boots out the running job, so Glass stops
starting at login. Neither touches the app bundle — drag `GlassMcp.app` to the Trash afterward to
remove Glass entirely.

Run uninstall before deleting the app bundle. If you deleted it while an account was still enrolled,
follow [unenrolling by hand](remove-macos-enrollment-manually.md) for the account worker. To also remove the normal
account's HTTP/menu-bar startup integration, remove its plist and unload its job from that normal account:

```bash
rm -f "$HOME/Library/LaunchAgents/tech.fixedwidth.glass.plist" &&
    launchctl bootout "gui/$(id -u)/tech.fixedwidth.glass"
```

An already-absent job needs no further unloading; other errors should be resolved before continuing.
