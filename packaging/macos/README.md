# packaging/macos

Assembles `glass-mcp` into a signed macOS app bundle and (optionally) runs it as a
per-user LaunchAgent. This is a quick reference for the files here; see
[docs/running-on-macos.md](../../docs/running-on-macos.md) for the full setup
guide (creating a signing identity, running `glass-mcp setup` to grant Screen
Recording / Accessibility and install the run integration, connecting a client).

## Files

- **`Info.plist`** — the app bundle's `Info.plist` template. Ships with the
  production bundle id (`tech.fixedwidth.glass`) and `LSUIElement` set, so
  glass-mcp has no Dock icon and no standard app menu but **does** show a menu-bar
  status item at runtime (`NSStatusItem`, via `--menubar` — see below). (Not
  `LSBackgroundOnly`, which would suppress the status item.) `build-app.sh` copies
  this in and can override the identifier and version.
- **`build-app.sh`** — builds `glass-mcp --release`, assembles `GlassMcp.app`
  around it, and codesigns the bundle. Run `./build-app.sh --help` for flags;
  `--identity` is required (there's deliberately no ad-hoc-signing default — an
  ad-hoc signature's Designated Requirement isn't stable, so TCC grants wouldn't
  survive a rebuild).
- **`tech.fixedwidth.glass.plist`** — a `gui/<uid>` LaunchAgent template that runs
  the bundled binary as `glass-mcp serve --http --menubar`: a visible `glass ●`
  menu-bar item (endpoint, Copy endpoint, Restart, Quit glass) alongside the MCP
  server. `KeepAlive` is `false`, so **Quit glass** actually stops it — launchd
  won't relaunch the job until the next login (`RunAtLoad` starts it then). Copy
  the template, fill in the placeholders (your home directory, and the app path
  if you didn't install to `/Applications`), then load it.

## Build + sign

```bash
./packaging/macos/build-app.sh --identity "your signing identity"
# -> target/macos-app/GlassMcp.app
```

## Grant permissions + install the run integration

```bash
target/macos-app/GlassMcp.app/Contents/MacOS/glass-mcp setup
```

`glass-mcp setup` is the guided first-run: it requests Screen Recording +
Accessibility (opening the exact System Settings pane and polling for you),
then either installs this LaunchAgent (`--launchagent`, or answering yes when
asked) or leaves nothing installed for an attended/stdio client (`--no-launchagent`),
and confirms the result via `doctor`. See
[docs/running-on-macos.md](../../docs/running-on-macos.md) for the full flow,
including the flags (`--non-interactive`, `--addr`) and the Screen-Recording
relaunch nuance.

## Load / unload the LaunchAgent by hand

`glass-mcp setup --launchagent` does this for you (filling in the template below
and running `launchctl bootstrap`); use these commands directly to stop it,
reload it after moving the app, or debug a load failure:

```bash
mkdir -p ~/Library/LaunchAgents ~/Library/Logs/GlassMcp
cp packaging/macos/tech.fixedwidth.glass.plist ~/Library/LaunchAgents/
# edit ~/Library/LaunchAgents/tech.fixedwidth.glass.plist: replace /Users/YOU
# with your home directory (and the app path if not /Applications).

launchctl bootstrap gui/$(id -u) ~/Library/LaunchAgents/tech.fixedwidth.glass.plist
launchctl print gui/$(id -u)/tech.fixedwidth.glass    # confirm it's running

launchctl bootout gui/$(id -u)/tech.fixedwidth.glass   # stop + unload
```

No `sudo` is needed anywhere here — a LaunchAgent bootstrapped into your own
`gui/<uid>` domain is entirely user-scoped, and it's what keeps glass-mcp's
process launchd-parented (not SSH- or Terminal-parented), which is what makes its
TCC grants attach reliably to the signed binary itself.

## Release pipeline (maintainers)

Tagging `v*` runs the `macos` job in [`.github/workflows/release.yml`](../../.github/workflows/release.yml),
which builds a **universal2** (`arm64` + `x86_64`) `GlassMcp.app`, Developer-ID-signs it
(hardened runtime + secure timestamp, nested clip-shim dylib included), notarizes and
staples it via `xcrun notarytool` + `stapler`, and uploads
`glass-mcp-<tag>-universal-apple-darwin.zip` to the GitHub Release.

The job **skips cleanly** (no failure) until these repository secrets are set, so releases
still publish the Linux/Windows artifacts before macOS signing is available:

| Secret | Contents |
|--------|----------|
| `MACOS_DEVELOPER_ID_CERT_P12` | base64 of the "Developer ID Application" cert + key, exported as `.p12` |
| `MACOS_DEVELOPER_ID_CERT_PASSWORD` | the `.p12` export password |
| `MACOS_SIGN_IDENTITY` | the identity Common Name (`Developer ID Application: … (TEAMID)`) |
| `MACOS_NOTARY_API_KEY_P8` | base64 of the App Store Connect API key `.p8` (role: App Manager) |
| `MACOS_NOTARY_API_KEY_ID` | the API key ID |
| `MACOS_NOTARY_API_ISSUER_ID` | the App Store Connect issuer UUID |

Base64-encode a file for a secret with `base64 -i <file> | pbcopy`. To run the signing +
notarization steps locally instead of in CI, use `build-app.sh --universal --timestamp
--identity …` followed by `notarize.sh --app … --key … --key-id … --issuer …`.

The job's skip-gate probes only `MACOS_DEVELOPER_ID_CERT_P12`; configure all six together —
a partial configuration will run and fail at the first missing value rather than skip cleanly.
