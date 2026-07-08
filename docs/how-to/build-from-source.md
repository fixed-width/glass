# Build glass from source

End users don't need this: Linux and Windows releases attach prebuilt binaries, and macOS ships a
notarized `.dmg` ([setup-macos.md](setup-macos.md)). This guide is for **contributors** and anyone
running an unreleased checkout.

## Build (all platforms)

```bash
git clone https://github.com/fixed-width/glass
cd glass
cargo build --release -p glass-mcp        # → target/release/glass-mcp
```

glass pins a nightly toolchain in `rust-toolchain.toml`, which rustup installs automatically on the
first build — there's no toolchain to choose. On **macOS**, building additionally needs the Xcode
Command Line Tools (the notarized `.dmg` does not):

```bash
xcode-select --install
```

The `objc2-*` crates that bind Cocoa/CoreGraphics/CoreFoundation need the macOS SDK and `clang` the
CLT provides at their build-time link step; without it, `cargo build` fails there.

On Linux and Windows the built binary is all you need — return to the [Linux](setup-linux.md) or
[Windows](setup-windows.md) setup guide to install the runtime deps and connect an agent. macOS needs
the signing + LaunchAgent steps below.

## macOS: sign the app and run it as a LaunchAgent

Building from source, you reproduce by hand what the `.dmg` double-click does for you: establish a
stable signing identity, sign the app, install it as a LaunchAgent (its own responsible process), and
grant the two permissions. Because the grants key to a stable identity, this is a **one-time** step —
rebuilds and restarts never re-prompt. The *why* behind all of this is
[the permission model](../explanation/macos-permissions.md).

### Create a signing identity

Any code-signing identity works — it doesn't need to be trusted by Apple or tied to an Apple Developer
account (Developer ID / notarization only matter for distributing to *other* people, not for TCC
grants on your own machine).

**GUI (simplest, when you have a keyboard in front of you):**

1. Open **Keychain Access**.
2. **Keychain Access → Certificate Assistant → Create a Certificate…**
3. Name it (e.g. `glass-mcp signing`); **Identity Type: Self Signed Root**; **Certificate Type: Code
   Signing**.
4. **Create**. It lands in your login keychain, ready for `codesign -s`.

**CLI (headless boxes, or scripting the setup)** — the same identity scripted with `openssl` +
`security` into a **dedicated keychain** (so it never touches your login keychain). This is *not* fully
turnkey-headless: the trust step (step 3) triggers a one-time authorization prompt, so plan for a
console you can click once, or use the admin-domain / pre-trusted-cert variant noted inline.

```bash
KEYCHAIN="$HOME/Library/Keychains/glass-signing.keychain-db"
KEYCHAIN_PASSWORD="$(openssl rand -base64 24)"   # only unlocks this one keychain
WORKDIR="$(mktemp -d)"

# 1. A self-signed cert carrying the Code Signing extended key usage codesign requires. Extensions
#    come from a -config file (not -addext), so this works with both real OpenSSL and the LibreSSL
#    macOS ships as /usr/bin/openssl.
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
# Since macOS Sierra, codesign also needs this explicit ACL grant, or it hits a keychain-access
# prompt with no one there to click it:
security set-key-partition-list -S apple-tool:,apple:,codesign: -s \
  -k "$KEYCHAIN_PASSWORD" "$KEYCHAIN"

# 3. Trust the cert for code signing. Importing the key pair is NOT enough: until it's trusted,
#    `security find-identity -p codesigning` reports "0 valid identities". This step is the one that
#    is NOT headless — it triggers an authorization prompt; on a box with no console it fails with
#    "SecTrustSettingsSetTrustSettings: The authorization was denied…". Run it where you can click
#    once (user trust domain, below), or on a headless/CI box use the admin domain
#    (`sudo security add-trusted-cert -d …`), or start from a cert already trusted for code signing.
security add-trusted-cert -p codeSign -k "$KEYCHAIN" "$WORKDIR/cert.pem"

rm -rf "$WORKDIR"
```

Verify the identity landed and is usable:

```bash
security find-identity -v -p codesigning "$KEYCHAIN"
```

If `find-identity -p codesigning` reports **"0 valid identities"**, the trust step didn't take — re-run
it where you can authorize it, or use the admin-domain variant. If `codesign` still can't use the
identity, fall back to the GUI method — it establishes the trust for you.

### Build and sign the app

```bash
./packaging/macos/build-app.sh --identity "glass-mcp signing"
# -> target/macos-app/GlassMcp.app
```

`--identity` is required; `build-app.sh` also takes the keychain explicitly (`--keychain`), so nothing
needs to be added to the default keychain search list. See
[packaging/macos/README.md](../../packaging/macos/README.md) for the rest of the flags (custom bundle
id, version overrides). Confirm the signature:

```bash
codesign -dvv target/macos-app/GlassMcp.app   # want NO "adhoc" / "not signed"
```

### Grant the permissions and install the LaunchAgent

A permission *request* fired from a terminal command is attributed to the **terminal**, not the app,
so the reliable path is to run glass as a **LaunchAgent** (its own responsible process) and enable
`GlassMcp.app` in the two Privacy panes by hand.

1. **Install the LaunchAgent** — fill the shipped plist template with your app path and home directory
   and load it:

   ```bash
   APP="$PWD/target/macos-app/GlassMcp.app/Contents/MacOS/glass-mcp"
   PLIST="$HOME/Library/LaunchAgents/tech.fixedwidth.glass.plist"
   sed -e "s|/Applications/GlassMcp.app/Contents/MacOS/glass-mcp|$APP|" \
       -e "s|/Users/YOU|$HOME|g" \
       packaging/macos/tech.fixedwidth.glass.plist > "$PLIST"
   # A LaunchAgent's default cwd is the read-only "/"; glass writes capture baselines under its cwd,
   # so point it at a writable path:
   /usr/libexec/PlistBuddy -c "Add :WorkingDirectory string $HOME" "$PLIST"
   mkdir -p "$HOME/Library/Logs/GlassMcp"
   launchctl bootstrap "gui/$(id -u)" "$PLIST"
   ```

   The shipped template runs `glass-mcp serve --http --menubar`, so this LaunchAgent shows the same
   **`glass ●`** menu-bar item as the `.dmg` install. Running `glass-mcp serve --http` directly (without
   `--menubar`) stays headless: no menu bar, MCP served silently.

2. **Enable `GlassMcp.app` in both Privacy panes.** In **System Settings → Privacy & Security**, open
   **Screen Recording** and then **Accessibility**; in each, click **＋**, add `GlassMcp.app` (the
   bundle under `target/macos-app/`), and turn it on.

3. **Reload the agent so it re-reads the grants** (a grant enabled while the agent is running isn't
   visible until it restarts):

   ```bash
   launchctl kickstart -k "gui/$(id -u)/tech.fixedwidth.glass"
   ```

4. **Register it with your MCP client** ([connect-an-agent.md](connect-an-agent.md#over-http)):

   ```bash
   claude mcp add --transport http glass http://127.0.0.1:7300/
   ```

Your client's `glass_doctor` then reports the running agent's own grants — both should read granted.

### Managing the LaunchAgent by hand

```bash
launchctl bootout gui/$(id -u)/tech.fixedwidth.glass                                    # stop
launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tech.fixedwidth.glass.plist     # start again
launchctl print gui/$(id -u)/tech.fixedwidth.glass                                       # confirm running
```

No `sudo` is needed anywhere in this flow. The plist fields are documented in
[packaging/macos/README.md](../../packaging/macos/README.md).

## Troubleshooting: headless / SSH setup

Gotchas that only show up when driving a box over SSH (no one at the keyboard):

- **Enabling the grants needs a real console login, not just an SSH shell.** Enabling `GlassMcp.app`
  in the Screen Recording / Accessibility panes is a GUI action in System Settings, so it needs someone
  logged in at the screen (Screen Sharing works). Once granted, everything else — the LaunchAgent,
  `doctor`, driving apps — works headless over SSH.
- **Non-interactive `codesign` needs the keychain unlocked first.** A keychain created (or last
  unlocked) in an earlier login session is locked again by the time a bare SSH shell runs `codesign` —
  you'll see `errSecInternalComponent`. Unlock it explicitly first:
  `security unlock-keychain -p <password> <keychain>`. This has nothing to do with TCC/AX grants — it's
  the signing step failing to reach the private key.
- **Any `@main` Swift source** (including the `glass-macos` capture-test fixture,
  `crates/glass-macos/fixture/quadrants.swift`) **needs `swiftc -parse-as-library`.** Without it,
  `swiftc` assumes top-level statements and errors on an explicit `@main` entry point.
