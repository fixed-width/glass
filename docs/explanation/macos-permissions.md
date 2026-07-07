# The macOS permission model

macOS gates screen capture and input injection behind its privacy system (TCC), and the way those
grants are recorded shapes how glass is packaged and run on the Mac. Understanding this model explains
*why* macOS glass ships as a signed app plus a LaunchAgent rather than a plain binary — and why, once
set up, it never asks again.

glass needs two one-time grants before it can drive anything:

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
rebuilding, moving the app, or restarting the LaunchAgent never re-prompts.

The step-by-step for each path is in the setup and build guides:
[how-to/setup-macos.md](../how-to/setup-macos.md) and
[how-to/build-from-source.md](../how-to/build-from-source.md).
