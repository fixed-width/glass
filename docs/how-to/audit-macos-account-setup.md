# Audit and manually manage macOS account setup

Use this guide to inspect administrator-authorised operations before approving them, or to create
and log into the development account yourself so its password never needs to be entered in Glass.
The [administrator-access reference](../reference/macos-administrator-access.md) gives the exact
wrapper commands, root roles, source links, password handling and persistent record formats.

## Inspect before authorising

Run these from Terminal in your **normal account**, not a root shell. They inspect the installed app:

```bash
glass_app='/Applications/GlassMcp.app'
glass_binary="$glass_app/Contents/MacOS/glass-mcp"
"$glass_binary" --version
/usr/bin/codesign --verify --deep --strict "$glass_app"
/usr/bin/codesign -dv --verbose=4 "$glass_app" 2>&1
/usr/bin/codesign -d -r- "$glass_app" 2>&1
/usr/sbin/spctl --assess --type execute --verbose=4 "$glass_app"
```

Compare the bundle identifier and TeamIdentifier with the release identity you trust; source builds
can use your own signing identity. The app identifier is `tech.fixedwidth.glass`. Match the reported
version/revision to the source checkout or release you are reviewing. A signature check is not proof
of source-to-binary correspondence. If the release provides checksums/provenance, check those too;
[building from source](build-from-source.md) is another way to control what you install.

In a checkout of that source revision, the entry points are easy to locate:

```bash
git grep -n -E 'fn elevate_using|pub fn enroll_with_options|pub fn remove_registration' -- crates/glass-mcp/src/macos_environment.rs
git grep -n -E 'pub\(super\) fn prepare|pub\(super\) fn privileged' -- crates/glass-mcp/src/macos_environment
git grep -n -E 'struct Password|struct Receipt' -- crates/glass-mcp/src/macos_account.rs
```

The GUI's native administrator dialog runs one of the fixed root roles listed in the reference.
It does not display the whole implementation: account and registration changes are compiled code
in that same app. In Terminal, supported commands such as `setup --account` invoke the same code
through `sudo`; they are not different implementations.

## Inspect the enrollment and jobs

The following examples read state. Replace `glassdev` with the actual account short name. If there
is no enrollment, the registration files will be absent; do not create them to make these examples
succeed.

```bash
glass_owner_uid="$(id -u)"
glass_development_account='glassdev'
glass_registration="/Library/Application Support/Glass/accounts/$glass_owner_uid"
/usr/libexec/PlistBuddy -c Print "$glass_registration/account.plist"
/bin/ls -lde "$glass_registration" "$glass_registration/worker.sock"
/usr/bin/dscl /Local/Default -read "/Users/$glass_development_account" UniqueID PrimaryGroupID GeneratedUID NFSHomeDirectory
```

Check that `owner_uid` is your normal UID and the account's UID/UUID/home match the directory-service
record. The reference describes the older preview layout if the per-owner file is absent.

Reading the other account's protected home or inspecting its launchd domain may itself require
administrator authentication. These commands inspect the registered jobs; they do not restart them:

```bash
glass_development_home="$(/usr/libexec/PlistBuddy -c 'Print :home' "$glass_registration/account.plist")"
glass_development_uid="$(/usr/libexec/PlistBuddy -c 'Print :uid' "$glass_registration/account.plist")"
sudo /usr/bin/plutil -p "$glass_development_home/Library/LaunchAgents/tech.fixedwidth.glass.worker.plist"
sudo /usr/bin/plutil -p "$glass_development_home/Library/LaunchAgents/tech.fixedwidth.glass.onboarding.plist"
sudo /bin/launchctl print "gui/$glass_development_uid/tech.fixedwidth.glass.worker"
sudo /bin/launchctl print "gui/$glass_development_uid/tech.fixedwidth.glass.onboarding"
```

Expect the installed Glass executable, `macos-worker` or `macos-onboarding`, and your normal UID in
`ProgramArguments`. A GUI domain/job can be absent when the account is logged out. That is not a
reason to boot out an entire user domain or remove unrelated jobs. Review diagnostic output locally
before sharing it: paths, account identities and process details may be present.

## Keep the development-account password outside Glass

You can perform the account creation and desktop login through macOS, leaving Glass to perform the
checked registration. This is an alternative for users who prefer manual control, not an extra
requirement of the usual wizard.

### 1. Create the account in macOS

In **System Settings → Users & Groups**, add a **Standard** account with a short name such as
`glassdev`. Enter its password only in macOS. If you already have the intended separate standard
account, reuse it; do not recreate it. See Apple's
[account creation guide](https://support.apple.com/guide/mac-help/add-a-user-or-group-mchl3e281fc9/mac).

Use one account for one Glass enrollment. If another development account is currently enrolled,
stop its Glass work and [unenroll it](setup-macos.md#return-to-shared-desktop-use) before choosing a
different one. Keep accounts and files you still need.

### 2. Approve Glass privacy access from the normal account

Open the installed Glass app. If the first-run location chooser appears, select **Off your desktop…**.
Use **glass → Application setup… → step 4 → Open Settings…** for any
missing Accessibility or Screen Recording permission, or select the same panes directly under
**System Settings → Privacy & Security** and enable **GlassMcp**. These app-wide controls are
available before choosing/enrolling an account. No approval is required separately in the
development account for the same signed app. If its entry is missing, use **+** to add
`/Applications/GlassMcp.app` in the relevant pane.

This is privacy consent, separate from administrator authorisation. Do not enter a normal-account
or administrator password in a Glass development-account password field.

### 3. Log into the development desktop yourself

Leave your normal account logged in. Choose **Apple menu → Lock Screen**, use **Switch User** or the
login screen's account selector, then log into the development account. Finish all macOS welcome
screens until its desktop is available. No Glass action is required there.

For this **manual startup route**, leave the development account logged in: lock its screen and
switch back to your normal account. Do not choose Log Out for the development account when you
intend to use its existing desktop. This differs from the normal wizard's first-use procedure,
which permits logging out and then using **Start Environment…** to start it again. Account-switching
controls can vary by macOS version and remote client; the login-screen route is used in the
[Glass setup guide](setup-macos.md). Apple also describes
[switching between logged-in users](https://support.apple.com/guide/mac-help/switch-quickly-between-users-mchlp2439/mac).

### 4. Register it from the normal account

With no Glass application work active, use **Application setup… → Use existing account**, or run
one of the following supported commands from the normal user's Terminal.

For the usual menu-bar HTTP integration:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp setup --account glassdev --launchagent
```

For a client that launches the bundled server over stdio:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp setup --account glassdev --no-launchagent
```

Replace `glassdev` with your selected account. Choose the integration you actually use. Do **not**
prefix the entire `setup` command with `sudo`: its normal-user coordinator invokes the restricted
root role when needed. Authenticate the native `sudo` or macOS request locally. If your normal account cannot use `sudo`,
use the GUI, or append `< /dev/null` to the setup command to request a native administrator dialog.
Enrollment of an
existing account does not ask for or reset its password. Wait for **Ready** before agent work.

This still uses administrator-authorised Glass registration: it is not a fully handwritten
replacement for enrollment. The password-independent parts remain auditable through the fixed
command/record/source references. At this point the development desktop is already running, so
Glass can reuse it instead of performing a password-based **Start Environment** login.

### 5. Start it manually after a reboot or logout

Repeat the manual login and return-to-normal-account procedure above, leaving the development
account logged in. Glass can reuse that desktop. If setup reports a registration problem, use
**Use existing account** to check/repair it; repair can still need administrator authorisation.

This route avoids Glass's password transfer and temporary Screen Sharing access changes. Screen
Sharing is required for Glass's **Start Environment** login path, not for your own macOS account
login. Keeping Screen Sharing disabled means using manual desktop login rather than Start
Environment; see the [startup reference](../reference/macos-administrator-access.md#startup-and-screen-sharing).

The environment still needs a real graphical login; this procedure does not provide unattended
startup. While you are using the development account as the visible console desktop, Glass refuses
private input. Return to the normal account before resuming agent work.

## Review interrupted Screen Sharing access

If startup reports unresolved temporary access, read the metadata record named in the reference
and open **System Settings → General → Sharing → Screen Sharing**. Inspect **Allow access for** and
the selected development account's entry. Apple documents the controls in
[Turn Mac screen sharing on or off](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac).

Preserve entries that existed before the Glass operation. If the pending record reports an
ambiguous addition, review whether that particular entry should remain; remove it through System
Settings only if appropriate. Then reopen Glass setup so it can verify cleanup. Do not delete the
pending record to force Ready or add all users as a workaround. Pre-authorising the account in
Screen Sharing does not eliminate every administrator check in Glass's startup implementation.

## Remove enrollment by hand

If Glass is available, the supported normal-user command is:

```bash
/Applications/GlassMcp.app/Contents/MacOS/glass-mcp unenroll
```

It completes workspace recovery before requesting privileged registration removal. If the app has
already been removed, follow [Unenroll by hand](setup-macos.md#unenroll-by-hand), including the
ownership, running-work and permission-journal checks. That procedure retains the development
account and files. Removing job files alone cannot restore unfinished workspace permission changes.

## What does not have a complete manual equivalent

Glass's signing requirements, activation-socket permissions, registration locks and recovery journal
form one checked registration/cleanup protocol. This guide does not claim that copying a plist,
running a generic `launchctl` command, or changing file permissions recursively reproduces it.
Use the supported setup/unenroll commands for those transactions, or the existing guarded manual
removal procedure when the binary is unavailable. The reference links the actual implementation so
it can be inspected before granting administrator access.
