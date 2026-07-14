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
  caching them. (Wire types for both are in [Type conventions](#type-conventions) below.)
- **No silent fallbacks.** A failed capture or input returns a structured error, never a blank or
  stale frame.
- **Unknown enum values are rejected, not silently coerced.** An out-of-set value for any closed
  choice — `button`, `op`, `condition`, `direction`, `mode`, `stream`, `backend`, `sandbox`, a
  `glass_do` action kind, and so on — comes back as a structured error naming the valid options.

## Result envelope

Every tool returns, on success, one leading text content block in a fixed shape:

`{ "ok": true, "tool": "<tool name>", "result": { ... } }`

Each tool's entry below gives its `result` shape as a "Returns" line.

`result` holds only glass-computed or glass-echoed fields — ids, geometry, counts, elapsed times,
matched flags. Bulk text the *target app* controls — the `glass_a11y_snapshot` outline, `glass_logs`
lines, clipboard text, the `glass_list_windows` array (window titles are app-supplied), the
`glass_a11y_marks` legend — never rides inside `result`. It follows as its own subsequent content
block, wrapped in an untrusted marker, so an app that puts an instruction-shaped string in a window
title or a log line can't pass it off as glass itself instructing the agent.

A capture tool (`glass_screenshot`, `glass_wait_stable` with an image, `glass_a11y_marks`, and
`glass_diff` / `glass_wait_for_region` when they attach one) emits the image content block *first*,
then the envelope, then a trailing note that the image is untrusted too. Every other tool — including
`glass_do`'s optional `then.screenshot`/`then.diff` image — puts the envelope first, with any sibling
blocks (an image, an app-controlled text block) following it.

A failed call comes back as an MCP **error** result, not this envelope — check for an error before
parsing `result`.

Most input/action tools (`glass_click`, `glass_move`, `glass_drag`, `glass_scroll`, `glass_gesture`,
`glass_type`, `glass_key`, `glass_stop`, `glass_clipboard_set`) return an empty `{}` — `ok:true` in
the envelope is itself the confirmation that the action ran.

## Type conventions

Exact wire types for the ids and coordinates used throughout this reference (freshness rules for
ids are in [Conventions](#conventions) above):

- **Element ids** — the `#id` in a `glass_a11y_snapshot` line, and the `id` param of
  `glass_click_element` / `glass_set_value` — are `u32`.
- **Window ids** — `glass_list_windows`' `id`, `glass_select_window`'s `id` param, and every tool's
  `window_id` param — are `u64`, carrying the platform's own window handle.
- **Input coordinates** — `x`/`y` (and `x1,y1,x2,y2`, and gesture `from`/`to`) on
  `glass_click`/`glass_move`/`glass_drag`/`glass_scroll`/`glass_gesture` — are signed `i32`,
  window-relative. A negative value addresses a point off the window's top-left edge rather than
  being rejected.
- **Region coordinates** — `region`/`stability_region` (`x,y,width,height`), wherever a tool accepts
  one — are unsigned `u32`; they can never be negative.
- `glass_logs`' `max_lines` is a `u32`.

## Session lifecycle

### `glass_start`

Build, launch, and locate a native GUI app; returns its window geometry.

- `run` (array of string, **required**) — program and arguments; `run[0]` is the executable.
- `build` (string) — shell command run in `cwd` before launching.
- `cwd` (string) — working directory for `build` and `run`.
- `env` (object) — extra environment variables for the launched app, as `{ "KEY": "VALUE" }` pairs.
- `backend` (string) — `"x11"` or `"wayland"` (Linux), `"windows"` (Windows host), `"macos"` (macOS
  host), `"android"` (an AVD emulator, any host), or `"ios"` (an iOS Simulator, macOS host). Omit for
  the server default (`GLASS_BACKEND`, else `windows` on Windows, `macos` on macOS, else `x11`).
- `sandbox` (string) — `"default"`, `"strict"`, or `"off"`. Omit for the server default
  (`GLASS_SANDBOX`, else `default`). See [explanation/containment.md](../explanation/containment.md).
- `window_hint` (`{ title?, class? }`) — disambiguate which window is the app's when several appear,
  or find a window the launched process hands off to an unrelated process (some packaged Windows
  apps). `title` is a case-insensitive substring; `class` is an exact match.
- `a11y` (boolean, default false) — **Linux only.** Spawn a private AT-SPI bus so the accessibility
  tools work against this app. Opt-in, since it spawns extra processes.
- `timeout_ms` (integer) — launch timeout.

Returns the located window's geometry: `{x, y, width, height}`.

### `glass_stop`

Stop the running app and end the session. No parameters. Returns `{}`.

## Capture & visual comparison

### `glass_screenshot`

Capture the app window, or an optional sub-rectangle, as a lossless WebP image.

- `region` (`{ x, y, width, height }`, window-relative) — capture just this rectangle; omit for the
  whole window. Vision cost scales with pixel area, so a tight region is a recurring token saving.
- `window_id` (integer) — capture this window (id from `glass_list_windows`) instead of the active
  one, without changing which window subsequent ops target. Omit for the active window.

Returns `{width, height}` — the captured frame's dimensions — plus `x, y` (the region's origin) when
`region` was given.

### `glass_baseline_save`

Save the current frame as a named visual baseline for later `glass_diff` / `glass_wait_for_region`.

- `name` (string, **required**) — baseline name.

Returns `{name}`, echoing the saved name.

### `glass_diff`

Diff the current frame against a named baseline; returns change stats and a bounding box **as text**.

- `name` (string, **required**) — baseline to compare against.
- `mode` (string) — `"perceptual"` (default) or `"exact"`.
- `threshold` (number, default `0.1`) — perceptual sensitivity, `0..1`; smaller is stricter.
- `tolerance` (integer 0–255, default `0`) — per-channel tolerance for `mode:"exact"`.
- `include_image` (boolean, default false) — also return the current frame cropped to the changed
  region. No image is returned when nothing changed.
- `region` (`{x,y,width,height}`) — window-relative sub-rectangle to diff; omit to diff the whole
  window. Scopes the comparison (and the reported `bbox`, which becomes region-relative) to just
  this area — the way to ask "did *only* this part change?".

Returns `{changed_pixels, total_pixels, changed_pct, aa_ignored, bbox}` (`bbox` is `null` when
nothing changed), plus the given `region` echoed back when one was passed; only attaches an image
when `include_image:true` and something changed.

## Settling & waiting

All four return text and time out **softly** with `{matched:false}` (or `{settled:false}`) rather
than erroring — branch on that instead of retrying blindly.

### `glass_wait_stable`

Wait until the window stops changing, then return the settled frame.

- `include_image` (boolean, default true) — set false for a text-only result (no image; cheap
  before a text `glass_diff`); `region` is ignored when false.
- `region` (`{x,y,width,height}`) — crop the returned frame.
- `stability_region` (`{x,y,width,height}`) — watch only this sub-rectangle for settling, ignoring
  unrelated motion (a clock, a spinner) elsewhere. Independent of `region`.
- `settle_frames` (integer) — consecutive stable frames required.
- `interval_ms` (integer) — sample interval.
- `timeout_ms` (integer) — give up after this long.
- `tolerance` (integer 0–255) — per-frame change tolerance.
- `window_id` (integer) — observe this window (id from `glass_list_windows`) instead of the active
  one, without changing which window subsequent ops target.

Returns `{settled, saw_motion, observed_ms, width, height}`; `x, y` — the region's origin — are
added only when `include_image` attached a frame and `region` was given (the text-only result never
includes them). `saw_motion` and `observed_ms` make `settled` non-opaque: `settled:true` with
`saw_motion:false` over a short `observed_ms` is only a brief quiet window, not necessarily a
finished animation.

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

Returns `{matched, elapsed_ms, element{id, role, name, value, bounds, states}}` — `element` is
`null` when `matched:false`; otherwise its `id` is usable with `glass_click_element`.

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
an approved design without spending vision tokens. For the non-blocking case — one already-captured
frame instead of polling — `glass_diff` takes the same `region`.

### `glass_wait_for_log`

Block until a log line containing `contains` appears, then return it as text.

- `contains` (string, **required**, non-empty) — substring to wait for.
- `stream` (string) — `"stdout"`, `"stderr"`, or `"both"` (default).
- `cursor` (integer) — start scanning from this cursor (from a prior `glass_logs`) to catch a line
  emitted just before the call; omit to match only lines emitted after it.
- `interval_ms` (integer, default 100) — poll interval.
- `timeout_ms` (integer, default 10000) — returns `{matched:false}` on timeout.

Returns `{matched, line{seq, stream, text}, cursor, elapsed_ms}` (`line` is `null` when unmatched),
plus `note` on a default-cursor timeout when the substring was already in the log before this call —
it points you at `cursor:0`. Resume reading from the returned `cursor`.

## Input

Every tool in this section returns an empty `result:{}` on success — `ok:true` in the envelope is
itself the confirmation that the action ran; there is nothing else to report.

`glass_click`, `glass_drag`, and `glass_scroll` accept an optional `modifiers` array — `"ctrl"`,
`"shift"`, `"alt"`, or `"super"` (e.g. `["ctrl"]`, `["ctrl","shift"]`; macOS calls this key ⌘ and
also accepts `"cmd"` as an alias) — held during the action, enabling shift/ctrl-click multi-select,
modified drags, and Ctrl+scroll.

On the **iOS** backend `glass_click`, `glass_type`, `glass_key`, `glass_scroll`, and `glass_drag`
drive the Simulator over `idb_companion` (install it — see
[setup-ios.md](../how-to/setup-ios.md#input--accessibility)); only multi-touch `glass_gesture` is
unsupported there.

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
  integers like 1–5, not pixels). Positive `dy` is wheel-down, negative wheel-up; positive `dx`
  reveals content to the **right**, negative to the left. glass clicks `|dx|`/`|dy|` times. How an
  app maps a notch to its view (lines, pixels, zoom) is the app's choice.
- `modifiers` (array of string) — keys held during the scroll.

> **On touch backends (Android, iOS), `glass_scroll` is a real one-finger swipe — it is *input*,
> not an inert viewport nudge.** There is no wheel on touch; glass reproduces a scroll as a finger
> drag anchored at `x,y`, travelling roughly `notches × 120 px` opposite the wheel direction (the
> resulting pan is then amplified and made non-linear by the view's fling/deceleration, so it is not
> a fixed distance per notch). Three things follow:
>
> - **It can mutate app state.** Over an *interactive* surface — a drawing canvas, a slider, a
>   swipe-to-act row — the swipe registers as input (e.g. commits a stroke). Scroll from an inert
>   part of the container, or start the anchor on a non-actionable element.
> - **A scroll against the container's edge is an expected no-op.** At a scroll boundary there is
>   nothing to reveal in that direction, so nothing moves — and the tool still returns `ok`. That is
>   not a failure or a dropped `dx`; scroll the other way, or from a position that has room.
> - **Verify a pan by the accessibility tree, not a whole-window diff.** A thin container (a
>   toolbar) pans only a small fraction of the window, so `glass_diff`'s `changed_pct` barely moves
>   even when the scroll worked. Snapshot before/after and compare a container element's `bounds`
>   (they shift by the pan distance); items scrolled off-screen keep reporting `bounds` outside
>   `[0,width)`, which is the tell that it panned.

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
two-finger swipe = two parallel segments; a `from==to` pointer is held in place. Multi-touch isn't
available on every backend — it returns a clear `Unsupported` error where the active backend can't
do it.

- `pointers` (array of `{ from{x,y}, to{x,y} }`, **required**) — 2–10 window-relative segments.
- `duration_ms` (integer, default 250) — gesture span.

**Platform notes:** multi-touch is currently implemented on the Android backend (via the optional
on-device companion agent); other backends return `Unsupported`.

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

Returns `{executed}` (the number of actions that ran) plus, when `then` was given, a `then` object
keyed by whichever of `settle`/`diff`/`screenshot` you asked for — each key holds that sub-tool's
own `result` shape from its entry above.

## Windows

### `glass_list_windows`

List the app's top-level windows. Returns `{count}`; the window array itself — `id`, `title`,
`class`, geometry, and which is active, as JSON — rides as an untrusted sibling text block, since a
window's `title` is app-controlled text. Ids are not stable across calls; re-list after windows open
or close.

### `glass_select_window`

Make a window active by `id` (from `glass_list_windows`). Subsequent capture/click/type/window ops
target it, with window-relative coordinates.

- `id` (integer, **required**) — window id from the latest listing.

Returns the now-active window's geometry: `{x, y, width, height}`.

### `glass_window`

Focus, resize, or move the active window, or read its geometry.

- `op` (string, **required**) — `"focus"`, `"resize"`, `"move"`, or `"geometry"`.
- `x`, `y` (integer) — target position for `"move"`.
- `width`, `height` (integer) — target size for `"resize"`.

Resize/move are non-goals on Android and iOS (apps are full-screen); those backends serve `"focus"`
and `"geometry"` but return an unsupported error for `"resize"`/`"move"`.

Returns the window's geometry after the op: `{x, y, width, height}`.

## Accessibility (semantic addressing)

Deterministic, low-token element addressing that complements the pixel loop. Available where the app
exposes an accessibility tree (most GTK/Qt/toolkit apps — not bare canvas/game UIs); these tools
**error** for an app with no accessible UI rather than return a fake tree, so fall back to
`glass_screenshot` then. On Linux, start the app with `glass_start`'s `a11y:true`. The **iOS** backend
reads the Simulator's accessibility tree over `idb_companion` (install it — see
[setup-ios.md](../how-to/setup-ios.md#input--accessibility)). See
[reference/platforms.md](platforms.md) for per-OS backends (AT-SPI / UI Automation / uiautomator / AX / idb).

### `glass_a11y_snapshot`

Capture the active window's accessibility tree as compact text. No parameters. Returns `{}`; the
tree itself rides as an untrusted sibling text block, one line per element:
`#<id> <Role> "<name>" (x,y wxh) [states]`; pass an `#id` to `glass_click_element`.

### `glass_a11y_marks`

Screenshot of the active window with a numbered Set-of-Mark box on each interactable element, plus a
text legend (`#<id> <Role> "<name>"`). No parameters. Returns `{count}` — the number of marked
elements; the image and the legend text follow as siblings (the legend untrusted-wrapped), per the
image ordering above. Same ids as `glass_a11y_snapshot`. The box is only as precise as the toolkit's
a11y geometry (can drift ~10–20px), but the `#id` and the click are exact.

### `glass_click_element`

Click an element by its `#id` (clicks the center of its bounds, via the normal click path).

- `id` (integer, **required**) — the `#id` from the latest snapshot.
- `return` (string) — `"snapshot"` appends a fresh a11y outline as an untrusted sibling block (and
  refreshes the id cache), `"settle"` folds settle metadata into `result.observed`, or `"none"`
  (default) adds nothing.

Returns `{id}` — the `#id` you clicked — plus `observed: {settled, saw_motion, observed_ms}` when
`return:"settle"`.

### `glass_set_value`

Set an editable element's value directly via accessibility (instant, no keystrokes). Errors if the
element isn't editable, changed since the snapshot, or the app exposes no accessibility tree.

- `id` (integer, **required**) — the element's `#id`.
- `text` (string, **required**) — the value to set.
- `return` (string) — `"snapshot"`, `"settle"`, or `"none"` (default), as for `glass_click_element`.

Returns `{id}` plus `observed: {settled, saw_motion, observed_ms}` when `return:"settle"`, exactly
as for `glass_click_element`.

### `glass_scroll_to_element`

Scroll a container on **either axis** until an accessibility element is **on-screen**, then
return it as text. The element must be actually visible (intersecting the viewport), not merely
present in the a11y tree — so the returned `id` is usable with `glass_click_element` even for a
non-virtualized container (a horizontal toolbar) whose off-screen items are always in the tree.
Errors if the app exposes no accessibility tree.

- `name` (string) — substring of the target's accessible name (selector); `name` and/or `role`
  is required.
- `role` (string) — role filter, e.g. `"ListItem"`, `"Button"` (selector).
- `value_contains` (string) — additionally require the matched element's value to contain this
  substring; not a standalone selector.
- `direction` (string) — `"up"`/`"down"` (vertical) or `"left"`/`"right"` (horizontal). **Omit
  to infer** the direction from the target's off-screen position (e.g. an item at `x ≥ width`
  scrolls right); inference falls back to a vertical `down`→`up` sweep when the target isn't in
  the tree yet (a virtualized list). The search reverses to the other end if not found first.
- `x`, `y` (integer) — scroll anchor (window-relative). By default the swipe anchors on the
  target's own row/column, so a container that isn't centered in the window (a top toolbar) is
  driven correctly; set both to override (e.g. for an empty-tree virtualized list where there's
  no target row to anchor on yet).
- `step` (integer, default 3) — wheel notches per scroll step. A calibration escape hatch —
  larger covers distance faster but risks stepping past a row's/column's realized band.
- `timeout_ms` (integer, default 20000) — returns `{matched:false}` on timeout.

Returns `{matched, elapsed_ms, element{id, role, name, value, bounds, states}, scrolled{steps,
reversed, direction}}` — `element` is `null` when `matched:false`; `direction` is the resolved
(possibly inferred) sweep direction, and the `id` is usable with `glass_click_element`.

## Clipboard

Both act on the app's clipboard. How isolated that is from your real desktop clipboard — or whether
it *is* your real clipboard — depends on the backend and sandbox; see the Platform notes on each tool
below, and [explanation/containment.md](../explanation/containment.md#clipboard-isolation) for the
mechanism.

### `glass_clipboard_get`

Read the app's clipboard as text (`""` if empty). No parameters. On success, `result` is `{}`; the
clipboard text itself rides as an untrusted sibling text block. Also the cheap text-extraction path:
`glass_do` `ctrl+a` then `ctrl+c`, then read here — faster and token-free versus OCR for any app
with selectable text. Returns `Unsupported` where the backend can't provide clipboard access.

**Platform notes:** clipboard containment depends on the backend and sandbox. On the private headless
Linux displays and a contained Windows app, the clipboard is a private box isolated from your real
system clipboard. In shared-desktop mode (`GLASS_DISPLAY=:0`) or an uncontained backend
(`sandbox: off`), get/set act on your **real** system clipboard — snapshot with `glass_clipboard_get`
first to preserve it. On a contained macOS app **not** built with the hardened runtime, glass
redirects to a private pasteboard it shares (isolated, fully working); a hardened-runtime app (App
Store / notarized) can't be redirected and returns Unsupported.

### `glass_clipboard_set`

Write text to the app's clipboard so it can paste it. On success, `result` is `{}`. Returns
`Unsupported` where the backend can't provide clipboard access.

- `text` (string, **required**) — the text to write.

**Platform notes:** clipboard containment depends on the backend and sandbox. On the private headless
Linux displays and a contained Windows app, the clipboard is a private box isolated from your real
system clipboard. In shared-desktop mode (`GLASS_DISPLAY=:0`) or an uncontained backend
(`sandbox: off`), get/set act on your **real** system clipboard — snapshot with `glass_clipboard_get`
first to preserve it. On a contained macOS app **not** built with the hardened runtime, glass
redirects to a private pasteboard it shares (isolated, fully working); a hardened-runtime app (App
Store / notarized) can't be redirected and returns Unsupported.

> **iOS paste-consent:** when the app then reads a pasteboard glass wrote (`glass_clipboard_set` → an
> in-app `UIPasteboard` read), iOS raises a SpringBoard consent alert and the *first* read returns
> nothing. Click **Allow Paste** (it appears in the a11y tree) and retry — the two-step flow is in
> [setup-ios.md](../how-to/setup-ios.md#clipboard).

## Logs & diagnostics

### `glass_logs`

Read captured stdout/stderr log lines with a resumable cursor.

- `contains` (string) — return only lines containing this substring.
- `stream` (string) — `"stdout"`, `"stderr"`, or `"both"` (default).
- `cursor` (integer) — resume from this cursor.
- `max_lines` (integer, `u32`) — cap the number of lines returned.

Returns `{cursor}` — resume a later call from here; the matched lines themselves (each
`{seq, stream, text}`) ride as an untrusted sibling text block, since log output is app-controlled.

### `glass_doctor`

Diagnose the glass environment and report per-check status with a remedy for anything missing. Use
it to self-diagnose a `glass_start` failure.

- `deep` (boolean, default false) — also spawn and tear down the default backend's headless display
  to verify it actually starts (slower).

**Platform notes:** on Linux the checks cover the headless display servers (Xvfb for x11, sway for
wayland) and software GL; the report names exactly the checks it ran for the selected backend.

Returns `{report}` — the human-readable diagnostic text above, as a single string.

Mirrors the `glass-mcp doctor` CLI — see [reference/cli.md](cli.md).

### `glass_capabilities`

Report which operations can be performed **right now** on a backend — so you can check before you
act, instead of discovering an `Unsupported` error by trying. Static: no session required, works
before `glass_start`.

- `backend` (string, optional) — which backend to report: `x11`, `wayland`, `windows`, `macos`,
  `android`, `ios`. Omit for the active/default backend.

Returns JSON as `result` (no untrusted siblings — capability data is glass-computed, not read from
the app). For a backend compiled into this binary:

`{ "backend", "available": true, "capabilities": { <operation>: { "status", "note"?, "tools" } } }`

Each of the five operations — `input`, `multi_touch`, `clipboard`, `accessibility`,
`window_move_resize` — carries a live `status`, one of four states: `supported` (works now),
`degraded` (works now at reduced fidelity/coverage — `note` says what's lost and how to restore
it), `requires_setup` (a setup step is missing right now — `note` says what), or `unsupported`
(this backend never does it). `note` is present when there's something to explain (what's
degraded/missing, or a caveat — even a plain `supported` op can carry one, e.g. iOS `clipboard`
being supported but needing on-screen paste consent); omitted otherwise.

Every entry also carries `tools`: the MCP tools that operation gates, so a
`degraded`/`requires_setup`/`unsupported` entry tells you exactly which calls to expect trouble
from:

- **input** → `glass_type`, `glass_click`, `glass_key`, `glass_drag`, `glass_scroll`,
  `glass_move`, `glass_do`
- **multi_touch** → `glass_gesture`
- **clipboard** → `glass_clipboard_get`, `glass_clipboard_set`
- **accessibility** → `glass_a11y_snapshot`, `glass_a11y_marks`, `glass_click_element`,
  `glass_set_value`, `glass_wait_for_element`, `glass_scroll_to_element`
- **window_move_resize** → `glass_window`

For a valid backend **not** built into the running binary:
`{ "backend", "available": false, "reason": "..." }`.

**Platform notes:** availability is live. android `input` is `degraded` (adb-only injection
unless the on-device agent is set up) and its `multi_touch`/`clipboard` need that same agent
(`GLASS_ANDROID_AGENT_JAR`); iOS `accessibility` needs `idb_companion`; those read
`requires_setup` until set up. Desktop-backend `accessibility` is live too: it reads
`requires_setup` when the enabling stack isn't ready — the Linux AT-SPI runtime isn't installed,
the macOS Accessibility grant isn't held, or Windows UI Automation can't initialize (e.g. a
non-interactive Session 0) — and `supported` once it is. Whether a given *window* then exposes a
tree is still app-dependent (bare canvas/game UIs don't), surfaced when you call the a11y tools;
`glass_doctor` reports the same per-OS readiness in more detail.
