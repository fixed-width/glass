# Changelog

All notable changes to glass are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and glass adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!--
Maintenance: add entries under [Unreleased] as user-facing changes merge to
master. At release time, rename [Unreleased] to the new version with its UTC
release date (the GitHub release's `published_at` date, so the changelog matches
the site's release list), add a fresh empty [Unreleased] above it, and update the compare links at the
bottom. Keep entries user-facing — what changed for someone using glass — not
internal refactors, CI, or test-only changes.
-->

## [Unreleased]

### Fixed
- iOS: a log line an app emits at launch — before its first frame (e.g. an `applicationDidFinishLaunching`
  / `App.init` `os_log`) — is now captured, so you can gate readiness on it with `glass_wait_for_log`.
  Previously the unified-log stream attached only after the app had already launched, so launch-time lines
  were lost to the live tail; the stream now starts before launch and the launch waits until it is
  delivering.
- iOS: a Homebrew-installed `idb_companion` is now found automatically even when glass is launched by
  launchd (the `.app` / LaunchAgent), whose minimal `PATH` omits Homebrew's bindir — so input and the
  accessibility tree work without setting `GLASS_IDB_COMPANION` by hand.
- Visual baselines (`glass_baseline_save` / `glass_diff`) are written to an absolute, always-writable
  location instead of a working-directory-relative one that failed under launchd's read-only `/` cwd.
- iOS: `glass doctor` now always shows the `idb_companion` status in the `[ios]` section, even when
  iOS isn't the selected backend (e.g. a `.app` / LaunchAgent server defaulting to `GLASS_BACKEND=macos`
  while iOS is driven per-call). Previously the line was omitted unless `GLASS_BACKEND=ios`, so its
  absence read like "not found" for the input/accessibility precondition. When iOS isn't the active
  backend an absent companion is reported as an advisory warning rather than a hard failure.

## [0.4.0] - 2026-07-11

### Added
- An [iOS Simulator backend](docs/how-to/setup-ios.md) (`GLASS_BACKEND=ios`, macOS only): launch, capture,
  log streaming, and clipboard for native iOS apps in the Simulator, driven through `xcrun simctl`, plus
  input (tap/click, type, swipe, scroll) and the accessibility tree (snapshot, click-element, set-value)
  over [`idb_companion`](docs/how-to/setup-ios.md#input--accessibility) when it is installed. Includes a
  `glass doctor` preflight for Xcode, an installed iOS runtime, an available simulator, and `idb_companion`;
  with `--deep`, the preflight spawns `idb_companion` for real against an already-booted simulator (or runs a
  bounded `idb_companion --version` self-test when none is booted) and fails if the companion is broken or
  missing, rather than trusting that it is merely resolvable on `PATH`.
  Multi-touch gestures (`glass_gesture`) are not supported on the Simulator yet.
- A [Windows access model](docs/explanation/windows-permissions.md) explanation: Windows needs no
  per-app permission grants (unlike macOS), what actually gates access (interactive session, UAC/UIPI
  integrity levels, SmartScreen on unsigned downloads), and how to get past the first-run SmartScreen
  prompt.

### Changed
- Installing the optional Android companions is simpler and better documented: the setup guide,
  `glass doctor`, and `glass-mcp env` now lead with the easiest path — download `glass-agent.jar`
  and `glass-a11y.apk` from the [glass-android-agent](https://github.com/fixed-width/glass-android-agent)
  releases and drop them next to the `glass-mcp` binary, where glass discovers them automatically
  (no environment variables, no build step). `GLASS_ANDROID_AGENT_JAR` / `GLASS_ANDROID_A11Y_APK`
  are documented as overrides of that auto-discovery.
- Installing glass now starts from the Releases page rather than a source build: `README.md`,
  `docs/how-to/setup-linux.md`, and `docs/how-to/setup-windows.md` lead with the prebuilt binary.
- `docs/reference/platforms.md` documents the assets each release attaches.

### Fixed
- On the iOS Simulator backend, a `glass_drag` (or any `idb` HID gesture) longer than 30s no
  longer aborts mid-swipe with a timeout error. The per-gesture RPC deadline now scales with the
  gesture's own duration plus a margin, instead of a flat 30s, so a long drag runs to completion
  while a wedged companion is still bounded.
- `doctor --deep` no longer tells you to "run with --deep" for the Android `screencap` and
  `uiautomator` probes when you already passed `--deep`. Those deep probes only run when
  Android is the selected backend, so on another host backend the skip reason now points at
  the real gate: "set `GLASS_BACKEND=android`".
- The `glass_diff` tool reference now documents its `region` parameter — a window-relative scoped
  diff that also makes the reported `bbox` region-relative — which had been usable but undocumented.

## [0.3.1] - 2026-07-08

### Changed
- Documentation reorganized into a [Diátaxis](https://diataxis.fr) structure under
  [`docs/`](docs/README.md): a getting-started [tutorial](docs/tutorial/first-drive.md)
  that has an agent build and drive the interactive egui fixture end to end, task-focused
  how-to guides, complete reference (every tool, environment variable, and CLI command),
  and explanations of how glass works. The `README` is now a concise landing page. The old
  `docs/running-on-{linux,macos,windows}.md` guides moved to `docs/how-to/setup-*.md`
  (redirects left at the old paths).

### Fixed
- a11y: `a11y: true` now exposes the accessibility tree for **accesskit-based apps**
  (egui/winit/Slint/Iced) on Linux — glass advertises a screen reader on its private
  AT-SPI bus, which accesskit's adapter requires to activate. GTK/Qt were unaffected.
- The default backend on a macOS host is documented correctly as `macos` — the `glass_start`
  tool description, the `backend` parameter docs, and `glass-mcp env` previously named only
  "windows on Windows, else x11".

## [0.3.0] - 2026-07-07

### Added
- `glass_scroll_to_element`: blind-scroll an accessibility element into view.
- `window_id` parameter on `glass_screenshot`, `glass_wait_stable`, and
  `glass_wait_for_region` to target a specific window.
- `glass_diff` can be scoped to a window-relative sub-region.
- `glass_set_value` support for switches and dropdowns.
- macOS: `glass_start` launches `.app` bundles directly (LaunchServices /
  NSWorkspace), adopting or terminating the running app.
- macOS: `cmd`/`command` is accepted as an alias for the Super modifier.
- **macOS drag-install + double-click setup.** Tagged releases attach a notarized,
  Gatekeeper-clean universal `.dmg`; drag `GlassMcp.app` to `/Applications` and
  double-click it. A permission checklist guides granting Accessibility and Screen
  Recording (one at a time; granting Screen Recording relaunches glass so it takes
  effect), then glass installs a login item and runs as a visible **`glass ●`
  menu-bar app** showing the MCP endpoint, with Copy endpoint, Restart, Quit, and
  Uninstall.
- macOS: `glass-mcp uninstall` (and the menu-bar "Uninstall glass…") stop glass from
  starting at login; `glass-mcp status` reports whether glass is running and its endpoint.
- macOS: an app icon, so `GlassMcp.app` is no longer a blank bundle in Finder and the Dock.

### Fixed
- x11: off-screen captures are clipped to the display instead of failing with
  `BadMatch`.
- x11: window captures include popovers and menus drawn outside the window.
- `glass_click_element` auto-routes into an owning popover window.
- wayland: capture works on software renderers that advertise only 24-bpp shm.
- a11y: `glass_set_value` on a spin button writes through the Value interface;
  a role-only query no longer matches a bare focusable container.
- a11y: click the visible part of a clipped element, not the window edge.
- macOS: don't orphan a bundle launch whose window never appears; absorb the
  accessibility-snapshot startup race.

## [0.2.0] - 2026-07-04

### Added
- **macOS backend.** Drive native macOS apps: screen capture (ScreenCaptureKit),
  mouse/keyboard input (CGEvent), window management, accessibility reading
  (AXUIElement), and clipboard access — behind the same platform-agnostic core
  as the Linux and Windows backends.
- macOS containment: a Seatbelt sandbox for the launched app and a clipboard
  shim that isolates the app's pasteboard from the host.
- An immutable, provenance-attested release pipeline and macOS packaging.

### Changed
- Adopted default `rustfmt`, enforced in CI.

### Fixed
- x11: oversized capture requests report a clear error instead of failing
  opaquely.
- `glass_set_value` reports honestly when the written value cannot be read back.
- Windows: HGLOBAL handles are released via RAII guards.

## [0.1.2] - 2026-06-18

### Changed
- Share one SIMD pixel-swizzle kernel across the X11, Windows, and Wayland
  capture paths for faster frame conversion.

## [0.1.1] - 2026-06-18

### Added
- **Linux accessibility (opt-in).** An `a11y` flag starts a private AT-SPI
  session bus for the launched app and reads its accessibility tree, so an agent
  can address elements semantically instead of by pixel.
- **Audit log.** `--audit-log` (and `GLASS_AUDIT_*`) records every actuation to
  JSONL with content redaction; `glass_doctor` reports audit posture.
- `glass_drag` gains a `duration_ms` and paces synthetic drags across frames on
  X11 and Wayland.

### Fixed
- Input fidelity: hold the modifier across the whole frame for synthetic chords
  and scroll wheels; self-commit each keystroke when typing on X11 and Wayland;
  pace synthetic typing on Windows to avoid an OS injection race.
- Windows: adopt the boxed app window (not glass's launcher console) under
  Sandboxie; honest `set_value` and more robust window-finding.
- x11: focus the launched and selected window so synthetic keys land; translate
  stale-window X errors to `WindowNotFound`.
- Launched apps run in their own process group with graceful teardown, so
  helper processes don't orphan.

## [0.1.0] - 2026-06-08

First public release — open core, Apache-2.0.

### Added
- An MCP server giving an agent a **build → see → interact → debug** loop over
  external native GUI apps, driven as a black box regardless of toolkit or
  language.
- Linux **X11** and **Wayland** (wlroots) backends and a **Windows** backend
  (Windows.Graphics.Capture / SendInput / UI Automation) behind a
  platform-agnostic core.
- Core tools: `glass_start`, `glass_stop`, `glass_screenshot`, `glass_click`,
  `glass_list_windows`, `glass_select_window`, and `glass_doctor`.

[Unreleased]: https://github.com/fixed-width/glass/compare/v0.4.0...HEAD
[0.4.0]: https://github.com/fixed-width/glass/compare/v0.3.1...v0.4.0
[0.3.1]: https://github.com/fixed-width/glass/compare/v0.3.0...v0.3.1
[0.3.0]: https://github.com/fixed-width/glass/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/fixed-width/glass/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/fixed-width/glass/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/fixed-width/glass/compare/c1d0d5f...v0.1.1
[0.1.0]: https://github.com/fixed-width/glass/commit/c1d0d5f
