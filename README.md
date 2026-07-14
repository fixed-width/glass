# glass

[![CI](https://github.com/fixed-width/glass/actions/workflows/ci.yml/badge.svg)](https://github.com/fixed-width/glass/actions/workflows/ci.yml)

A Rust [MCP](https://modelcontextprotocol.io) server that gives an AI coding agent a closed **build →
see → interact → debug** loop over external native GUI applications.

glass lets an agent launch a GUI app, capture what is on screen, inject mouse and keyboard input, read
the app's logs, and detect visual changes — so a coding agent can build and debug UI applications
independently instead of asking the user "does this look right?".

glass drives apps as an external black box, so it works with any native GUI app regardless of toolkit
or language. It has two Linux backends (**X11** and **Wayland**), a **Windows** backend, an
**Android** backend (an AVD emulator, driven over `adb` from any host), an **iOS** backend (native
apps in the Simulator over `xcrun simctl`, with input and the accessibility tree via `idb_companion`;
multi-touch gestures excepted), and a **macOS** backend, behind a platform-agnostic core.

## The loop in practice

Point an agent at a GUI app and it runs the whole cycle itself:

```jsonc
glass_start   { "build": "cargo build --release", "run": ["target/release/my-app"] }  // builds, then launches (sandboxed)
glass_screenshot                       // see the window
glass_click   { "x": 240, "y": 160 }   // interact
glass_wait_stable                      // let the render settle
glass_diff                             // what changed? changed_pct + bbox, as text — no image
glass_logs                             // read the app's stderr
```

`glass_diff` and the `glass_wait_for_*` tools return text only, so the routine checks between
screenshots cost no vision tokens. Why the loop is shaped this way:
[the build → see → interact → debug loop](docs/explanation/the-loop.md).

## Install at a glance

Download the latest build for your platform from the
[Releases page](https://github.com/fixed-width/glass/releases/latest), then set up your host:

- **Linux** — [docs/how-to/setup-linux.md](docs/how-to/setup-linux.md) (X11 or Wayland; `Xvfb` /
  `sway` + bubblewrap)
- **Windows** — [docs/how-to/setup-windows.md](docs/how-to/setup-windows.md) (a prebuilt `.exe` +
  Sandboxie)
- **macOS** — [docs/how-to/setup-macos.md](docs/how-to/setup-macos.md) (install the notarized `.dmg`;
  no build needed)
- **Android** — [docs/how-to/setup-android.md](docs/how-to/setup-android.md) (an AVD emulator, from any
  host)
- **iOS** — [docs/how-to/setup-ios.md](docs/how-to/setup-ios.md) (the Simulator, macOS host only)

Every asset is listed in [docs/reference/platforms.md](docs/reference/platforms.md#release-artifacts).
Prefer to compile, or on an architecture with no published asset? See
[docs/how-to/build-from-source.md](docs/how-to/build-from-source.md) — it is a single `cargo build`.

Then [connect glass to your agent](docs/how-to/connect-an-agent.md) and run `glass-mcp doctor` to check
the environment. New here? Follow [the tutorial](docs/tutorial/first-drive.md) for a guaranteed first
success.

## Drive it well — the `glass-drive` skill

glass needs no app integration and no skill to run, but an agent drives it far more reliably with the
open [glass-drive](docs/how-to/drive-glass-well.md) Agent Skill — it stops the agent spending its first
turns rediscovering the verify-cheaply-then-look loop. **Installing it is the single highest-leverage
thing you can add** when pointing an agent at glass.

## Platform support

**✓** supported · **◑** partial · **–** not supported · **🚧** planned.

<!-- KEEP IN SYNC with docs/reference/platforms.md (the canonical matrix) and the code. -->

| Capability | Linux (X11 + Wayland) | Windows | Android (AVD) | iOS (Simulator) | macOS |
|---|:--:|:--:|:--:|:--:|:--:|
| Capture · input · windows · clipboard · logs | ✓ | ✓ | ✓ | ✓ | ✓ |
| Accessibility (semantic addressing) | ✓ AT-SPI | ✓ UI Automation | ✓ UIAutomator | ✓ idb | ✓ AX |
| Containment / sandboxing | ✓ bubblewrap | ✓ Sandboxie | ✓ the emulator VM | ✓ the Simulator | ✓ Seatbelt |
| Display isolation (app off your desktop) | ✓ headless Xvfb / sway | ◑ virtual display · VM tier | ✓ headless emulator | ✓ headless simctl boot | 🚧 |

Full matrix, per-capability detail, and system requirements:
[docs/reference/platforms.md](docs/reference/platforms.md). Transport is MCP over stdio (default) or
network HTTP.

## Documentation

The full docs — tutorial, how-to guides, reference, and explanations — are under
**[`docs/`](docs/README.md)**. See [`CHANGELOG.md`](CHANGELOG.md) for release notes, and
[Stability and versioning](docs/reference/stability.md) for what a 1.0 release guarantees.

## License

glass is **open core**, licensed **Apache-2.0** — see [`LICENSE-APACHE`](LICENSE-APACHE).
