Running glass on a macOS host.

← [Back to README](../README.md)

## Install (recommended): the notarized `.dmg`

Every tagged release attaches a **notarized, universal** `GlassMcp.app` in a `.dmg` to the
[GitHub Releases page](https://github.com/fixed-width/glass/releases). Because it's signed
and notarized, macOS opens it without a Gatekeeper detour, and first-run is a double-click:

1. **Download** the `.dmg` from Releases and open it.
2. **Drag `GlassMcp.app` to `/Applications`.**
3. **Double-click `GlassMcp.app`.** A **permission checklist window** appears, listing
   **Accessibility** and **Screen Recording**, each with a ✓/○ status and its own **Open
   Settings** button.
4. Click **Open Settings** for each permission, **one at a time**. It adds `GlassMcp.app` to
   that permission's pane under **System Settings → Privacy & Security** and opens the pane
   so you can turn it on. Because the app is asking for *itself* (it's its own responsible
   process), the grant lands on `GlassMcp.app` directly — no manual `＋`-add needed. Granting
   **Screen Recording relaunches `GlassMcp.app` automatically** (macOS quits and reopens it
   to pick up the grant) — that relaunch is expected, not an error. If you grant the two out
   of that order, or a grant made elsewhere doesn't show up, click **Re-check**. Grants are
   **one-time**: keyed to the app's signed identity, they survive restarts and updates (see
   [The permission model, in short](#the-permission-model-in-short)).
5. Once both are granted, glass relaunches into its ready state — the checklist doesn't
   reappear, and the **`glass ●`** menu bar shows instead. Along the way it **installs its
   LaunchAgent** (so it keeps serving across logins and restarts).

That's the whole setup. The MCP endpoint is:

```
http://127.0.0.1:7300/
```

Head to [Connect your agent](#connect-your-agent).

### The menu-bar item

Once granted, `GlassMcp.app` runs as a **visible menu-bar app** — look for **`glass ●`** in
the menu bar. It starts automatically at login (via the LaunchAgent) and its dropdown shows:

- the **served endpoint** (or a notice if another glass is already bound to it),
- **Copy endpoint** — copy the MCP endpoint to your clipboard,
- **Restart** — restart the background agent (e.g. after a fresh grant),
- **Quit glass** — stop the agent,
- **Uninstall glass…** — stop glass from starting at login and quit it now (asks you to
  confirm first; see [Uninstall](#uninstall)).

glass is deliberately never a silent, invisible background process: it holds Screen
Recording and Accessibility, so it stays something you can always see and stop. **Quit
glass** actually stops it — the LaunchAgent won't relaunch it behind your back — and it
stays stopped until you next log in or relaunch `GlassMcp.app` yourself.

## Connect your agent

The onboarded LaunchAgent serves MCP over **Streamable HTTP** at the endpoint shown in the
`glass ●` menu bar:

```
http://127.0.0.1:7300/
```

It binds **loopback-only** (`127.0.0.1`), so there is **no bearer token** — a client on the
same machine connects to the bare URL. (The same endpoint is served whether the LaunchAgent
was installed by the `.dmg` double-click above or by the [Build from
source](#build-from-source-contributors) recipe below.)

### Per-client MCP registration

Register that endpoint with whatever MCP client you use. glass works with any MCP client;
the two below are just examples.

**Claude Code:**

```bash
claude mcp add --transport http glass http://127.0.0.1:7300/
```

**Generic MCP client (JSON config):**

```json
{
  "mcpServers": {
    "glass": { "type": "http", "url": "http://127.0.0.1:7300/" }
  }
}
```

Your client's `glass_doctor` tool then reports the running agent's own grants — both Screen
Recording and Accessibility should read granted.

**From another machine,** tunnel first (`ssh -L 7300:127.0.0.1:7300 you@themac`), then point
the client at `http://127.0.0.1:7300/`. Binding beyond loopback follows the same fail-closed
token rule as the other platforms — see the "Network transport" section of
[running-on-windows.md](running-on-windows.md) for the `gen-token` / `--token-file` flow,
which works identically here.

**If you prefer stdio** to the HTTP LaunchAgent, register the app's binary directly:

```bash
claude mcp add glass --scope user -- \
  /Applications/GlassMcp.app/Contents/MacOS/glass-mcp
```

Note that a stdio server is launched by — and attributed to — your MCP client, so the grants
must attach to *that* process; the LaunchAgent model above exists precisely so glass holds its
own grants (see [The permission model, in short](#the-permission-model-in-short)).

### Optional: the `glass-mcp` CLI

The `.dmg` doesn't put `glass-mcp` on your `$PATH` — it's a binary inside the app bundle, at
`/Applications/GlassMcp.app/Contents/MacOS/glass-mcp`. You don't need it for MCP (the menu-bar
LaunchAgent and your client registration above are the whole setup), but it's handy for
checking things from a terminal. To use it as a plain command, symlink it onto `$PATH` once:

```bash
sudo ln -s /Applications/GlassMcp.app/Contents/MacOS/glass-mcp /usr/local/bin/glass-mcp
```

Then:

```bash
glass-mcp status   # is glass running, and at what endpoint (reads /healthz)
glass-mcp doctor    # checks the environment, with a remedy for any gap
```

## System requirements

- **macOS 14 or later**, developed and tested on **Apple Silicon**. The shipped `.dmg` is a
  universal binary (arm64 + x86_64), but Intel Macs aren't yet verified.
- glass drives apps in the **logged-in Aqua session** (the real desktop, not a
  headless framebuffer) and captures via ScreenCaptureKit / injects input via
  CGEvent — both gated by **macOS's privacy permissions (TCC)**, covered below.
- **Building from source additionally needs the Xcode Command Line Tools** (the notarized
  `.dmg` does not):

  ```bash
  xcode-select --install
  ```

  The `objc2-*` crates that bind Cocoa/CoreGraphics/CoreFoundation need the macOS SDK
  and `clang` the CLT provides at their build-time link step; without it, `cargo build`
  fails there. See [Build from source (contributors)](#build-from-source-contributors).

## The permission model, in short

macOS requires two one-time grants — **Screen Recording** (capture) and
**Accessibility** (window management + input injection) — before glass can drive
anything. Each grant is recorded against the *responsible process*, keyed to its
**Designated Requirement**: the combination of its bundle identifier and its
code-signing certificate. That has two consequences that shape everything below:

- **Rebuilding doesn't lose the grant.** Sign every build with the same identity
  and bundle id, and a rebuilt binary inherits the grant automatically — no
  re-click needed. Change either one and macOS treats it as a new app, needing a
  fresh grant. (This is why a notarized `.app`, with a stable Apple-issued identity,
  keeps its grants across updates.)
- **Who launches the process matters.** A bare `ssh user@mac 'glass-mcp ...'`
  shell is attributed to `sshd`, which can't hold a grant. Running glass-mcp as a
  **LaunchAgent** in your own login session makes `launchd` its parent instead —
  its own, grantable, responsible process. This is why macOS glass ships as a
  signed app + LaunchAgent rather than a plain binary you `ssh` in and run.

With the notarized `.dmg`, all of this is automatic: double-clicking `GlassMcp.app` makes it
its own responsible process, so each checklist **Open Settings** request attributes to the
app and lands on it, and once both are granted the app installs its own LaunchAgent (see
[Install](#install-recommended-the-notarized-dmg)).
Building from source, you do the same steps by hand: create a stable signing identity once,
build + sign the app, install it as a LaunchAgent, and enable it in the Screen Recording +
Accessibility panes (see [Build from source](#build-from-source-contributors)). Either way,
once the grants are in, rebuilds and restarts never need the checklist again.

## Keep the Mac awake

glass captures and drives the real desktop, so a sleeping or locked display has
nothing to grab. On a box you're not actively using, hold it awake and unlocked:

```bash
caffeinate -d -i -s &
```

`glass-mcp doctor` checks this (the `display awake` line) and names this exact
command if the session is locked.

## Build from source (contributors)

End users don't need any of this — they install the notarized `.dmg`
([Install](#install-recommended-the-notarized-dmg)). This section is for **contributors** and
anyone running an unreleased checkout: it reproduces, by hand, what the double-click flow does
for you — establish a signing identity, build + sign the app, install it as a LaunchAgent, and
grant the two permissions.

### Create a signing identity

Any code-signing identity works — it doesn't need to be trusted by Apple or tied
to an Apple Developer account (Developer ID / notarization only matter for
Gatekeeper-distributing an app to *other* people, not for TCC grants on your own
machine). **This step is only for building from source** (contributors, or
running an unreleased checkout) — end users install the notarized, pre-signed `.app` from the
`.dmg` and won't need a signing identity of their own at all.

#### GUI (simplest, when you have a keyboard in front of you)

1. Open **Keychain Access**.
2. Menu bar → **Keychain Access → Certificate Assistant → Create a Certificate…**
3. Name it whatever you like (e.g. `glass-mcp signing`); **Identity Type: Self
   Signed Root**; **Certificate Type: Code Signing**.
4. Click **Create**. It lands in your login keychain, ready for `codesign -s`.

#### CLI (headless boxes, or scripting the setup)

The GUI flow above is the simplest when you're sitting at the Mac, and it
establishes the code-signing trust for you. The same identity can also be
**scripted** with `openssl` + `security` into a **dedicated keychain** (so it
never touches your login keychain) — useful when you're driving a box over SSH.
This is *not* fully turnkey-headless, though: the final trust step (step 3 below)
triggers a one-time authorization prompt, so plan for either a console you can
click once or the admin-domain / pre-trusted-cert variant noted there.

```bash
KEYCHAIN="$HOME/Library/Keychains/glass-signing.keychain-db"
KEYCHAIN_PASSWORD="$(openssl rand -base64 24)"   # only unlocks this one keychain
WORKDIR="$(mktemp -d)"

# 1. A self-signed cert carrying the Code Signing extended key usage codesign
#    requires. Extensions come from a -config file (not the newer -addext flag),
#    so this works with both real OpenSSL and the LibreSSL macOS ships as
#    /usr/bin/openssl.
cat > "$WORKDIR/codesign.cnf" <<'EOF'
[req]
distinguished_name = dn
x509_extensions = v3_req
prompt = no

[dn]
CN = glass-mcp signing

[v3_req]
basicConstraints = critical, CA:FALSE
keyUsage = critical, digitalSignature
extendedKeyUsage = critical, codeSigning
EOF
openssl req -x509 -newkey rsa:2048 -sha256 -days 3650 -nodes \
  -keyout "$WORKDIR/key.pem" -out "$WORKDIR/cert.pem" \
  -subj "/CN=glass-mcp signing" -config "$WORKDIR/codesign.cnf"

# 2. Bundle it as a PKCS#12 and hand it to a fresh keychain.
openssl pkcs12 -export -out "$WORKDIR/cert.p12" \
  -inkey "$WORKDIR/key.pem" -in "$WORKDIR/cert.pem" -passout "pass:$KEYCHAIN_PASSWORD"

security create-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"
security import "$WORKDIR/cert.p12" -k "$KEYCHAIN" -P "$KEYCHAIN_PASSWORD" \
  -T /usr/bin/codesign -T /usr/bin/security
# Since macOS Sierra, codesign also needs this explicit ACL grant, or it hits a
# keychain-access prompt with no one there to click it:
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
  -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN"

# 3. Trust the cert for code signing. Importing the key pair is NOT enough on its
#    own: until it's trusted, `security find-identity -p codesigning` reports "0
#    valid identities" and `codesign -s "glass-mcp signing"` fails with "no identity
#    found". This step is the one that is NOT headless — it triggers an authorization
#    prompt, and on a box with no console it fails with "SecTrustSettingsSetTrust-
#    Settings: The authorization was denied since no user interaction was possible."
#    So run it where you can click once (user trust domain, below), or on a truly
#    headless/CI box use the admin domain (`sudo security add-trusted-cert -d …`), or
#    start from a cert already trusted for code signing.
security add-trusted-cert -p codeSign -k "$KEYCHAIN" "$WORKDIR/cert.pem"

rm -rf "$WORKDIR"
```

`build-app.sh` takes the keychain explicitly (`--keychain`), so nothing needs to
be added to the default keychain search list:

```bash
security unlock-keychain -p "$KEYCHAIN_PASSWORD" "$KEYCHAIN"   # each new shell/SSH session
./packaging/macos/build-app.sh --identity "glass-mcp signing" --keychain "$KEYCHAIN"
```

Verify the identity landed and is usable before relying on it:

```bash
security find-identity -v -p codesigning "$KEYCHAIN"
codesign -dvv target/macos-app/GlassMcp.app   # want NO "adhoc" / "not signed"
```

If `find-identity -p codesigning` reports **"0 valid identities"**, the trust step
(step 3) didn't take — its prompt was denied or skipped; re-run it where you can
authorize it, or use the admin-domain variant. (The "Troubleshooting: headless /
SSH setup" section further down covers the `errSecInternalComponent` you'll hit if
the keychain is locked when `codesign` runs.) If `codesign` still can't find or use
the identity, fall back to the GUI method above — it establishes the trust for you,
and either way this is a one-time step.

### Build and sign the app

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

### Grant the permissions and install the LaunchAgent

The two grants have to land on `GlassMcp.app`'s own identity — and, as the permission
model above explains, a permission *request* fired from a terminal command is attributed
to the **terminal**, not to the app. So the reliable way, and the path for a
build-from-source checkout, is to run glass as a **LaunchAgent** (its own responsible
process) and enable `GlassMcp.app` in the two Privacy panes by hand.

1. **Install the LaunchAgent.** Fill the shipped plist template with your app path and
   home directory and load it:

   ```bash
   APP="$PWD/target/macos-app/GlassMcp.app/Contents/MacOS/glass-mcp"
   PLIST="$HOME/Library/LaunchAgents/tech.fixedwidth.glass.plist"
   sed -e "s|/Applications/GlassMcp.app/Contents/MacOS/glass-mcp|$APP|" \
       -e "s|/Users/YOU|$HOME|g" \
       packaging/macos/tech.fixedwidth.glass.plist > "$PLIST"
   # glass writes capture baselines under its working directory; a LaunchAgent's default
   # cwd is the read-only "/", so point it at a writable path:
   /usr/libexec/PlistBuddy -c "Add :WorkingDirectory string $HOME" "$PLIST"
   mkdir -p "$HOME/Library/Logs/GlassMcp"
   launchctl bootstrap "gui/$(id -u)" "$PLIST"
   ```

   The shipped template runs `glass-mcp serve --http --menubar`, so this LaunchAgent shows
   the same **`glass ●`** menu-bar item as the `.dmg` install (see [The menu-bar
   item](#the-menu-bar-item)). If you run `glass-mcp serve --http` directly, without
   `--menubar` — e.g. testing a build by hand, not via the LaunchAgent — it stays headless:
   no menu bar, no Dock icon, MCP served silently over HTTP.

2. **Enable `GlassMcp.app` in both Privacy panes.** In **System Settings → Privacy &
   Security**, open **Screen Recording** and then **Accessibility**; in each, click
   **＋**, add `GlassMcp.app` (the bundle under `target/macos-app/`), and turn it on.
   Adding it there keys the grant to the app's Designated Requirement — the process that
   actually captures the screen and injects input.

3. **Reload the agent so it re-reads the grants.** A grant enabled while the agent is
   already running isn't visible to it until it restarts — Screen Recording binds its
   capability at process start, and the Accessibility trust check is cached:

   ```bash
   launchctl kickstart -k "gui/$(id -u)/tech.fixedwidth.glass"
   ```

4. **Register it with your MCP client** (as in [Connect your agent](#connect-your-agent)):

   ```bash
   claude mcp add --transport http glass http://127.0.0.1:7300/
   ```

Your client's `glass_doctor` tool then reports the running agent's own grants — both
should read granted. As long as you keep signing with the same identity and bundle id,
this is a **one-time step**: rebuilding, moving the app, or restarting the LaunchAgent
never re-prompts.

### Managing the LaunchAgent by hand

The step above installs and starts the LaunchAgent; to stop, restart, or reload it manually
(e.g. after moving the app to a new path):

```bash
launchctl bootout gui/$(id -u)/tech.fixedwidth.glass                                 # stop
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tech.fixedwidth.glass.plist   # start again
launchctl print gui/$(id -u)/tech.fixedwidth.glass                                    # confirm it's running
```

No `sudo` is needed anywhere in this flow. See
[packaging/macos/README.md](../packaging/macos/README.md) for the plist template
and what each field means.

## Uninstall

From the **`glass ●`** menu bar, click **Uninstall glass…** (it asks you to confirm, since
uninstalling also quits glass). Or, from a terminal:

```bash
glass-mcp uninstall
```

Either one stops glass from starting at login: removes
`~/Library/LaunchAgents/tech.fixedwidth.glass.plist` and boots out the running job. Neither
touches the app bundle itself — drag `GlassMcp.app` to the Trash afterward to remove glass
entirely.

Without either (e.g. on a build that predates the menu item or the CLI), the equivalent by
hand:

```bash
launchctl bootout gui/$(id -u)/tech.fixedwidth.glass
rm ~/Library/LaunchAgents/tech.fixedwidth.glass.plist
```

then drag `GlassMcp.app` to the Trash.

## Tools available on macOS

Once both permissions are granted, the agent has `glass_start`,
`glass_screenshot`, `glass_click`, `glass_type`, `glass_wait_stable`, `glass_diff`,
`glass_logs`, `glass_list_windows`, `glass_select_window`, and `glass_doctor`. The
accessibility-tree tools — `glass_a11y_snapshot`, `glass_a11y_marks`,
`glass_click_element`, and `glass_set_value` — also work on macOS, reading and
driving the AXUIElement tree; they need the same Accessibility grant (no separate
permission).

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

A few gotchas that only show up when you're driving a box over SSH (no one at the
keyboard), e.g. re-signing after a code change or granting permissions:

- **Enabling the grants needs a real console login, not just an SSH shell.**
  Enabling `GlassMcp.app` in the Screen Recording / Accessibility panes by hand (the
  [Build from source](#build-from-source-contributors) manual-grant step) is a GUI action in
  System Settings, so it needs someone logged in at the screen — glass also needs a real GUI
  login to capture and drive anything. Do the one-time grant at the console (Screen Sharing
  works). Once granted, everything else — the `gui/<uid>` LaunchAgent, `doctor`, driving apps
  — works headless over SSH.
- **Non-interactive `codesign` needs the keychain unlocked first.** A keychain
  created (or last unlocked) in an earlier login session is locked again by the time
  a bare SSH shell runs `codesign` — you'll see `errSecInternalComponent` rather
  than an obviously-keychain-shaped error. Unlock it explicitly before signing:
  `security unlock-keychain -p <password> <keychain>` (the login keychain if you
  didn't pass `--keychain` to `build-app.sh`, or your dedicated signing keychain if
  you used the CLI cert recipe above). This has nothing to do with TCC/AX
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
</content>
</invoke>
