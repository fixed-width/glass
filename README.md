# glass

[![CI](https://github.com/fixed-width/glass/actions/workflows/ci.yml/badge.svg)](https://github.com/fixed-width/glass/actions/workflows/ci.yml)

A Rust [MCP](https://modelcontextprotocol.io) server that gives an AI coding agent a closed **build → see →
interact → debug** loop over external native GUI applications.

glass lets an agent launch a GUI app, capture what is on screen, inject mouse and
keyboard input, read the app's logs, and detect visual changes — so a coding
agent can build and debug UI applications independently instead of asking the
user "does this look right?".

glass drives apps as an external black box, so it works with any native GUI app
regardless of toolkit or language. It currently has two Linux backends — **X11** and
**Wayland** ([wlroots](https://gitlab.freedesktop.org/wlroots/wlroots)) — and a **Windows** backend ([Windows.Graphics.Capture](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture),
SendInput, UI Automation), behind a platform-agnostic core; a **macOS** backend is
planned. See [`packaging/README-windows.md`](packaging/README-windows.md)
for the Windows build and setup.

## Platform support

Where glass stands feature-by-feature across backends and OSes. **✓** supported · **–** not
supported · **🚧** planned.

<!-- KEEP IN SYNC with the code (and CLAUDE.md) whenever capabilities change. -->

### Core capabilities (per backend)

| Capability | X11 | Wayland | Windows | macOS |
|---|:--:|:--:|:--:|:--:|
| Screen capture — full + region crop | ✓ | ✓ | ✓ | 🚧 |
| Click / move | ✓ | ✓ | ✓ | 🚧 |
| Type text · key chord | ✓ | ✓ | ✓ | 🚧 |
| Scroll · drag | ✓ | ✓ | ✓ | 🚧 |
| Modifier-held click / drag / scroll | ✓ | ✓ | ✓ | 🚧 |
| Window discovery | ✓ | ✓ | ✓ | 🚧 |
| Multi-window (`glass_list_windows` / `glass_select_window`) | ✓ | ✓ | ✓ | 🚧 |
| Window move / resize / focus | ✓ | ✓ | ✓ | 🚧 |
| Log capture (stdout / stderr) | ✓ | ✓ | ✓ | 🚧 |
| Clipboard get / set | ✓ | ✓ | ✓ | 🚧 |

### Accessibility — semantic addressing (per OS)

| | Linux | Windows | macOS |
|---|:--:|:--:|:--:|
| Provider | AT-SPI | UI Automation | AX 🚧 |
| Serves backends | X11 + Wayland | Windows | — |
| Tree snapshot · click-by-element | ✓ | ✓ | 🚧 |
| Set value (`glass_set_value`) | ✓ | ✓ | 🚧 |
| Value population (text / numeric) | ✓ | ✓ | 🚧 |
| Set-of-Mark overlay (`glass_a11y_marks`) | ✓ | ✓ | 🚧 |

Accessibility is per-OS (AT-SPI serves both Linux backends). It returns an error — never a fake
tree — for apps with no accessible UI (bare canvas / game UIs), so the agent falls back to pixels.

### Containment / sandboxing (per OS)

| | Linux | Windows | macOS |
|---|:--:|:--:|:--:|
| Engine | bubblewrap | Sandboxie Classic | — 🚧 |
| `off` / `default` / `strict` | ✓ | ✓ | accepts, not enforced 🚧 |
| Fail-closed when engine absent | ✓ | ✓ | n/a |
| Build step contained | ✓ | ✓ | 🚧 |

### Isolation & runtime (per backend)

| | X11 | Wayland | Windows | macOS |
|---|:--:|:--:|:--:|:--:|
| Display isolation (app off your desktop) | ✓ private Xvfb | ✓ headless sway | – interactive desktop¹ | 🚧 |
| Clipboard isolation | ✓ | ✓ | – shared OS clipboard | 🚧 |
| Headless (no host desktop needed) | ✓ | ✓ | – needs a session² | 🚧 |

¹ A Windows VirtualDisplay / headless provider is a planned follow-on; stronger isolation today is
the VM tier (the Windows Sandbox `.wsb` template under `packaging/windows-sandbox/`, or a managed
VM running `glass-mcp serve --http`). ² Windows needs an interactive, logged-in session to render
and capture.

**Transport:** MCP over **stdio** (default, all platforms) or **network HTTP** (`glass-mcp serve
--http`, all platforms) — the network transport is behind the default-on `network` cargo feature
(a `--no-default-features` build is stdio-only).

## Install

### Prerequisites

- **Rust**, via [rustup](https://rustup.rs). glass pins a nightly toolchain in
  `rust-toolchain.toml` (needed for the portable-SIMD hot paths); rustup installs it
  automatically on the first build, so there's no toolchain to choose.
- **A display dependency**, for the backend you'll run:
  - **Linux / X11 (default):** the headless X server — `sudo apt-get install -y xvfb`
    (Debian/Ubuntu; Fedora `xorg-x11-server-Xvfb`, Arch `xorg-server-xvfb`). glass spawns
    its own private display, so this binary is the only thing to install.
  - **Linux / Wayland:** a discoverable `sway ≥ 1.12` plus [Mesa](https://www.mesa3d.org/) software GL — see
    [Running on Wayland](#running-on-wayland-sway).
  - **Windows:** nothing extra; glass uses built-in Windows APIs.
- **A containment runtime** — launched apps are **sandboxed by default**, and the `default`
  level is *fail-closed*: with no sandbox available, `glass_start` errors rather than running
  the app unconfined. So either install the runtime, or set `GLASS_SANDBOX=off` on the server
  to launch apps unconfined:
  - **Linux:** [bubblewrap](https://github.com/containers/bubblewrap) — `sudo apt-get install -y bubblewrap`
    (Fedora/Arch: `bubblewrap`) — **and** unprivileged user namespaces enabled. Ubuntu 23.10+
    restricts them via AppArmor; allow with
    `sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0` (persist via `/etc/sysctl.d/`).
  - **Windows:** [Sandboxie Classic](https://sandboxie-plus.com/downloads), installed with its service running.

  See [Containment / sandboxing](#containment--sandboxing) for the levels; `glass-mcp doctor`
  checks availability and prints the exact remedy for your system.

### Build from source

```bash
git clone https://github.com/fixed-width/glass
cd glass
cargo build --release -p glass-mcp        # → target/release/glass-mcp
```

(Tagged releases also attach prebuilt binaries to the GitHub Releases page, with
per-platform setup notes under [`packaging/`](packaging).)

### Verify

```bash
./target/release/glass-mcp doctor    # checks the environment, with a remedy for any gap
```

## Connect it to your agent (MCP)

By default `glass-mcp` speaks MCP over **stdio**, so you register the binary with
your MCP client. (To attach from another machine, see
[Over the network](#over-the-network).)

**Claude Code:**
```bash
claude mcp add glass --scope user -- /absolute/path/to/target/release/glass-mcp
```

**Claude Desktop / project `.mcp.json`:**
```json
{
  "mcpServers": {
    "glass": {
      "command": "/absolute/path/to/target/release/glass-mcp"
    }
  }
}
```

No `env` is needed: on Linux, the default X11 backend spawns its **own private headless
display** (see [Running on X11](#running-on-x11-the-default)), and the agent picks
the backend per call via `glass_start`'s `backend` argument (see
[Backends](#backends)). Add an `env` block only to change the defaults —
`"env": { "GLASS_DISPLAY": ":42" }` to attach to a display *you* manage, or
`"env": { "GLASS_BACKEND": "wayland" }` to make Wayland the default backend.

The agent then gets tools like `glass_start`, `glass_screenshot`, `glass_click`,
`glass_drag`, `glass_scroll`, `glass_type`, `glass_key`, `glass_wait_stable`,
`glass_baseline_save`, `glass_diff`, `glass_logs`, `glass_list_windows`,
`glass_select_window`, `glass_a11y_snapshot`, `glass_click_element`, `glass_set_value`,
`glass_a11y_marks`, `glass_wait_for_element`, `glass_wait_for_region`,
`glass_wait_for_log`, `glass_do`, `glass_clipboard_get`, `glass_clipboard_set`, and
`glass_doctor`.

### Over the network

stdio requires glass-mcp to run on the **same machine** as the agent. When the agent
and the target app are on different machines, run glass-mcp as a network server on the
app's machine (rmcp Streamable HTTP) and point your client at the URL:

```bash
mkdir -p ~/.glass
glass-mcp gen-token --out ~/.glass/token                  # cross-platform CSPRNG token
glass-mcp serve --http --addr 0.0.0.0:7300 --token-file ~/.glass/token
```

The client supplies the token as an `Authorization: Bearer <token>` header. Binding a
non-loopback address **without** a token is refused (fail-closed); a loopback bind needs
no token and pairs with an SSH tunnel for confidentiality
(`ssh -L 7300:127.0.0.1:7300 user@appbox`, then point the client at
`http://127.0.0.1:7300/`). The network transport is behind the default-on `network`
cargo feature (a `--no-default-features` build is stdio-only).

### Verify your setup

`glass-mcp doctor` checks that the environment glass needs is in place (Xvfb for X11,
a discoverable `sway ≥ 1.12` and Mesa software GL for Wayland) and prints how to fix
anything missing:

```bash
glass-mcp doctor          # per-check ✓/⚠/✗ with remedies; exits non-zero if the
                          # default backend can't run (CI-friendly)
glass-mcp doctor --deep   # additionally spawn + tear down the display to prove it starts
glass-mcp doctor --json   # machine-readable output
```

The agent can run the same checks itself via the `glass_doctor` tool (e.g. to
self-diagnose a failed `glass_start`).

To see how glass is **configured** (as opposed to whether it can run), use `env`:

```bash
glass-mcp env            # all GLASS_* vars: purpose, default, current value
glass-mcp env --json     # machine-readable
```

It lists every `GLASS_*` variable (see [External tool paths](#external-tool-paths) and the
backend/containment sections) with its default and current value; the network token
(`GLASS_TOKEN`) is shown only as `set`/`(unset)`, never printed.

Run `glass-mcp --help` for the full command list, `glass-mcp <command> --help` for a
command's flags, and `glass-mcp --version` for the version. (With no command, `glass-mcp`
serves MCP over stdio — the default.)

A few capabilities worth knowing:

- **Region capture.** `glass_screenshot` and `glass_wait_stable` accept an
  optional window-relative `region` so the agent can grab just the area it cares
  about. Vision-model image cost scales with pixel area, so a tight region is a
  large, recurring token saving versus the whole window.
- **Region-scoped settling.** `glass_wait_stable` also takes a
  `stability_region` — it waits for *that* sub-rectangle to stop changing,
  ignoring unrelated motion elsewhere (a clock, a spinner) that would otherwise
  keep the window from ever settling.
- **Wait-for-condition tools.** Three text-only blocking waits collapse
  screenshot poll-loops into a single call: `glass_wait_for_element` blocks
  until a UI element reaches a precise state (e.g. a button becomes enabled) and
  returns the element's `#id` for immediate use with `glass_click_element`;
  `glass_wait_for_region` blocks until a watched region changes or converges to a
  saved baseline; `glass_wait_for_log` blocks until a matching log line appears.
  All return `{matched, …}` and time out softly with `{matched:false}`.
- **Modifier-held clicks/drags/scrolls.** `glass_click`, `glass_drag`, and
  `glass_scroll` accept an optional `modifiers` array (e.g. `["ctrl"]`,
  `["ctrl","shift"]`) that holds Ctrl/Shift/Alt/Super during the action —
  enabling shift/ctrl-click multi-select, modified drags, and Ctrl+scroll.
- **Batched input (`glass_do`).** Run an ordered sequence of input actions
  (click/type/key/move/drag/scroll/settle) in one call with an optional text-first
  `then` observe (settle/diff/screenshot), collapsing per-action round-trips and
  failing fast at the offending action. Use for KNOWN sequences (login, form-fill,
  menu→item); if you need to see a result to choose the next action, don't batch
  that part.
- **Clipboard get/set.** `glass_clipboard_get` reads the clipboard as text
  (`""` when empty); `glass_clipboard_set` writes text so the app can paste it.
  Both are isolated to the app's display on the private Xvfb/sway backends —
  they never touch your real clipboard unless you set `GLASS_DISPLAY=:0` or use
  the Windows backend. `glass_clipboard_get` is also the cheap text-extraction
  path: issue `ctrl+a` then `ctrl+c` via `glass_do`, then read here — faster and
  token-free compared to OCR for any app with selectable text.
- **Real window managers.** On X11, window discovery uses `_NET_WM_PID`, a
  title/class hint, and `_NET_CLIENT_LIST`, so glass finds an app's window
  whether it runs bare on `Xvfb` or reparented under a desktop WM's decorations.
  On Wayland, glass enumerates the app's windows over the IPC of the headless
  sway compositor it spawns for the session.
- **Multiple windows.** `glass_list_windows` enumerates the app's top-level
  windows (id, title, class, geometry, which is active); `glass_select_window`
  makes one active, and subsequent capture/click/type/window ops target it with
  window-relative coordinates. All three backends enumerate every top-level the app
  owns (X11 via EWMH, Wayland via sway IPC, Windows via the launched Job's windows).
- **Accessibility tree (semantic addressing).** Where the app exposes an
  accessibility tree (most GTK/Qt/toolkit apps — not bare canvas/Unity/game UIs),
  `glass_a11y_snapshot` returns its elements as compact text — role, name, and
  window-relative bounds, each with an `#id` — and `glass_click_element` clicks one
  by `#id`. That's deterministic, low-token element addressing that complements the
  pixel loop; it errors (never a fake tree) for apps with no accessible UI, so the
  agent falls back to screenshots. Available on **Linux** (AT-SPI via [`at-spi2-core`](https://gitlab.gnome.org/GNOME/at-spi2-core),
  serving both X11 and Wayland) and **Windows** ([UI Automation](https://learn.microsoft.com/en-us/windows/win32/winauto/entry-uiauto-win32)); `./scripts/test-a11y.sh`
  exercises the Linux reader end-to-end.
  `glass_a11y_marks` returns the same elements as a numbered Set-of-Mark overlay
  drawn on the screenshot (plus a text legend) for agents that ground visually —
  click a mark with `glass_click_element` by its `#id`.

## Containment / sandboxing

On Linux, launched apps run inside a **[bubblewrap](https://github.com/containers/bubblewrap) sandbox** by default (filesystem +
process containment, network on). Three levels are available via `glass_start`'s `sandbox`
arg or the `GLASS_SANDBOX` environment variable:

- **`default`** — bubblewrap containment, network on (the default).
- **`strict`** — same as `default` plus `--unshare-net` (no outbound network from the app or build).
- **`off`** — no containment; app runs unconfined.

`default` and `strict` are fail-closed: if `bwrap` is not installed or unprivileged user namespaces
are disabled, `glass_start` returns an error rather than silently falling back to unconfined.
Install bubblewrap with `sudo apt-get install -y bubblewrap` on Debian/Ubuntu.

On **Windows**, `default`/`strict` give **real in-OS containment via
Sandboxie Classic** (filesystem/registry virtualization; the boxed app still renders, is
WGC-captured, and is SendInput-driven on the interactive desktop). `default` = contained,
network on; `strict` = contained, no network egress; `off` = launched unconfined. The engine
is Sandboxie **Classic** (cleanly GPLv3 — Plus needs a commercial "Business Certificate"); you
install it yourself ([sandboxie-plus.com/downloads](https://sandboxie-plus.com/downloads)), and
glass only *invokes* `Start.exe`/`SbieIni.exe` as subprocesses (no linking) — the same model as
Linux `bubblewrap`. It is configurable, not hardcoded: `GLASS_WIN_SANDBOX_PROVIDER=auto|sandboxie|none`
(default `auto`) and `GLASS_SANDBOXIE_DIR` (default `%ProgramFiles%\Sandboxie`, auto-detected).
Like Linux, `default`/`strict` are **fail-closed**: if no in-OS provider is available (Sandboxie
absent / its service not running, or `provider=none`), `glass_start` errors rather than running
unconfined — `off` is the explicit escape hatch. The build step also runs contained. Native
AppContainer / Low-integrity were evaluated on-box and **rejected** (the integrity-drop makes
ordinary Win32 apps fail to render; they need per-app tuning, whereas Sandboxie virtualizes
transparently). For even stronger isolation, the **VM tier** remains the stronger option: the
checked-in Windows Sandbox template under `packaging/windows-sandbox/`, or a managed VM running
`glass-mcp serve --http`. `glass_doctor` reports this posture (its Windows `sandbox` section).

```bash
glass-mcp doctor   # checks sandbox availability alongside display/compositor deps
```

## External tool paths

glass shells out to a few third-party programs. Each resolves from a `GLASS_*`
environment variable when set, otherwise a sensible default (a bare name found on
`PATH`). Point a variable at a full path to use a binary in a non-standard location.

| Tool | Env var | Default | Used by |
|---|---|---|---|
| bubblewrap | `GLASS_BWRAP` | `bwrap` (on `PATH`) | Linux app + build containment |
| Xvfb | `GLASS_XVFB` | `Xvfb` (on `PATH`) | X11 private headless display |
| sway | `GLASS_SWAY` | auto-discovered¹ | Wayland headless compositor |
| build shell | `GLASS_SH` | `sh` (on `PATH`) | running `spec.build` |
| Sandboxie dir | `GLASS_SANDBOXIE_DIR` | `%ProgramFiles%\Sandboxie` | Windows containment |

¹ Otherwise `sway` is discovered automatically: a recent-enough one on `PATH`, then
`~/.local/share/glass/sway/bin/sway`, then next to the `glass-mcp` binary. `GLASS_SWAY`
forces a specific binary and skips that search (and fails closed if the path is wrong).
`glass_doctor` reports the resolved paths.

## Backends

The backend is chosen **per `glass_start`** — the tool takes an optional
`backend` (`"x11"` or `"wayland"` on Linux, `"windows"` on a Windows host), so the
agent can pick per launch with no server restart. When omitted it falls back to the
`GLASS_BACKEND` environment variable, then to the host default (**windows** on a
Windows host, otherwise **x11**). The backend is built on `glass_start` (so the
server boots even with no display/compositor), and the MCP tools behave identically
across backends — only the setup differs:

- **X11** (Linux default) — spawns its own private headless `Xvfb` (nothing to set
  up), or attaches to a display you name with `GLASS_DISPLAY`. See
  [Running on X11](#running-on-x11-the-default).
- **Wayland (wlroots)** — spawns a private headless `sway` compositor per session,
  so there's no ambient display to set up. See [Running on Wayland](#running-on-wayland-sway).
- **Windows** (default on a Windows host) — drives the app on the interactive
  desktop (WGC capture, SendInput, UI Automation). See
  [`packaging/README-windows.md`](packaging/README-windows.md).

## Running on X11 (the default)

The X11 backend chooses its display from **`GLASS_DISPLAY`** — it never reads
ambient `$DISPLAY`, so the environment you launch from can't accidentally aim
glass at your live desktop:

- **`GLASS_DISPLAY` unset (default)** — glass spawns its **own private headless
  `Xvfb`** on a free display, logs the chosen number to stderr (`glass: spawned a
  private headless X11 display :N`), and tears it down on exit. Zero setup, fully
  isolated. Requires `Xvfb` installed (`sudo apt-get install -y xvfb`); override
  the size with `GLASS_XVFB_SCREEN` (default `1280x800x24`).
- **`GLASS_DISPLAY=:42`** (or bare `42`) — attach to a display *you* manage, e.g.
  a persistent sandbox you want to keep watching over VNC (see below).
- **`GLASS_DISPLAY=:0`** — deliberately drive your **real desktop**. The agent
  moves your actual cursor and pops real windows; useful for driving live apps,
  but it competes with you for input. This only happens when you ask for it
  explicitly.

To watch the default headless display live, point a VNC viewer at the logged
number: `x11vnc -display :N` + any VNC viewer (or `Xephyr` for a window).

### Optional: a persistent display you control

If you'd rather run your own display — to keep a VNC view pinned across server
restarts, say — start one and set `GLASS_DISPLAY` to it. A helper manages a
sandbox `Xvfb` (defaults to `:42`; override the number with `GLASS_DISPLAY`, the
size with `GLASS_XVFB_SCREEN`):

```bash
./scripts/sandbox-xvfb.sh start      # also: status | stop | restart
```

Then register glass with `"env": { "GLASS_DISPLAY": ":42" }`. Watch it with
`x11vnc -display :42` + any VNC viewer, or run a windowed `Xephyr :42`.

#### Make that display persistent (survive logout)

Run the `Xvfb` at login via a **systemd user service**:

```ini
# ~/.config/systemd/user/glass-xvfb.service
[Unit]
Description=glass sandbox Xvfb display :42

[Service]
ExecStart=/usr/bin/Xvfb :42 -screen 0 1280x800x24
Restart=on-failure

[Install]
WantedBy=default.target
```
```bash
systemctl --user daemon-reload
systemctl --user enable --now glass-xvfb.service
loginctl enable-linger "$USER"   # optional: keep it up without an active login
```
(Adjust the `Xvfb` path to `command -v Xvfb`.) Or, for desktop-only autostart,
drop an equivalent `Exec=Xvfb :42 -screen 0 1280x800x24` into a
`~/.config/autostart/glass-xvfb.desktop` entry.

Requires `Xvfb` installed (`sudo apt-get install -y xvfb` on Debian/Ubuntu).

## Running on Wayland (sway)

Select it **per launch** with `glass_start`'s `backend: "wayland"`, or make it the
default for every launch with `GLASS_BACKEND=wayland` (e.g.
`"env": { "GLASS_BACKEND": "wayland" }` in the MCP config). Unlike X11, this
backend doesn't attach to an ambient display — for each session it spawns a
**private headless [`sway`](https://swaywm.org) instance** (sway is the
third-party wlroots-based Wayland compositor) and runs the target app inside it. The app's windows float at their natural size;
`glass_list_windows`/`glass_select_window` enumerate and switch between them over
sway IPC. Capture goes through `wlr-screencopy` of the active window's output
region, and input through the `wlr-virtual-pointer` and `zwp_virtual_keyboard`
protocols.

glass needs a **sway ≥ 1.12 / wlroots ≥ 0.20** it can discover (no env var): on
`PATH` (once your distro ships one that new), or installed to
`~/.local/share/glass/sway/` by the [sway-build](https://github.com/fixed-width/sway-build) tool, or in a
`sway/` dir beside the `glass-mcp` binary. It also needs the host's Mesa software GL so GPU-less hosts can
render:

```bash
sudo apt-get install -y libegl1 libgl1-mesa-dri   # Debian/Ubuntu
```

Because sway is headless and per-session, there's **nothing to set up or keep
running** — no persistent display, no `$DISPLAY`/`$WAYLAND_DISPLAY`. sway also
launches an Xwayland server, so X11-only apps run under this backend too.

Because the target app runs inside the headless sway that glass spawns (not the
host's compositor), this backend works on **any** Linux host — **including GNOME and
KDE** desktops, where the host desktop is simply irrelevant. Driving the user's
**existing live desktop** session — the Wayland analog of X11 `GLASS_DISPLAY=:0`
— is a separate, deliberate **non-goal**: it requires the XDG-portal path with an
interactive consent dialog, unsuited to unattended use.

## Benchmarking

Per-frame hot-path micro-benchmarks ([criterion](https://github.com/bheisler/criterion.rs)) live in `crates/*/benches/`:

```bash
cargo bench -p glass-core -p glass-x11                            # run all (diff, webp encode/decode, xdata_to_rgba)
cargo bench -p glass-core -p glass-x11 -- --save-baseline main    # save a baseline, then compare after a change:
cargo bench -p glass-core -p glass-x11 -- --baseline main
```

(Only `glass-core` and `glass-x11` carry benchmarks; their libs set `bench = false`
so `cargo bench` runs the criterion targets rather than the unit-test harness,
which would reject criterion's `--save-baseline`/`--baseline` flags.)

Profile a hot path as a flamegraph (needs [`cargo install flamegraph`](https://github.com/flamegraph-rs/flamegraph) and
`kernel.perf_event_paranoid <= 1`):

```bash
./scripts/bench.sh diff "identical/1920x1080"   # writes flamegraph.svg
```

## Status

The Linux feature set is implemented and tested across **both** Linux backends
(X11 and Wayland/wlroots), and the **Windows** backend (WGC capture, SendInput, UI
Automation) is built and CI-tested; **macOS is the one OS backend not yet built**.

## License

glass is **open core**, licensed **Apache-2.0** — see [`LICENSE-APACHE`](LICENSE-APACHE).
