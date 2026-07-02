Running glass on a macOS host.

← [Back to README](../README.md)

## System requirements

- **macOS 14 or later**, developed and tested on **Apple Silicon**. Nothing in the
  backend is Apple-Silicon-specific, but Intel Macs aren't yet verified.
- glass drives apps in the **logged-in Aqua session** (the real desktop, not a
  headless framebuffer) and captures via ScreenCaptureKit / injects input via
  CGEvent — both gated by **macOS's privacy permissions (TCC)**, covered below.

## The permission model, in short

macOS requires two one-time grants — **Screen Recording** (capture) and
**Accessibility** (window management + input injection) — before glass can drive
anything. Each grant is recorded against the *responsible process*, keyed to its
**Designated Requirement**: the combination of its bundle identifier and its
code-signing certificate. That has two consequences that shape everything below:

- **Rebuilding doesn't lose the grant.** Sign every build with the same identity
  and bundle id, and a rebuilt binary inherits the grant automatically — no
  re-click needed. Change either one and macOS treats it as a new app, needing a
  fresh grant.
- **Who launches the process matters.** A bare `ssh user@mac 'glass-mcp ...'`
  shell is attributed to `sshd`, which can't hold a grant. Running glass-mcp as a
  **LaunchAgent** in your own login session makes `launchd` its parent instead —
  its own, grantable, responsible process. This is why macOS glass ships as a
  signed app + LaunchAgent rather than a plain binary you `ssh` in and run.

So the setup is: create a stable signing identity once, build+sign the app with
it, grant it Screen Recording + Accessibility once via System Settings, then run
it as a LaunchAgent from then on — rebuilds and restarts never need the dialog
again.

## 1. Create a signing identity

Any code-signing identity works — it doesn't need to be trusted by Apple or tied
to an Apple Developer account (Developer ID / notarization only matter for
Gatekeeper-distributing an app to *other* people, not for TCC grants on your own
machine). The simplest way to make one:

1. Open **Keychain Access**.
2. Menu bar → **Keychain Access → Certificate Assistant → Create a Certificate…**
3. Name it whatever you like (e.g. `glass-mcp signing`); **Identity Type: Self
   Signed Root**; **Certificate Type: Code Signing**.
4. Click **Create**. It lands in your login keychain, ready for `codesign -s`.

(For a headless box with no one at the keyboard, the same identity can be created
entirely from the command line into a dedicated keychain with `security
create-keychain` + `security import`; the interactive Keychain Access flow above
is simpler when you have a GUI session to run it in.)

## 2. Build and sign the app

```bash
./packaging/macos/build-app.sh --identity "glass-mcp signing"
# -> target/macos-app/GlassMcp.app
```

`--identity` is required — see [packaging/macos/README.md](../packaging/macos/README.md)
for the rest of the flags (custom bundle id, a non-default keychain, version
overrides). Confirm the signature:

```bash
codesign -dv target/macos-app/GlassMcp.app
```

## 3. Grant Screen Recording + Accessibility

`glass-mcp doctor` only *checks* grant status — it never triggers the OS consent
dialogs (that's deliberate: a diagnostic shouldn't pop dialogs). The dialogs
appear the first time glass actually **drives** something, so register the
signed binary with your agent and ask it to do something:

```bash
claude mcp add glass --scope user -- \
  "$(pwd)/target/macos-app/GlassMcp.app/Contents/MacOS/glass-mcp"
```

Then, from your agent: *"Use glass to launch TextEdit and take a screenshot."*
The first capture attempt prompts for **Screen Recording**; the first click or
keystroke prompts for **Accessibility**. Grant both in **System Settings →
Privacy & Security → Screen Recording** and **→ Accessibility** (add `GlassMcp`
if it isn't listed yet, or toggle it on if it is), then ask your agent to try
again. Confirm both stuck:

```bash
target/macos-app/GlassMcp.app/Contents/MacOS/glass-mcp doctor
```

You want the `[macos]` section all `✓`.

As long as you keep signing with this same identity and bundle id, this is a
**one-time step**: rebuilding, moving the app, or restarting the LaunchAgent
below never re-prompts.

## 4. Keep the Mac awake

glass captures and drives the real desktop, so a sleeping or locked display has
nothing to grab. On a box you're not actively using, hold it awake and unlocked:

```bash
caffeinate -d -i -s &
```

`glass-mcp doctor` checks this (the `display awake` line) and names this exact
command if the session is locked.

## 5. Run it

The interactive setup above (stdio, your agent spawns the signed binary
directly) is fine for driving apps from a Terminal you're sitting at. For a box
no one is watching, run it as a LaunchAgent instead — that's what keeps
glass-mcp launchd-parented so its grant stays attached to a stable process
rather than to whatever spawns it that day.

### Unattended (LaunchAgent + network transport) — the recommended model

```bash
mkdir -p ~/Library/LaunchAgents ~/Library/Logs/GlassMcp
cp packaging/macos/tech.fixedwidth.glass.plist ~/Library/LaunchAgents/
# Edit the copy: replace /Users/YOU with your home directory (and the app path
# if you didn't install GlassMcp.app to /Applications).

launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tech.fixedwidth.glass.plist
launchctl print gui/$(id -u)/tech.fixedwidth.glass   # confirm it's running
```

This starts `glass-mcp serve --http --addr 127.0.0.1:7300` — see
[packaging/macos/README.md](../packaging/macos/README.md) for the load/unload
commands. No `sudo` is needed anywhere in this flow.

Point a client at it:

```bash
# Same machine: connect straight to loopback — no token needed.
# From another machine, tunnel it first:
ssh -L 7300:127.0.0.1:7300 you@themac
```

Then point your MCP client at `http://127.0.0.1:7300`. Binding beyond loopback
follows the same fail-closed token rule as the other platforms — see the "Network
transport" section of [running-on-windows.md](running-on-windows.md) for the
`gen-token` / `--token-file` flow, which works identically here.

Stop it with:

```bash
launchctl bootout gui/$(id -u)/tech.fixedwidth.glass
```

## Tools available on macOS

Once granted (step 3), the agent has `glass_start`, `glass_screenshot`,
`glass_click`, `glass_type`, `glass_wait_stable`, `glass_diff`, `glass_logs`,
`glass_list_windows`, `glass_select_window`, and `glass_doctor`. The
accessibility-tree tools — `glass_a11y_snapshot`, `glass_a11y_marks`,
`glass_click_element`, and `glass_set_value` — also work on macOS, reading and
driving the AXUIElement tree; they need the same Accessibility grant from step 3
above (no separate permission).

- **Clipboard** (`glass_clipboard_get`, `glass_clipboard_set`) — read and write the system pasteboard at
  `sandbox: off`. Under containment (`default`/`strict`) the clipboard is isolated **and
  working** for an app not built with hardened runtime (typically a debug or unsigned build), and
  `Unsupported` for a hardened-runtime app — see [Clipboard isolation](#clipboard-isolation) below.

## Sandboxing

glass **sandboxes every launched app by default** on macOS via the OS's built-in
**Seatbelt** sandbox (`sandbox_init`) — nothing to install. `glass_start`'s `sandbox`
arg (or `GLASS_SANDBOX`) selects the level:

- **`default`** (the default) — filesystem and process containment; network allowed.
- **`strict`** — same containment, plus outbound network blocked.
- **`off`** — no containment; the app runs unconfined.

`default` and `strict` are **fail-closed**: if `sandbox_init` rejects the generated
profile, `glass_start` errors rather than launching the app unconfined. `off` is the
explicit escape hatch. The `sandbox` level governs the **launched app only** — the
optional `build` step always runs unsandboxed, with your full developer environment.

### The profile

The generated Seatbelt profile is **deny-default**: everything not explicitly allowed is
denied. The filesystem model is not an allowlist of system directories — the **whole
filesystem is readable (read-only)**, except your **home directory** (`/Users`), which is
denied so secrets (`~/.ssh` and the rest of your home) stay hidden. Your working directory
and the launched program's own directory are re-allowed for reads even when they live
inside your home, so a project checked out under `~/` still works. **Writes** are limited
to the working directory plus the usual scratch/cache roots (`/private/var/folders`,
`/private/tmp`, `/tmp`, `/dev`) — nothing under your home is ever write-allowed.
`mach-register` is allowed so the app can still serve its accessibility tree to
`glass_a11y_snapshot`/`glass_a11y_marks`/`glass_click_element`/`glass_set_value` — a
sandboxed app that can't register with the window server returns an empty AX tree.

### Clipboard isolation

Under `default`/`strict`, `glass_clipboard_get`/`glass_clipboard_set` are isolated from your
real pasteboard **but still work** for an app that isn't built with Apple's **hardened runtime**
— typically a debug or unsigned build, like the app you're developing: at launch glass injects a
small shim (`DYLD_INSERT_LIBRARIES`) that swizzles `+[NSPasteboard generalPasteboard]` inside the
app process, redirecting it to a private, per-session named pasteboard that glass reads and
writes from the host side. The app copies/pastes normally against that private pasteboard;
your real clipboard is never touched. glass confirms the swizzle actually took (a sentinel
item written to the private pasteboard) before routing to it, so an injection that silently
failed doesn't get mistaken for a working bridge.

If the target runs under Apple's **hardened runtime** (App Store or notarized apps),
`DYLD_INSERT_LIBRARIES` injection is stripped by the OS and the swizzle can't take. Such apps
can't be injected, so the Seatbelt
profile denies them the real pasteboard service (`com.apple.pasteboard.1`) outright and
`glass_clipboard_get`/`glass_clipboard_set` return `Unsupported` — fail-closed at the profile
level, not a silent fall-back to the shared system clipboard.

For a non-hardened app whose shim confirmation doesn't arrive (injection silently failed),
`glass_clipboard_get`/`glass_clipboard_set` also return `Unsupported`: glass decides that
route after launch, from the missing sentinel, and never bridges to the real clipboard. Note
this is a glass-side gate — the profile *does* allow the pasteboard service for a non-hardened
target (that decision is made before launch, before confirmation is possible), so a silently-
failed injection leaves the app itself still able to reach the real pasteboard directly. In
practice injection either takes or the launch fails loudly, so this window is rare.
At `sandbox: off` clipboard access works normally against the real pasteboard (see
[Tools available on macOS](#tools-available-on-macos) above).

### Known limits

- **The mach allow-list is broad** (`(allow mach-lookup)`) — narrowing it to only the
  services a given app actually needs is a hardening follow-up.
- **The clipboard shim covers `NSPasteboard` only.** It swizzles the high-level
  `+[NSPasteboard generalPasteboard]` entry point that most apps use; an app that reaches the
  pasteboard through lower-level Carbon/Core Services APIs isn't redirected. Hardened-runtime
  apps can't be injected at all, so their clipboard access is `Unsupported` rather than
  isolated-but-working.
- **Electron apps may be able to escape their own in-app sandbox** under this
  containment. Seatbelt contains the process from the outside; an Electron app's
  internal renderer/main-process sandboxing is a separate mechanism this doesn't harden.
- `sandbox_init` is **deprecated** by Apple in favor of the App Sandbox entitlement
  model, but it remains present and functional on every currently-supported macOS
  release — it's what underpins App Sandbox itself, and Chromium uses it too.

Check `glass-mcp doctor`'s `[sandbox]` section for the live Seatbelt availability check.

## Troubleshooting: headless / SSH setup

Two gotchas that only show up when you're driving a box over SSH (no one at the
keyboard), e.g. re-signing after a code change or building the Swift capture-test
fixture:

- **Non-interactive `codesign` needs the keychain unlocked first.** A keychain
  created (or last unlocked) in an earlier login session is locked again by the time
  a bare SSH shell runs `codesign` — you'll see `errSecInternalComponent` rather
  than an obviously-keychain-shaped error. Unlock it explicitly before signing:
  `security unlock-keychain -p <password> <keychain>` (the login keychain if you
  didn't pass `--keychain` to `build-app.sh`). This has nothing to do with TCC/AX
  grants — it's the code-signing step itself failing to reach the private key.
- **Any `@main` Swift source (including the `glass-macos` capture-test fixture,
  `crates/glass-macos/fixture/quadrants.swift`) needs `swiftc -parse-as-library`.**
  Without it, `swiftc` assumes top-level statements (the `main.swift` convention) and
  errors on an explicit `@main` entry point.

---

## Problems?

`glass-mcp doctor` diagnoses most setup issues and prints a remedy for each
failed check. Bug reports and questions:
<https://github.com/fixed-width/glass/issues>.
