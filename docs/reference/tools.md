<!-- KEEP IN SYNC with the MCP tool definitions in `crates/glass-mcp` (and the platform matrix in
     reference/platforms.md) whenever a tool or parameter changes. -->

# Tool reference

Every tool glass exposes to an agent over MCP. Tools are grouped by purpose; within a group each
entry gives the tool's parameters, what it returns, and any platform limits.

For the concepts behind these tools — the build→see→interact→debug loop and why the observe tools
return text — see [explanation/the-loop.md](../explanation/the-loop.md). For which tools each OS
supports, see [reference/platforms.md](platforms.md).

## Conventions

- **Coordinates are window-relative.** `(0,0)` is the active window's top-left; glass maps to global
  coordinates internally.
- **Text vs image results.** Capture tools return a lossless WebP image. The observe tools
  (`glass_diff`, the `glass_wait_for_*` family, `glass_wait_stable` with `include_image:false`)
  return **text only**, so routine checks between screenshots cost no vision tokens.
- **Element ids** from `glass_a11y_snapshot` / `glass_a11y_marks`, and **window ids** from
  `glass_list_windows`, are valid only within the latest snapshot/listing — re-read rather than
  caching them.
- **No silent fallbacks.** A failed capture or input returns a structured error, never a blank or
  stale frame.

## Session lifecycle

### `glass_start`

Build, launch, and locate a native GUI app; returns its window geometry.

- `run` (array of string, **required**) — program and arguments; `run[0]` is the executable.
- `build` (string) — shell command run in `cwd` before launching.
- `cwd` (string) — working directory for `build` and `run`.
- `env` (array of `[name, value]` pairs) — extra environment for the launched app.
- `backend` (string) — `"x11"` or `"wayland"` (Linux), `"windows"` (Windows host), or `"android"`
  (an AVD emulator, any host). Omit for the server default (`GLASS_BACKEND`, else `windows` on
  Windows, else `x11`).
- `sandbox` (string) — `"default"`, `"strict"`, or `"off"`. Omit for the server default
  (`GLASS_SANDBOX`, else `default`). See [explanation/containment.md](../explanation/containment.md).
- `window_hint` (`{ title?, class? }`) — disambiguate which window is the app's when several appear,
  or find a window the launched process hands off to an unrelated process (some packaged Windows
  apps). `title` is a case-insensitive substring; `class` is an exact match.
- `a11y` (boolean, default false) — **Linux only.** Spawn a private AT-SPI bus so the accessibility
  tools work against this app. Opt-in, since it spawns extra processes.
- `timeout_ms` (integer) — launch timeout.

Returns the located window's geometry.

### `glass_stop`

Stop the running app and end the session. No parameters.

## Capture & visual comparison

### `glass_screenshot`

Capture the app window, or an optional sub-rectangle, as a lossless WebP image.

- `region` (`{ x, y, width, height }`, window-relative) — capture just this rectangle; omit for the
  whole window. Vision cost scales with pixel area, so a tight region is a recurring token saving.
- `window_id` (integer) — capture this window (id from `glass_list_windows`) instead of the active
  one, without changing which window subsequent ops target. Omit for the active window.

### `glass_baseline_save`

Save the current frame as a named visual baseline for later `glass_diff` / `glass_wait_for_region`.

- `name` (string, **required**) — baseline name.

### `glass_diff`

Diff the current frame against a named baseline; returns change stats and a bounding box **as text**.

- `name` (string, **required**) — baseline to compare against.
- `mode` (string) — `"perceptual"` (default) or `"exact"`.
- `threshold` (number, default `0.1`) — perceptual sensitivity, `0..1`; smaller is stricter.
- `tolerance` (integer 0–255, default `0`) — per-channel tolerance for `mode:"exact"`.
- `include_image` (boolean, default false) — also return the current frame cropped to the changed
  region. No image is returned when nothing changed.

Returns `changed_pct` and a `bbox`; only attaches an image when `include_image:true` and something
changed.

## Settling & waiting

All four return text and time out **softly** with `{matched:false}` (or `{settled:false}`) rather
than erroring — branch on that instead of retrying blindly.

### `glass_wait_stable`

Wait until the window stops changing, then return the settled frame.

- `include_image` (boolean, default true) — set false for a text-only `{settled,width,height}`
  result with no image (cheap before a text `glass_diff`); `region` is ignored when false.
- `region` (`{x,y,width,height}`) — crop the returned frame.
- `stability_region` (`{x,y,width,height}`) — watch only this sub-rectangle for settling, ignoring
  unrelated motion (a clock, a spinner) elsewhere. Independent of `region`.
- `settle_frames` (integer) — consecutive stable frames required.
- `interval_ms` (integer) — sample interval.
- `timeout_ms` (integer) — give up after this long.
- `tolerance` (integer 0–255) — per-frame change tolerance.
- `window_id` (integer) — observe this window (id from `glass_list_windows`) instead of the active
  one, without changing which window subsequent ops target.

### `glass_wait_for_element`

Block until a UI element reaches a precise state, then return it as text. Errors if the app exposes
no accessibility tree.

- `name` (string) — substring of the element's accessible name (selector).
- `role` (string) — element role filter, e.g. `"Button"`, `"ProgressBar"` (selector).
- `condition` (string, default `appears`) — one of `appears`, `disappears`, `enabled`, `disabled`,
  `checked`, `unchecked`, `selected`, `unselected`, `expanded`, `collapsed`, `focused`, `visible`,
  `hidden`.
- `value_contains` (string) — additionally require the matched element's value to contain this
  substring; not a standalone selector (`name` and/or `role` still required).
- `interval_ms` (integer, default 200) — poll interval (one a11y snapshot per tick).
- `timeout_ms` (integer, default 10000) — returns `{matched:false}` on timeout.

Returns `{matched, elapsed_ms, element{id, role, name, bounds, states}}` — the `id` is usable with
`glass_click_element`.

### `glass_wait_for_region`

Block until a visual region changes (diverges from a reference) or matches (converges to a saved
baseline), then return text metrics.

- `until` (string) — `"changes"` (default; diverge from reference) or `"matches"` (converge to
  `baseline`).
- `baseline` (string) — saved baseline to compare against; omit to use the frame at call start.
- `region` (`{x,y,width,height}`) — sub-rectangle to watch; omit for the whole window.
- `mode` (string) — `"perceptual"` (default) or `"exact"`.
- `threshold` (number, default `0.1`) / `tolerance` (integer 0–255, default `0`) — sensitivity.
- `interval_ms` (integer, default 100) — poll interval.
- `timeout_ms` (integer, default 10000) — returns `{matched:false}` on timeout.
- `include_image` (boolean, default false) — on match, also return the watched region as an image.
- `window_id` (integer) — observe this window (id from `glass_list_windows`) instead of the active
  one, without changing which window subsequent ops target.

Returns `{matched, changed_pct, bbox, elapsed_ms}`. Use `until:"matches"` to confirm the UI reached
an approved design without spending vision tokens.

### `glass_wait_for_log`

Block until a log line containing `contains` appears, then return it as text.

- `contains` (string, **required**, non-empty) — substring to wait for.
- `stream` (string) — `"stdout"`, `"stderr"`, or `"both"` (default).
- `cursor` (integer) — start scanning from this cursor (from a prior `glass_logs`) to catch a line
  emitted just before the call; omit to match only lines emitted after it.
- `interval_ms` (integer, default 100) — poll interval.
- `timeout_ms` (integer, default 10000) — returns `{matched:false}` on timeout.

Returns `{matched, line{seq, stream, text}, cursor, elapsed_ms}`; resume reading from the returned
`cursor`.

## Input

`glass_click`, `glass_drag`, and `glass_scroll` accept an optional `modifiers` array (e.g.
`["ctrl"]`, `["ctrl","shift"]`) held during the action — enabling shift/ctrl-click multi-select,
modified drags, and Ctrl+scroll.

### `glass_click`

Click at window-relative coordinates.

- `x`, `y` (integer, **required**) — window-relative target.
- `button` (string) — `"left"` (default), `"right"`, or `"middle"`.
- `count` (integer) — click count (e.g. `2` for double-click).
- `modifiers` (array of string) — keys held during the click.

### `glass_type`

Type a string into the focused window.

- `text` (string, **required**).

### `glass_key`

Press a key chord.

- `chord` (string, **required**) — e.g. `"ctrl+s"`, `"Return"`, `"alt+F4"`.

### `glass_scroll`

Scroll at window-relative coordinates by wheel notches.

- `x`, `y` (integer, **required**) — window-relative point.
- `dx`, `dy` (integer) — horizontal/vertical scroll in **wheel notches** (discrete clicks — small
  integers like 1–5, not pixels). Positive `dy` is wheel-down, negative wheel-up; glass clicks
  `|dy|` times. How an app maps a notch to its view (lines, pixels, zoom) is the app's choice.
- `modifiers` (array of string) — keys held during the scroll.

### `glass_drag`

Drag with a button held from one point to another.

- `x1`, `y1`, `x2`, `y2` (integer, **required**) — window-relative start and end.
- `button` (string) — mouse button held.
- `duration_ms` (integer, default 200) — span the motion over this long so a frame-based GUI
  (egui/winit) samples the path across multiple frames. Lower is faster but coarser.
- `modifiers` (array of string) — keys held during the drag.

### `glass_move`

Move the pointer to window-relative coordinates.

- `x`, `y` (integer, **required**).

### `glass_gesture`

Perform a multi-touch gesture: 2–10 pointers, each a straight `from→to` segment, all down together
at `t=0` and up at `duration_ms`. Pinch = two pointers toward/apart; rotate = two on an arc;
two-finger swipe = two parallel segments; a `from==to` pointer is held in place.

- `pointers` (array of `{ from{x,y}, to{x,y} }`, **required**) — 2–10 window-relative segments.
- `duration_ms` (integer, default 250) — gesture span.

**Android only** (requires the on-device agent — `adb`'s `input` has no multi-touch command). The
`adb` fallback and the desktop backends refuse rather than degrade to a single pointer.

### `glass_do`

Run an ordered sequence of input actions in one call (collapsing per-action round-trips), then
optionally observe.

- `actions` (array, **required**, non-empty) — each item is `{ action: "click"|"move"|"drag"|
  "scroll"|"type"|"key"|"settle", ...same fields as the matching tool }`. A `settle` action waits
  for the screen to stop changing between steps.
- `then` (`{ settle?, diff?, screenshot? }`) — a terminal observe after all actions succeed; text-
  first, returning an image only for `screenshot` (or `diff` with its own `include_image`).

Fails fast: if an action errors it reports which index failed and how many ran. Use for **known**
sequences (login, form-fill, menu→item); if you must see a result to choose the next action, don't
batch that part.

## Windows

### `glass_list_windows`

List the app's top-level windows — `id`, `title`, `class`, geometry, and which is active — as a JSON
array. Ids are not stable across calls; re-list after windows open or close.

### `glass_select_window`

Make a window active by `id` (from `glass_list_windows`). Subsequent capture/click/type/window ops
target it, with window-relative coordinates.

- `id` (integer, **required**) — window id from the latest listing.

### `glass_window`

Focus, resize, or move the active window, or read its geometry.

- `op` (string, **required**) — `"focus"`, `"resize"`, `"move"`, or `"geometry"`.
- `x`, `y` (integer) — target position for `"move"`.
- `width`, `height` (integer) — target size for `"resize"`.

Resize/move are non-goals on Android (apps are full-screen).

## Accessibility (semantic addressing)

Deterministic, low-token element addressing that complements the pixel loop. Available where the app
exposes an accessibility tree (most GTK/Qt/toolkit apps — not bare canvas/game UIs); these tools
**error** for an app with no accessible UI rather than return a fake tree, so fall back to
`glass_screenshot` then. On Linux, start the app with `glass_start`'s `a11y:true`. See
[reference/platforms.md](platforms.md) for per-OS backends (AT-SPI / UI Automation / uiautomator / AX).

### `glass_a11y_snapshot`

Capture the active window's accessibility tree as compact text. No parameters. Each line is
`#<id> <Role> "<name>" (x,y wxh) [states]`; pass an `#id` to `glass_click_element`.

### `glass_a11y_marks`

Screenshot of the active window with a numbered Set-of-Mark box on each interactable element, plus a
text legend (`#<id> <Role> "<name>"`). No parameters. Same ids as `glass_a11y_snapshot`. The box is
only as precise as the toolkit's a11y geometry (can drift ~10–20px), but the `#id` and the click are
exact.

### `glass_click_element`

Click an element by its `#id` (clicks the center of its bounds, via the normal click path).

- `id` (integer, **required**) — the `#id` from the latest snapshot.
- `return` (string) — `"snapshot"` folds a fresh a11y tree into the result (and refreshes the
  cache), `"settle"` waits for the UI to stop changing (text-only), or `"none"` (default).

### `glass_set_value`

Set an editable element's value directly via accessibility (instant, no keystrokes). Errors if the
element isn't editable, changed since the snapshot, or the app exposes no accessibility tree.

- `id` (integer, **required**) — the element's `#id`.
- `text` (string, **required**) — the value to set.
- `return` (string) — `"snapshot"`, `"settle"`, or `"none"` (default), as for `glass_click_element`.

### `glass_scroll_to_element`

Scroll a container until an accessibility element becomes visible, then return it as text. In a
virtualized list only the on-screen rows exist in the a11y tree, so an off-screen row can't be clicked
until it's scrolled into view; this collapses the scroll-and-snapshot loop into one call. Errors if the
app exposes no accessibility tree.

- `name` (string) — substring of the target's accessible name (selector); `name` and/or `role` is
  required.
- `role` (string) — role filter, e.g. `"ListItem"`, `"Button"` (selector).
- `value_contains` (string) — additionally require the matched element's value to contain this
  substring; not a standalone selector.
- `direction` (string, default `down`) — primary sweep direction (`"down"`/`"up"`); the search
  reverses to the other end if the target isn't found first.
- `x`, `y` (integer) — scroll anchor (window-relative); default to the window center. Set both to aim
  the wheel at a specific scrollable container.
- `step` (integer, default 3) — wheel notches per scroll step; larger covers distance faster but risks
  stepping past a row's realized band.
- `timeout_ms` (integer, default 20000) — returns `{matched:false}` on timeout.

Returns `{matched, elapsed_ms, element{id, role, name, bounds, states}, scrolled{steps, reversed}}` —
the `id` is usable with `glass_click_element`.

## Clipboard

Both act on the **app's** clipboard, isolated from your real clipboard on the private Xvfb/sway
backends, on a contained Windows app (a private boxed clipboard), and on a contained macOS app that
isn't built with Apple's hardened runtime (a shim redirects it to a private pasteboard glass shares).
A hardened-runtime macOS app (App Store / notarized) can't be redirected, so these return
`Unsupported`; on Android they need the on-device agent. Only shared-desktop modes (`GLASS_DISPLAY=:0`,
or the Windows/macOS backend with `sandbox:off`) touch your **real** clipboard. See
[explanation/containment.md](../explanation/containment.md#clipboard-isolation).

### `glass_clipboard_get`

Read the clipboard as text (`""` if empty). No parameters. Also the cheap text-extraction path:
`glass_do` `ctrl+a` then `ctrl+c`, then read here — faster and token-free versus OCR for any app
with selectable text.

### `glass_clipboard_set`

Write text to the clipboard so the app can paste it.

- `text` (string, **required**) — the text to write.

## Logs & diagnostics

### `glass_logs`

Read captured stdout/stderr log lines with a resumable cursor.

- `contains` (string) — return only lines containing this substring.
- `stream` (string) — `"stdout"`, `"stderr"`, or `"both"` (default).
- `cursor` (integer) — resume from this cursor.
- `max_lines` (integer) — cap the number of lines returned.

### `glass_doctor`

Diagnose the glass environment (display dependency, containment runtime, software GL, external tool
paths) and report per-check status with a remedy for anything missing. Use it to self-diagnose a
`glass_start` failure.

- `deep` (boolean, default false) — also spawn and tear down the default backend's headless display
  to verify it actually starts (slower).

Mirrors the `glass-mcp doctor` CLI — see [reference/cli.md](cli.md).
