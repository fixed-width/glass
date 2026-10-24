# The macOS permission model

macOS gates screen capture and input injection behind its privacy system (TCC), and the way those
grants are recorded shapes how glass is packaged and run on the Mac. Understanding this model explains
*why* macOS glass ships as a signed app plus a LaunchAgent rather than a plain binary, and how
compatible updates can retain existing grants.

glass needs two grants in the account where its backend runs:

- **Screen Recording** — to capture the screen.
- **Accessibility** — to manage windows and inject input.

## Grants are keyed to the responsible process's identity

Each grant is recorded against the **responsible process**, keyed to its **Designated Requirement**:
the combination of its bundle identifier and its code-signing certificate. Two consequences follow,
and they drive everything about the macOS setup.

**Rebuilding doesn't lose the grant.** Sign every build with the same identity and bundle id, and a
rebuilt binary inherits the grant automatically — no re-click. Change either the identity or the
bundle id and macOS treats it as a new app that needs a fresh grant. This is exactly why a notarized
`.app`, with a stable Apple-issued identity, keeps its grants across updates: the identity that the
grant is keyed to never changes.

**Who launches the process matters.** A bare `ssh user@mac 'glass-mcp …'` shell is attributed to
`sshd`, which cannot hold a grant. Running glass as a **LaunchAgent** in your own login session makes
`launchd` its parent instead — giving glass its own grantable, responsible process. That is the whole
reason macOS glass runs as a LaunchAgent rather than a binary you `ssh` in and start.

## Grants are read at launch

Both permissions are effectively **snapshotted when the process starts**. A grant enabled while glass
is already running is not visible to it until it restarts — Screen Recording binds its capability at
process start, and the Accessibility trust check is cached. This is why granting a permission is always
followed by a relaunch or a kickstart, and why the onboarding flow is built around relaunching rather
than live-polling.

## How the packaging follows from the model

The notarized `.dmg` automates all of this: double-clicking `GlassMcp.app` makes it its own
responsible process, so each permission request attributes to the app and the grant lands on it; once
both are granted, the app installs its own LaunchAgent. Building from source, you reproduce the same
steps by hand — establish a stable signing identity, sign the app, install it as a LaunchAgent, and
enable it in the two Privacy panes. Either way, because the grants are keyed to a stable identity,
compatible rebuilds and fresh launches can reuse the app's existing grants.

The step-by-step for each path is in the setup and build guides:
[how-to/setup-macos.md](../how-to/setup-macos.md) and
[how-to/build-from-source.md](../how-to/build-from-source.md).

## A separate development account

With `setup --account ACCOUNT`, the grant-bearing worker runs as a LaunchAgent in that account's
real Aqua login. The app's **Application setup…** window and CLI share the same coordinator. Enrollment
registers a permission helper from that one Glass bundle; it opens **glass permissions** in the
development account and reads grant changes in fresh Glass processes. The coordinator waits for
consent and the user's return without a user deadline, then checks live worker readiness and any
pending workspace recovery. The
[account setup guide](../how-to/setup-macos.md#apps-off-your-desktop-preview) gives the full sequence,
including verification and MCP-client configuration.

The MCP server in your ordinary account continues handling tools, audit records, traces and other backends;
its own TCC snapshot does not describe the private worker's permissions. Setup checks the worker,
and `glass_capabilities` with `scope: "session"` reports actual input support and interference.

A readiness probe authenticates the worker's kernel identity, signature and login without launching
an application or sharing a workspace. Application sessions retain an environment lease so setup
and disable cannot change their route while they still accept input. Doctor reports busy state
without interrupting those sessions.

Enrollment records Glass's signing identity in protected service state. Before sharing a workspace,
the owner authenticates the running worker through its kernel connection identity and code signature.
The worker requires hardened runtime without debugging or library-validation exceptions. Compatible
Glass updates keep the same signing identity; applications under development need no signing or
enrollment for this check.

Glass retains this account's login and grants between app runs. `glass_stop` releases owned work,
not the login. Switching to the account for inspection suspends private work through the live
attachment guard. Logging it out makes the worker unavailable until the next real login. Neither
setup nor uninstall edits the TCC database or removes the user's account.

Interrupted permission recovery checks recorded process births, the machine's boot identity and the
previous private login's lifetime. A surviving process or a retained login session can conservatively
defer recovery. Glass preserves the journal and reports the incomplete cleanup rather than restoring
access while earlier work may still be running.
