# Changelog

All notable changes to glass are documented in this file.

The format is based on [Keep a Changelog](https://keepachangelog.com/en/1.1.0/),
and glass adheres to [Semantic Versioning](https://semver.org/spec/v2.0.0.html).

<!--
Maintenance: add entries under [Unreleased] as user-facing changes merge to
master. At release time, rename [Unreleased] to the new version with its date,
add a fresh empty [Unreleased] above it, and update the compare links at the
bottom. Keep entries user-facing — what changed for someone using glass — not
internal refactors, CI, or test-only changes.
-->

## [Unreleased]

### Changed
- Documentation reorganized into a [Diátaxis](https://diataxis.fr) structure under
  [`docs/`](docs/README.md): a getting-started [tutorial](docs/tutorial/first-drive.md),
  task-focused how-to guides, complete reference (every tool, environment variable, and
  CLI command), and explanations of how glass works. The `README` is now a concise landing
  page. The old `docs/running-on-{linux,macos,windows}.md` guides moved to
  `docs/how-to/setup-*.md` (redirects left at the old paths).

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

## [0.1.1] - 2026-06-17

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

[Unreleased]: https://github.com/fixed-width/glass/compare/v0.3.0...HEAD
[0.3.0]: https://github.com/fixed-width/glass/compare/v0.2.0...v0.3.0
[0.2.0]: https://github.com/fixed-width/glass/compare/v0.1.2...v0.2.0
[0.1.2]: https://github.com/fixed-width/glass/compare/v0.1.1...v0.1.2
[0.1.1]: https://github.com/fixed-width/glass/compare/c1d0d5f...v0.1.1
[0.1.0]: https://github.com/fixed-width/glass/commit/c1d0d5f
