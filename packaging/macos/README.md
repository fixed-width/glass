# packaging/macos

Assembles `glass-mcp` into a signed macOS app bundle and (optionally) runs it as a
per-user LaunchAgent. This is a quick reference for the files here; see
[docs/running-on-macos.md](../../docs/running-on-macos.md) for the full setup
guide (creating a signing identity, running `glass-mcp setup` to grant Screen
Recording / Accessibility and install the run integration, connecting a client).

## Files

- **`Info.plist`** — the app bundle's `Info.plist` template. Ships with the
  production bundle id (`tech.fixedwidth.glass`) and `LSBackgroundOnly` set —
  glass-mcp is a headless agent with no windows, no menu bar, and nothing to show
  in the Dock. `build-app.sh` copies this in and can override the identifier and
  version.
- **`build-app.sh`** — builds `glass-mcp --release`, assembles `GlassMcp.app`
  around it, and codesigns the bundle. Run `./build-app.sh --help` for flags;
  `--identity` is required (there's deliberately no ad-hoc-signing default — an
  ad-hoc signature's Designated Requirement isn't stable, so TCC grants wouldn't
  survive a rebuild).
- **`tech.fixedwidth.glass.plist`** — a `gui/<uid>` LaunchAgent template that runs
  the bundled binary as `glass-mcp serve --http`. Copy it, fill in the
  placeholders (your home directory, and the app path if you didn't install to
  `/Applications`), then load it.

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
