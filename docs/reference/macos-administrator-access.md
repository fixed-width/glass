# macOS administrator access and password handling

This reference describes the installed Glass app's separate-account setup: what an administrator
approval runs, what it changes, and what it records. For inspection commands and alternatives to
entering the development-account password in Glass, see
[Audit and manually manage macOS account setup](../how-to/audit-macos-account-setup.md).

## Which password goes where

| Request | Credential | Recipient and purpose |
| --- | --- | --- |
| macOS administrator dialog, or `sudo` in Terminal | An authorised administrator's password, or another authentication method offered by macOS | macOS or `sudo` authorises a transient root process. Glass does not collect this password. |
| Glass's Create account dialog | The password chosen for the new development account | Glass transfers it locally to its authenticated account-creation helper, which initialises the macOS account's credentials. |
| Glass's Start Environment dialog | The existing development account's password | Glass transfers it locally to its authenticated startup helper for a local Apple Screen Sharing login. |
| macOS's login screen | The account being logged into | macOS handles the login. Manually creating and logging into the account can keep its password outside Glass entirely. |

Administrator authentication, permission to administer the computer, and Accessibility/Screen
Recording consent are separate macOS decisions. Privacy consent does not give Glass root privileges.
The name on a system dialog can depend on the launch path, including Glass, `osascript`, or Terminal.
Separate registration, repair, startup and removal operations can each need authorisation.

## The commands behind administrator approval

The shared entry point is `elevate_using` in
[macos_environment.rs](../../crates/glass-mcp/src/macos_environment.rs).
It launches the **currently running Glass executable**, with a fixed internal role and its arguments.
The usual installed path is `/Applications/GlassMcp.app/Contents/MacOS/glass-mcp`.

From an interactive Terminal, its command shape is:

```text
/usr/bin/sudo <running-glass-binary> <role-and-arguments>
```

Non-interactive operation adds `sudo -n`, which refuses instead of asking for a password.
The GUI uses `/usr/bin/osascript -e` and an AppleScript expression of this form:

```applescript
do shell script "'/usr/bin/env' 'SUDO_UID=501' '/Applications/GlassMcp.app/Contents/MacOS/glass-mcp' 'macos-enroll' 'glassdev' '--onboarding'" with administrator privileges
```

This example uses owner UID `501` and account `glassdev`; the actual values come from the normal
user and selected account. Each shell argument is quoted separately, then the complete shell
command is escaped as an AppleScript string. `SUDO_UID` identifies whose enrollment is being
managed; it is not a credential. Terminal `sudo` supplies that variable itself.

These are the internal argument forms, **for auditing rather than direct invocation**:

```text
macos-prepare-account <account> <private-temporary-directory>/password.sock
macos-enroll <account> [--onboarding] [--repair]
macos-start-environment <private-temporary-directory>/password.sock
macos-unenroll
```

No password is an argument. The socket-based roles require the matching live, authenticated setup
connection; pasting an endpoint path into a shell is not an independent setup procedure. These roles
are hidden implementation interfaces, not substitutes for the supported `setup` and `unenroll`
commands. Their dispatch is in [main.rs](../../crates/glass-mcp/src/main.rs).

There is no downloaded setup shell script in this path. The privileged work is compiled
Rust/Objective-C in the same app. The AppleScript is the elevation wrapper, not the implementation
of account creation or registration.

## What each operation can change

| User operation | Privileged role | Changes and boundaries | Implementation |
| --- | --- | --- | --- |
| Create account or continue unfinished creation | `macos-prepare-account` | Reserves a UID/UUID, creates the requested standard account, initialises its password, saves creation checkpoints, then enrols it. A resumed account's existing credentials are verified rather than reset. An unused newly created record can be withdrawn if creation validation fails before credentials/home creation. | `create`/`prepare` in [account_creation.rs](../../crates/glass-mcp/src/macos_environment/account_creation.rs); [account_setup.rs](../../crates/glass-macos/src/account_setup.rs) and [account_setup.m](../../crates/glass-macos/src/account_setup.m) |
| Use existing account, setup, or registration repair | `macos-enroll` | Validates the standard account, installed app and ownership; prepares a missing standard `/Users/ACCOUNT` home; writes root-owned enrollment/signing records and the account's two Glass LaunchAgent plists. Loads or repairs only the identity-checked Glass jobs and their activation socket. It does not set an existing account's password. | `enroll_with_options`, `job`, `onboarding_job`, `prepare_home` in [macos_environment.rs](../../crates/glass-mcp/src/macos_environment.rs); [worker_activation.rs](../../crates/glass-mcp/src/macos_environment/worker_activation.rs) |
| Start Environment or finish interrupted startup cleanup | `macos-start-environment` | Reads the current enrollment, restores pending temporary access, checks the local Apple desktop service, and starts/reuses the selected desktop. It can temporarily add that account to the existing Screen Sharing access group and removes its confirmed temporary addition afterwards. It can activate the registered Glass worker. | `run`, `privileged`, `restore_record` in [desktop_start.rs](../../crates/glass-mcp/src/macos_environment/desktop_start.rs); `glass_desktop_access` in [account_setup.m](../../crates/glass-macos/src/account_setup.m) |
| Disable isolation, unenroll, or remove account integration during uninstall | `macos-unenroll` | Removes verified Glass jobs and enrollment after checking ownership and active work. Pending application/workspace recovery runs before privileged registration removal; pending startup access is restored during removal. The development account, home, app data and privacy grants are retained. | `leave_account_using`, `remove_registration` in [macos_environment.rs](../../crates/glass-mcp/src/macos_environment.rs); [worker_removal.rs](../../crates/glass-mcp/src/macos_environment/worker_removal.rs) |

Root is used for local account-database changes, registration inside another user's home and
root-owned state, cross-user job management, and the current startup implementation's service
identity/access checks. Those checks remain part of an authorised transaction even when an existing
record or access grant can be reused. This does not mean ordinary build commands need root.

Opening or resuming setup can require registration repair; an administrator request therefore does
not necessarily mean a new account is being created. Removal of the normal user's own HTTP
LaunchAgent is separate from privileged development-account registration removal. Removing the
`.app` itself is a separate Finder action, which can have its own macOS authorisation request.

The native account operations use OpenDirectory. Missing standard homes use
`/usr/sbin/createhomedir -c -l -u ACCOUNT`. Job management uses `/bin/launchctl` operations such as
`print`, `bootstrap`, `kickstart`, and job-specific removal. The exact job contents are generated by
`job` and `onboarding_job`; inspection instructions are in the [audit guide](../how-to/audit-macos-account-setup.md#inspect-the-enrollment-and-jobs).

## Startup and Screen Sharing

Glass requires Screen Sharing to have been enabled by the user for **Start Environment**. It does
not enable that host-wide service, install a VNC password, or replace its allowed-user policy.
Apple documents the service and its network access in
[Turn Mac screen sharing on or off](https://support.apple.com/guide/mac-help/turn-screen-sharing-on-or-off-mh11848/mac).

The startup client connects to `127.0.0.1:5900`, checks the connected Apple service's process identity,
and uses the Apple authentication handshake implemented in
[desktop_transport.rs](../../crates/glass-macos/src/desktop_transport.rs). The credential block is
encrypted before transmission. This is a local protocol connection, not a claim that the entire
Screen Sharing protocol is TLS. Glass's choice of a loopback destination does not restrict the
interfaces on which macOS itself offers Screen Sharing.

The client selects a separate desktop; it does not request screen frames or send keyboard, pointer
or clipboard events through that connection. Application interaction uses the registered Glass worker.

When the selected account lacks service access, Glass records its original membership and the
existing group's identity before a temporary change. Existing membership is preserved. If a crash
makes ownership of an added entry ambiguous, Glass retains the record and reports the problem
rather than removing an unconfirmed entry. The [manual access review](../how-to/audit-macos-account-setup.md#review-interrupted-screen-sharing-access)
describes how to inspect it. Startup recovery contains no password to replay.

## Credential lifetime and persistence

The GUI uses secure text fields and clears them when the account/password dialog finishes. Rust
password buffers use `Zeroizing`; `Password` has no serialization or debug formatter. Account
creation and desktop startup transfer password bytes over a local Unix socket only after peer
identity checks. The normal side authenticates the signed root helper; the helper checks the signed
normal-user peer. The temporary directory has mode `0700` and the socket has mode `0600`.

The socket is an IPC endpoint, **not a password file**. Its directory/socket are normally removed
when the operation ends. Interrupted processes can leave temporary endpoints, but the protocol does
not write password payloads into those filesystem nodes.

For creation, OpenDirectory receives the password to initialise or verify the selected account.
macOS must retain its normal account authentication data, just as it does for an account created in
System Settings. This is different from Glass retaining the password for later use. For startup,
the credential block exists for the local login operation; a later explicit startup may need the
password again. Glass does not add these passwords to Keychain, put them in shell arguments, or
write them into its enrollment, startup or recovery records.

This does not promise perfect erasure of every copy in process memory: secure text fields, native
`NSString` objects and operating-system APIs can hold their own copies. In particular,
[account_setup.m](../../crates/glass-macos/src/account_setup.m) makes a native string copy for
OpenDirectory. Zeroising Rust-owned buffers does not establish what every framework or the OS
retains in memory.

Source for this path:

- Secure fields: `create_account_dialog` and `start_account_dialog` in [setup_window.rs](../../crates/glass-macos/src/setup_window.rs).
- Non-serializable password type and credential-free creation receipts: [macos_account.rs](../../crates/glass-mcp/src/macos_account.rs).
- Framed socket exchange and peer checks: [account_creation.rs](../../crates/glass-mcp/src/macos_environment/account_creation.rs), [private_session.rs](../../crates/glass-macos/src/private_session.rs).
- Startup record fields and credential use: [desktop_start.rs](../../crates/glass-mcp/src/macos_environment/desktop_start.rs), [desktop_transport.rs](../../crates/glass-macos/src/desktop_transport.rs).

## Records left on disk

`OWNER_UID` is the normal account's numeric UID. Paths below the home directory belong to that
normal account unless otherwise stated. Metadata can be readable without being writable; these
records are not all secret files.

| Location | Contents |
| --- | --- |
| `/Library/Application Support/Glass/accounts/OWNER_UID/account.plist` | Version, normal owner UID, development account name/UID/GID/UUID/home. No password. |
| The same directory's `worker.requirement` | Public code-signing requirement used to authenticate the worker; not a credential. |
| The same directory's `worker.sock`, `disabled`, and lock/staging records in the accounts tree | Local activation endpoint and registration/recovery state. No saved login password. |
| `/Library/Application Support/Glass/account-creation/ACCOUNT.plist` | Creation identity and `created`/`password_set` booleans. `password_set` is a checkpoint flag, not a password or password hash. |
| `/Library/Application Support/Glass/accounts/.desktop-start-OWNER_UID.json` | Account/group identity, original membership, grant intent/confirmation and authentication-dispatch flag. No password or encrypted credential block. Removed after confirmed cleanup. |
| `~/Library/Application Support/glass/macos/permission-recovery/pending-setup.json` | Explicit setup intent: attempt ID, owner, account/identity, creation choice, integration and address. No password. |
| The same recovery directory's `pending.jsonl` | Workspace paths, prior permission/access metadata, file/process identities and output reconciliation state needed for recovery. No setup/login password. |
| `DEVELOPMENT_HOME/Library/LaunchAgents/tech.fixedwidth.glass.worker.plist` and `tech.fixedwidth.glass.onboarding.plist` | Signed executable path, internal role, normal owner UID, activation/session settings. No credentials. |
| `~/Library/LaunchAgents/tech.fixedwidth.glass.plist` and `~/Library/Logs/GlassMcp/` | Optional normal-user HTTP startup integration and server logs. |

The current layout is per normal user; older previews used registration files directly under
`/Library/Application Support/Glass`. See the existing [manual unenrollment procedure](../how-to/setup-macos.md#unenroll-by-hand)
for both layouts. Deleting recovery records is not equivalent to restoring permissions.

## Privilege boundaries and verification limits

The normal app/server runs as the normal user. Registered workers and development builds/apps run
as the selected standard user. The root roles above are transient; this feature does not install a
persistent root LaunchDaemon or a passwordless `sudo` rule. Routine application runs use the
registered worker and temporary, journalled workspace access rather than asking for root for every
build. The [workspace access implementation](../../crates/glass-mcp/src/workspace_permissions.rs)
and [permission journal](../../crates/glass-mcp/src/permission_journal.rs) describe those changes.

Administrator approval nevertheless authorises a root process. The restrictions described here are
checks in Glass's code, not a narrower privilege enforced by the password dialog. A valid signature
or notarisation ticket identifies/validates an app; it does not prove that its source is harmless or
that a binary was built from the source you read. The [audit guide](../how-to/audit-macos-account-setup.md)
shows how to inspect the installed identity and compare the corresponding version/source.

These password-handling statements concern Glass's setup/login credentials. Arbitrary build
commands, application output, requested screenshots or other tool data can contain secrets.
[Session traces](session-trace.md) retain selected tool arguments/results when enabled; excluding
setup passwords does not make arbitrary application data secret-free. The selected workspace can
itself contain sensitive files; sharing it is not a credential filter. The normal account's home and credentials are not copied
to the development account.
