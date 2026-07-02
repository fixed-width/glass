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
**Wayland** ([wlroots](https://gitlab.freedesktop.org/wlroots/wlroots)) — a **Windows** backend ([Windows.Graphics.Capture](https://learn.microsoft.com/en-us/uwp/api/windows.graphics.capture),
SendInput, UI Automation) — an **Android** backend (drives native apps in an AVD emulator over `adb`) — and a
**macOS** backend (ScreenCaptureKit capture, CGEvent input, AXUIElement windows and accessibility tree), behind
a platform-agnostic core; on macOS, sandboxing is still planned. See the per-host setup guides:
[Linux](docs/running-on-linux.md) · [Windows](docs/running-on-windows.md) · [macOS](docs/running-on-macos.md).

## The loop in practice

Point an AI coding agent at a GUI app and it runs the whole **build → see → interact →
debug** cycle itself:

```jsonc
glass_start   { "build": "cargo build --release", "run": ["target/release/my-app"] }  // builds, then launches the app (sandboxed)
glass_screenshot                       // see the window
glass_click   { "x": 240, "y": 160 }   // interact
glass_wait_stable                      // let the render settle
glass_diff                             // what changed? changed_pct + bbox, as text — no image
glass_logs                             // read the app's stderr
```

`glass_diff` and the `glass_wait_for_*` tools return text only, so the routine checks
between screenshots cost no vision tokens.

## Install

### Prerequisites

- **Rust**, via [rustup](https://rustup.rs). glass pins a nightly toolchain in
  `rust-toolchain.toml` (needed for the portable-SIMD hot paths); rustup installs it
  automatically on the first build, so there's no toolchain to choose.
- **Display/compositor and containment runtime** — setup depends on your host OS; see
  the guide for **[Linux](docs/running-on-linux.md)** · **[Windows](docs/running-on-windows.md)** · **[macOS](docs/running-on-macos.md)**.
  Apps are **sandboxed by default**; set `GLASS_SANDBOX=off` to run unconfined.
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

No `env` is needed: glass uses your host's default backend (see [Backends](#backends)) and,
where the host supports it, gives each session its **own isolated display** with nothing to
set up — so the app never lands on your desktop. The agent can also choose a backend per call
via `glass_start`'s `backend` argument. Add an `env` block only to override a default; the
specific knobs are host-specific — see your host guide:
**[Linux](docs/running-on-linux.md)** · **[Windows](docs/running-on-windows.md)** · **[macOS](docs/running-on-macos.md)**.

The agent then gets tools like `glass_start`, `glass_screenshot`, `glass_click`,
`glass_drag`, `glass_scroll`, `glass_gesture`, `glass_type`, `glass_key`, `glass_wait_stable`,
`glass_baseline_save`, `glass_diff`, `glass_logs`, `glass_list_windows`,
`glass_select_window`, `glass_a11y_snapshot`, `glass_click_element`, `glass_set_value`,
`glass_a11y_marks`, `glass_wait_for_element`, `glass_wait_for_region`,
`glass_wait_for_log`, `glass_do`, `glass_clipboard_get`, `glass_clipboard_set`, and
`glass_doctor`.

### Drive it well — the `glass-drive` skill

glass works with any MCP agent as-is, but an agent drives it more reliably with a little
guidance: verify with cheap text before spending a screenshot, fall back from the a11y tree to
pixels on a canvas, pace drags, reach for multi-touch. That's packaged as
**[`glass-drive`](https://github.com/fixed-width/skills)** — an open
[Agent Skill](https://agentskills.io) that works across agents (Claude Code, Codex, Cursor,
OpenCode, …):

```bash
npx skills add fixed-width/skills -s glass-drive
```

It's optional — glass needs no app integration and no skill to run — but it saves the agent
rediscovering the driving loop from scratch.

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

`glass-mcp doctor` checks that the environment glass needs is in place — your backend's
display dependencies, the containment runtime, and the external tool paths — and prints how
to fix anything missing:

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
- **Multi-touch gestures (`glass_gesture`, Android only).** Drive 2–10 simultaneous
  pointers — each a straight `from→to` segment over a shared duration — for pinch-zoom,
  two-finger rotate, and two-finger swipes. Android-only and requires the on-device
  agent (`adb`'s `input` has no multi-touch command); the `adb` fallback and the
  desktop backends refuse with a clear error rather than degrade to a single pointer.
- **Batched input (`glass_do`).** Run an ordered sequence of input actions
  (click/type/key/move/drag/scroll/settle) in one call with an optional text-first
  `then` observe (settle/diff/screenshot), collapsing per-action round-trips and
  failing fast at the offending action. Use for KNOWN sequences (login, form-fill,
  menu→item); if you need to see a result to choose the next action, don't batch
  that part.
- **Clipboard get/set.** `glass_clipboard_get` reads the clipboard as text
  (`""` when empty); `glass_clipboard_set` writes text so the app can paste it.
  Both are isolated to the app's display on the private Xvfb/sway backends, and
  on Windows a sandboxed app gets a **private clipboard** too — an injected hook
  backs the boxed app's clipboard with glass's own store, carrying text, HTML, RTF,
  and images over both the Win32 and OLE clipboards (so rich apps like Word, Excel,
  and Chrome work too; x64) and real-file copy via `CF_HDROP` (virtual-file drag-out
  — shell extensions, zip attachments — is deferred). So they never touch
  your real clipboard unless you set `GLASS_DISPLAY=:0` or run the Windows
  backend with `sandbox=off`. On **Android**, clipboard get/set works through the
  optional on-device agent (set `GLASS_ANDROID_AGENT_JAR`) —
  the system clipboard isn't reachable over plain `adb`, so without the agent these
  tools report unsupported. `glass_clipboard_get` is also the cheap text-extraction
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
  window-relative coordinates. The desktop backends enumerate every top-level the app
  owns (X11 via EWMH, Wayland via sway IPC, Windows via the launched Job's windows); the
  Android backend enumerates the app's on-screen windows — its activity plus any
  dialogs/popups — from `dumpsys window`, and `glass_select_window` retargets capture and
  input (Android composites, so there's no z-order raise).
- **Accessibility tree (semantic addressing).** Where the app exposes an
  accessibility tree (most GTK/Qt/toolkit apps — not bare canvas/Unity/game UIs),
  `glass_a11y_snapshot` returns its elements as compact text — role, name, and
  window-relative bounds, each with an `#id` — and `glass_click_element` clicks one
  by `#id`. That's deterministic, low-token element addressing that complements the
  pixel loop; it errors (never a fake tree) for apps with no accessible UI, so the
  agent falls back to screenshots. Available on **Linux** (AT-SPI via [`at-spi2-core`](https://gitlab.gnome.org/GNOME/at-spi2-core),
  serving both X11 and Wayland), **Windows** ([UI Automation](https://learn.microsoft.com/en-us/windows/win32/winauto/entry-uiauto-win32)), and **Android** (via `uiautomator`); `./scripts/test-a11y.sh`
  exercises the Linux reader end-to-end.
  `glass_a11y_marks` returns the same elements as a numbered Set-of-Mark overlay
  drawn on the screenshot (plus a text legend) for agents that ground visually —
  click a mark with `glass_click_element` by its `#id`.

## Containment / sandboxing

Launched apps run inside a sandbox by default. Three levels are available via `glass_start`'s
`sandbox` arg or the `GLASS_SANDBOX` environment variable:

- **`default`** — containment on, network on (the default).
- **`strict`** — containment on, no outbound network from the app.
- **`off`** — no containment; app runs unconfined.

`default` and `strict` are **fail-closed**: if no containment runtime is available,
`glass_start` errors rather than silently running the app unconfined. `off` is the explicit
escape hatch. The `sandbox` level governs the **launched app only** — the optional `build`
step always runs unsandboxed, with your full developer environment.

Install the containment runtime per your host guide:
[Linux](docs/running-on-linux.md) (bubblewrap) · [Windows](docs/running-on-windows.md) (Sandboxie).

```bash
glass-mcp doctor   # checks sandbox availability alongside your backend's display deps
```

## Audit log (opt-in)

Pass `--audit-log <path>` (or set `GLASS_AUDIT_LOG=<path>`) to append a JSONL record of
every actuation glass performs — launch/stop, type, key, click, drag, scroll, set_value,
clipboard writes, element clicks, window focus/resize/move, and each `glass_do`
sub-action. Reads (screenshots, diffs, accessibility snapshots, log/clipboard reads) are
not logged. The hook lives in the core actuation path, so no actuation can bypass it. One
JSON object per line: `seq`, `ts`, `action`, `target`, `args`, `result`, and for
content-bearing actions a `content` descriptor.

Typed/clipboard/launch content is **redacted by default** to a length + SHA-256 + short
prefix, so the log is not a secret sink. `GLASS_AUDIT_CONTENT=full` stores verbatim text,
`none` stores no content, and `GLASS_AUDIT_PREFIX_LEN=<n>` sizes the prefix (`0` disables
it). `glass-mcp doctor` reports whether auditing is on, the path, and the content mode.

Two things are recorded in plaintext regardless of `GLASS_AUDIT_CONTENT`: the short
content **prefix** (default 8 chars — set `GLASS_AUDIT_PREFIX_LEN=0` to drop it), and
**target metadata** (the active window's title and an element's role/name) which is
attribution, not actuation content. A window title or field label can itself be sensitive,
so treat the log as confidential. Launch records intentionally omit `env` and `cwd`.

## External tool paths

glass shells out to a few third-party programs. Each resolves from a `GLASS_*`
environment variable when set, otherwise a sensible default (a bare name found on
`PATH`). Point a variable at a full path to use a binary in a non-standard location.

| Tool | Env var | Default | Used by |
|---|---|---|---|
| bubblewrap | `GLASS_BWRAP` | `bwrap` (on `PATH`) | Linux app containment |
| Xvfb | `GLASS_XVFB` | `Xvfb` (on `PATH`) | X11 private headless display |
| sway | `GLASS_SWAY` | auto-discovered¹ | Wayland headless compositor |
| adb | `GLASS_ADB` | `adb` (on `PATH`) | Android device/emulator control |
| build shell | `GLASS_SH` | `sh` (on `PATH`) | running `spec.build` |
| Sandboxie dir | `GLASS_SANDBOXIE_DIR` | `%ProgramFiles%\Sandboxie` | Windows containment |

¹ Otherwise `sway` is discovered automatically: a recent-enough one on `PATH`, then
`~/.local/share/glass/sway/bin/sway`, then next to the `glass-mcp` binary. `GLASS_SWAY`
forces a specific binary and skips that search (and fails closed if the path is wrong).
`glass_doctor` reports the resolved paths.

## Backends

The backend is chosen **per `glass_start`** — the tool takes an optional
`backend` (`"x11"` or `"wayland"` on Linux, `"windows"` on a Windows host, or `"android"` for an emulator on any host), so the
agent can pick per launch with no server restart. When omitted it falls back to the
`GLASS_BACKEND` environment variable, then to the host default (**windows** on a
Windows host, otherwise **x11**). The backend is built on `glass_start` (so the
server boots even with no display/compositor), and the MCP tools behave identically
across backends — only the setup differs:

- **X11** (Linux) — spawns its own private headless `Xvfb` (nothing to set
  up), or attaches to a display you name with `GLASS_DISPLAY`. See
  [docs/running-on-linux.md](docs/running-on-linux.md).
- **Wayland (wlroots)** — spawns a private headless `sway` compositor per session,
  so there's no ambient display to set up. See
  [docs/running-on-linux.md](docs/running-on-linux.md).
- **Windows** — drives the app on the interactive
  desktop (WGC capture, SendInput, UI Automation), so it needs an interactive,
  logged-in session to render and capture. Synthetic typing is paced by
  **`GLASS_TYPE_DWELL_MS`** (default `60`) to stay ahead of a fast-injection race in
  the OS input pipeline — raise it on a slow/loaded host, lower it for speed. See
  [docs/running-on-windows.md](docs/running-on-windows.md).
- **Android (AVD)** — drives a native Android app in an emulator over `adb`; **host-OS-agnostic**
  (it shells out to `adb`, so it runs from a Linux, Windows, or macOS host). glass manages the
  AVD — attaching to a running emulator or booting a headless one itself — and the VM *is* the
  sandbox, so there's no separate containment step. The app is built (`spec.build`, e.g.
  `./gradlew assembleDebug`) on the host, installed, and launched; `glass_start`'s `run` is the
  launch component `package/.Activity` (plus an optional `.apk`). Capture, input, logs,
  multi-window, and a `uiautomator` accessibility tree work over `adb`; two optional on-device
  companions add more — an agent for clipboard + high-fidelity input, and an AccessibilityService
  for a Compose-rich a11y tree + high-fidelity `set_value`. Window
  resize/move (apps are full-screen) and physical devices are non-goals. See the Android section
  of your host guide: [Linux](docs/running-on-linux.md) · [Windows](docs/running-on-windows.md) · [macOS](docs/running-on-macos.md).

## Benchmarking

Per-frame hot-path micro-benchmarks ([criterion](https://github.com/bheisler/criterion.rs)) live in `crates/*/benches/`:

```bash
# core (diff, webp encode/decode) plus the per-backend pixel conversions
PKGS="-p glass-core -p glass-x11 -p glass-windows -p glass-wayland"
cargo bench $PKGS                          # run all
cargo bench $PKGS -- --save-baseline main  # save a baseline, then compare after a change:
cargo bench $PKGS -- --baseline main
```

(`glass-core`, `glass-x11`, `glass-windows`, and `glass-wayland` carry benchmarks; their
libs set `bench = false` so `cargo bench` runs the criterion targets rather than the
unit-test harness, which would reject criterion's `--save-baseline`/`--baseline` flags. The
`pixels` bench exists in all three backends, so name the crate with `-p` to flamegraph one.)

Profile a hot path as a flamegraph (needs [`cargo install flamegraph`](https://github.com/flamegraph-rs/flamegraph) and
`kernel.perf_event_paranoid <= 1`):

```bash
./scripts/bench.sh diff "identical/1920x1080"   # writes flamegraph.svg
```

## Platform support

Where glass stands by OS. **✓** supported · **◑** partial · **–** not supported · **🚧** planned.

<!-- KEEP IN SYNC with the code (and CLAUDE.md) whenever capabilities change. -->

| Capability | Linux (X11 + Wayland) | Windows | Android (AVD) | macOS |
|---|:--:|:--:|:--:|:--:|
| Capture · input · windows · clipboard · logs | ✓ | ✓ | ✓ † | ✓ ‡ |
| Accessibility (semantic addressing) | ✓ AT-SPI | ✓ UI Automation | ✓ UIAutomator | ✓ AX |
| Containment / sandboxing | ✓ bubblewrap | ✓ Sandboxie Classic | ✓ the emulator VM | 🚧 |
| Display isolation (app off your desktop) | ✓ headless Xvfb / sway | ◑ virtual display · VM tier | ✓ headless emulator | 🚧 |

† **Android** is emulator-only. Capture, multi-window, input, and logs work over `adb`, and glass manages the AVD (attach a running one, or boot a headless one). **Clipboard, high-fidelity input, and multi-touch gestures (`glass_gesture`)** use the optional on-device agent, and an optional on-device **AccessibilityService** sharpens the a11y tree (Compose) + `set_value` (both in the Android section of your host guide: [Linux](docs/running-on-linux.md) · [Windows](docs/running-on-windows.md) · [macOS](docs/running-on-macos.md)) — without the agent, input falls back to adb's `input` (single-pointer only — no multi-touch) and clipboard is unavailable; without the service, a11y falls back to `uiautomator`. glass is developed and tested against **Android 14 (API 34)**; the `adb` backend assumes no particular version and the optional companions declare an Android 7.0 (API 24) floor (details in your host guide). Window resize/move (apps are full-screen) and physical devices are non-goals.

‡ **macOS** capture, input, windows, clipboard, and logs are built and CI-tested (ScreenCaptureKit capture, CGEvent input, AXUIElement windows). Clipboard acts on the real system pasteboard (no containment yet).

The per-platform detail — sandboxing levels, display isolation, the accessibility tree —
lives in the [Containment](#containment--sandboxing), [Backends](#backends), and
per-host guides ([Linux](docs/running-on-linux.md) · [Windows](docs/running-on-windows.md) · [macOS](docs/running-on-macos.md)).

**Transport:** MCP over **stdio** (default, all platforms) or **network HTTP** (`glass-mcp serve
--http`, all platforms) — the network transport is behind the default-on `network` cargo feature
(a `--no-default-features` build is stdio-only).

## Status

The Linux feature set is implemented and tested across **both** Linux backends
(X11 and Wayland/wlroots), and the **Windows** backend (WGC capture, SendInput, UI
Automation) is built and CI-tested. An **Android** backend drives native apps in an AVD
emulator over `adb` — capture, input, logcat, multi-window, a `uiautomator` accessibility
tree, a managed AVD (attach-or-boot), and two optional on-device companions — an agent
(clipboard + high-fidelity input) and an AccessibilityService (Compose-rich a11y tree +
high-fidelity `set_value`), both set up in the [Linux](docs/running-on-linux.md) /
[Windows](docs/running-on-windows.md) Android guides; it's built and unit-tested in CI and
validated on-device. The **macOS** backend (ScreenCaptureKit capture, CGEvent input,
AXUIElement windows/logs, and an AXUIElement accessibility tree) is built and CI-tested;
sandboxing is not yet implemented — see [docs/running-on-macos.md](docs/running-on-macos.md).

## License

glass is **open core**, licensed **Apache-2.0** — see [`LICENSE-APACHE`](LICENSE-APACHE).
