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
`glass_a11y_marks` legend, and the matched element from `glass_wait_for_element` /
`glass_scroll_to_element` and matched line from `glass_wait_for_log` — never rides inside `result`. It
follows as its own subsequent content block, wrapped in an untrusted marker, so an app that puts an
instruction-shaped string in an element name or a log line can't pass it off as glass itself
instructing the agent.

A capture tool (`glass_screenshot`, `glass_wait_stable` with an image, `glass_a11y_marks`, and
`glass_diff` / `glass_wait_for_region` when they attach one) emits the image content block *first*,
then the envelope, then a trailing note that the image is untrusted too. Every other tool — including
`glass_do`'s optional `then.screenshot`/`then.diff` image — puts the envelope first, with any sibling
blocks (an image, an app-controlled text block) following it.

A failed call comes back as an MCP **error** result, not this envelope — check for an error before
parsing `result`.

Most input/action tools (`glass_click`, `glass_move`, `glass_drag`, `glass_scroll`, `glass_gesture`,
`glass_type`, `glass_key`, `glass_stop`, `glass_clipboard_set`) return an empty `{}` — `ok:true` in
the envelope is itself the confirmation that the action ran. (`glass_type` can fold an optional
observe into that result via `return` — see its entry.)

## Type conventions

Exact wire types for the ids and coordinates used throughout this reference (freshness rules for
ids are in [Conventions](#conventions) above):

- **Element ids** — the `#id` in a `glass_a11y_snapshot` line, and the `id` param of
  `glass_click_element` / `glass_set_value` — are `u32`.
- **Window ids** — `glass_list_windows`' `id`, `glass_select_window`'s `id` param, and every tool's
  `window_id` param — are `u64`, carrying the platform's own window handle.
- **Input coordinates** — `x`/`y` (and `x1,y1,x2,y2`, and gesture `from`/`to`) on
  `glass_click`/`glass_move`/`glass_drag`/`glass_scroll`/`glass_gesture` — are signed `i32`,
  window-relative. The type is signed, but the accepted range is not: a point outside the window,
  negative or past its width/height, is rejected with `CoordOutOfBounds` naming the window size,
  before the backend sees it. For a drag or gesture, *every* endpoint is checked, so a partly
  out-of-range path is refused rather than clipped.
- **Region coordinates** — `region`/`stability_region`/the rects inside `ignore`
  (`x,y,width,height`), wherever a tool accepts one — are unsigned `u32`; they can never be
  negative.
- `glass_logs`' `max_lines` is a `u32`.

## Session lifecycle

### `glass_start`

Build, launch, and locate a native GUI app; returns its window geometry.

- `run` (array of string, **required**) — what to launch, then its arguments. `run[0]` is the
  executable on a desktop backend, an `.app` path or bundle id on `ios`, and a
  `package/.Activity` component — optionally with an `.apk` to install first — on `android`.
  For example: `run: ["/absolute/path/app.apk", "com.example.app/.MainActivity"]`. Android accepts
  relative (`package/.Activity`) and fully qualified (`package/com.example.Activity`) components.
  The APK is optional and is installed with `adb install -r -t`, replacing an existing installation.
  Invalid lengths or malformed components fail with the expected form. `glass_stop` force-stops the
  component package; it leaves the installed APK and emulator intact.
  `run[1..]` are the app's own arguments, passed to the launched process; `android` launches an
  activity rather than a command line, so it has nowhere to put them and returns an error naming
  what it could not use rather than ignoring it.
- `build` (string) — shell command run in `cwd` before launching.
- `cwd` (string) — working directory for `build` and `run`.
- `env` (object) — extra environment variables, as `{ "KEY": "VALUE" }` pairs. They reach the
  launched app on the desktop backends and on `ios`; on `android` they configure the `build`
  command on the host only, since an app launched by `am start` is forked from zygote and never
  sees the shell's environment.
- `backend` (string) — `"x11"` or `"wayland"` (Linux), `"windows"` (Windows host), `"macos"` (macOS
  host), `"android"` (an AVD emulator, any host), or `"ios"` (an iOS Simulator, macOS host). Omit for
  the server default (`GLASS_BACKEND`, else `windows` on Windows, `macos` on macOS, else `x11`).
- `sandbox` (string) — `"default"`, `"strict"`, or `"off"`. Omit for the server default
  (`GLASS_SANDBOX`, else `default`). See [explanation/containment.md](../explanation/containment.md).
- `window_hint` (`{ title?, class? }`) — disambiguate which window is the app's when several appear,
  or find a window the launched process hands off to an unrelated process (some packaged Windows
  apps). `title` is a case-insensitive substring; `class` is an exact match.
- `a11y` (boolean, default true) — **Linux only.** Spawn a private AT-SPI bus so the accessibility
  tools work against this app. On by default (the accessibility path is the cheap, low-token way to
  drive a UI); pass `false` to skip the bus for canvas/pixel-only apps, since it spawns extra
  processes. Other backends read accessibility ambiently and ignore this flag.
- `timeout_ms` (integer) — launch timeout.

Returns the located window's geometry: `{x, y, width, height}`.

### `glass_stop`

Stop the running app and end the session. No parameters. Returns `{}`.

The app is asked to close before anything terminates it, so it runs its own shutdown path and
saves whatever it saves on quit. This matters for the *next* run: an app that records whether it
exited cleanly — a Chromium-based browser, say — otherwise opens with a crash-recovery prompt
instead of its normal first screen, so a driven app's first screen changes from run to run.

An app that ignores the request, or blocks on a "save changes?" prompt, is terminated anyway after
a short grace, so `glass_stop` always completes. That case is reported on stderr rather than in the
result, along with what to do about it where anything can be — the result stays `{}` either way.

Two exceptions: a Windows app launched under Sandboxie containment (`sandbox` of `default` or
`strict`) is terminated without being asked, because a close request from outside the box never
reaches it; and an app with no window to ask is terminated immediately — one that opened none, or
(on X11) a client that never opted into the `WM_DELETE_WINDOW` protocol, which has no handler for
the request and will not act on it. Toolkit apps opt in; a bare-Xlib one may not.

One reporting limit: on Wayland the compositor performs the ask, and it disconnects a client that
has no close protocol instead of asking it. glass sees the same thing either way — the window is
gone — so that case is reported as a clean close rather than as a termination. The X11 backend
reads the protocol list itself and does distinguish them.

## Capture & visual comparison

Glass uses four verification terms consistently:

- **Current semantic state**: one accessibility observation from `glass_a11y_snapshot`.
- **Current visual evidence**: one pixel observation from `glass_screenshot` or `glass_diff`.
- **Transition completion**: a requested semantic or pixel condition reached by
  `glass_wait_for_element` or `glass_wait_for_region`.
- **Visual quiescence**: consecutive frames stopped changing in `glass_wait_stable`; this does not
  prove that a particular semantic state or approved visual design was reached.

Choose the strongest check for the claim: exact text uses `glass_wait_for_element` with `name`,
`description` and/or `role` plus `value`; dialog dismissal uses `condition:"disappears"`;
canvas change uses `glass_wait_for_region`; animation completion uses `glass_wait_stable`.

### `glass_screenshot`

Capture current visual evidence from the app window, or an optional sub-rectangle, as a lossless
WebP image. A screenshot proves only visible pixels at capture time, not semantic state, transition
completion, or stability.

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

Compare current visual evidence with a named baseline; returns change stats and a bounding box
**as text**. This is one comparison, not a wait for transition completion or stability.

- `name` (string, **required**) — baseline to compare against.
- `mode` (string) — `"perceptual"` (default) or `"exact"`.
- `threshold` (number, default `0.1`) — perceptual sensitivity, `0..1`; smaller is stricter.
- `tolerance` (integer 0–255, default `0`) — per-channel tolerance for `mode:"exact"`.
- `include_image` (boolean, default false) — also return the current frame cropped to the changed
  region. No image is returned when nothing changed.
- `region` (`{x,y,width,height}`) — window-relative sub-rectangle to diff; omit to diff the whole
  window. Scopes the comparison (and the reported `bbox`, which becomes region-relative) to just
  this area — the way to ask "did *only* this part change?".
- `ignore` (array of `{x,y,width,height}`) — window-relative rectangles excluded from the
  comparison. Use for perpetually animating content (a blinking caret, a clock, a spinner) that
  would otherwise keep `changed_pct` non-zero forever. Combines with `region`: ignore rects are
  always window-relative and are intersected with it.

Returns `{changed_pixels, total_pixels, changed_pct, aa_ignored, ignored_pixels, bbox}` (`bbox` is
`null` when nothing changed), plus the given `region` echoed back when one was passed; only attaches
an image when `include_image:true` and something changed. `ignored_pixels` is the count excluded by
`ignore`; `changed_pct` is measured over `total_pixels - ignored_pixels`.

## Settling & waiting

All four return text and time out **softly** with `{matched:false}` (or `{settled:false}`) rather
than erroring — branch on that instead of retrying blindly.

### `glass_wait_stable`

Wait for visual quiescence, then return the last frame. This proves that consecutive frames stopped
changing, not that expected semantics or pixels were reached.

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
- `ignore` (array of `{x,y,width,height}`) — window-relative rectangles excluded from the settle
  comparison. Use for perpetually animating content (a blinking caret, a clock, a spinner) that
  would otherwise keep the window from ever settling; pixels inside a rect never count as changed
  and never set `saw_motion`. Combines with `stability_region`: rects are always window-relative
  and are intersected with it. Independent of `region`, which only crops the returned image. A
  rect that falls partially or entirely outside the compared area — the frame, or the
  `stability_region` sub-rectangle when one is set — is silently clamped or dropped, masking less
  than requested or nothing at all; the excluded count is reported as `ignored_pixels`, so a
  smaller-than-expected value flags a misplaced rect.

Returns `{settled, saw_motion, observed_ms, ignored_pixels, width, height}`; `x, y` — the region's
origin — are added only when `include_image` attached a frame and `region` was given (the text-only
result never includes them). `saw_motion` and `observed_ms` make `settled` non-opaque: `settled:true`
with `saw_motion:false` over a short `observed_ms` is only a brief quiet window, not necessarily a
finished animation. `ignored_pixels` is the count an `ignore` mask excluded from the settle
comparison; when it equals the compared area, the mask covered everything and nothing was compared.

### `glass_wait_for_element`

Wait for semantic transition completion, then return the matching element as text. This verifies an
accessible condition and optional value, not pixels. Errors if the app exposes no accessibility
tree.

- `name` (string) — substring of the element's accessible name (selector).
- `description` (string) — substring of the element's accessible description (selector), useful
  for unnamed controls exposed through a hint or help label.
- `role` (string) — element role filter, e.g. `"Button"`, `"ProgressBar"` (selector).
- `condition` (string, default `appears`) — one of `appears`, `disappears`, `enabled`, `disabled`,
  `checked`, `unchecked`, `selected`, `unselected`, `expanded`, `collapsed`, `focused`, `visible`,
  `hidden`.
- `value` (string) — additionally require the matched element's complete accessible value to equal
  this string exactly (case-sensitive); not a standalone selector and mutually exclusive with
  `value_contains`.
- `value_contains` (string) — additionally require the matched element's value to contain this
  substring; not a standalone selector (`name`, `description`, and/or `role` still required). Only
  an element that reports a value can match one: on Android that is the editable elements alone (a
  change for the on-device accessibility-service reader), so filter a label, button or check box
  there on `name` or `description` instead — a `value_contains` against one waits out the whole timeout and returns
  `{matched:false}`.
- `interval_ms` (integer, default 200) — poll interval (one a11y snapshot per tick).
- `timeout_ms` (integer, default 10000) — returns `{matched:false}` on timeout.

Returns `{matched, elapsed_ms}`. On a match, the matched element (`{id, role, name, description,
value, bounds, states}`) rides as an untrusted sibling text block, since its `name`/`description`/
`value` are app-controlled; its `id` is usable with `glass_click_element`. No sibling on timeout.

### `glass_wait_for_region`

Wait for pixel transition completion: a visual region changes (diverges from a reference) or matches
(converges to a saved baseline), then return text metrics. This verifies pixels, not semantic state
or subsequent stability.

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
- `ignore` (array of `{x,y,width,height}`) — window-relative rectangles excluded from the
  comparison. Use for perpetually animating content (a blinking caret, a clock, a spinner) that
  would otherwise keep `changed_pct` non-zero forever. `changed_pct` is measured over the pixels
  that remain. Combines with `region`: ignore rects are always window-relative and are intersected
  with it. A rect that falls partially or entirely outside the compared area — the frame, or the
  `region` sub-rectangle when one is set — is silently clamped or dropped, masking less than
  requested or nothing at all; the excluded count is reported as `ignored_pixels`, so a
  smaller-than-expected value flags a misplaced rect.

Returns `{matched, changed_pct, bbox, elapsed_ms, ignored_pixels}`. Use `until:"matches"` to confirm
the UI reached an approved design without spending vision tokens. `ignored_pixels` is the count an
`ignore` mask excluded from the last comparison; when it equals the watched area, nothing was
compared. For the non-blocking case — one already-captured frame instead of polling — `glass_diff`
takes the same `region`.

### `glass_wait_for_log`

Block until a log line containing `contains` appears, then return it as text.

- `contains` (string, **required**, non-empty) — substring to wait for.
- `stream` (string) — `"stdout"`, `"stderr"`, or `"both"` (default).
- `cursor` (integer) — start scanning from this cursor (from a prior `glass_logs`) to catch a line
  emitted just before the call; omit to match only lines emitted after it.
- `interval_ms` (integer, default 100) — poll interval.
- `timeout_ms` (integer, default 10000) — returns `{matched:false}` on timeout.

Returns `{matched, cursor, elapsed_ms}`, plus `note` on a default-cursor timeout when the substring
was already in the log before this call — it points you at `cursor:0`. On a match, the matched line
(`{seq, stream, text}`) rides as an untrusted sibling text block, since log output is app-controlled;
no sibling on timeout. Resume reading from the returned `cursor`.

## Input

Every tool in this section returns an empty `result:{}` on success. `ok:true` confirms only that
glass dispatched the action without an input error; it does not prove runtime state. Verify the
expected outcome with `glass_wait_for_element` (`name`, `description` and/or `role`, plus
`value` for exact text or `value_contains` for a substring), `glass_wait_for_region`, or
`glass_wait_stable`. The one exception is
`glass_type`'s optional `return` observe, which folds settle metadata or appends an accessibility
outline into the result.

`glass_click`, `glass_drag`, and `glass_scroll` accept an optional `modifiers` array — `"ctrl"`,
`"shift"`, `"alt"`, or `"super"` (e.g. `["ctrl"]`, `["ctrl","shift"]`; macOS calls this key ⌘ and
also accepts `"cmd"` as an alias) — held during the action, enabling shift/ctrl-click multi-select,
modified drags, and Ctrl+scroll.

On the **iOS** backend `glass_click`, `glass_type`, `glass_key`, `glass_scroll`, and `glass_drag`
drive the Simulator over `idb_companion` (install it — see
[setup-ios.md](../how-to/setup-ios.md#input--accessibility)). `glass_gesture` actuates a
two-finger pinch there; other multi-touch gestures are unsupported.

### `glass_click`

Click at window-relative coordinates.

- `x`, `y` (integer, **required**) — window-relative target.
- `button` (string) — `"left"` (default), `"right"`, or `"middle"`.
- `count` (integer, default 1, range 1–10) — consecutive click count (e.g. `2` for
  double-click). Larger values are rejected before input dispatch.
- `modifiers` (array of string) — keys held during the click.

### `glass_type`

Type a string into the focused window.

- `text` (string, **required**).
- `return` (string) — `"snapshot"`, `"settle"`, or `"none"` (default), as for
  `glass_click_element`. All three values are also accepted inside a `glass_do` `type` action;
  the chosen observation is retained in that action step's `result` and sibling `content_blocks`.

Returns `{}` plus `observed: {settled, saw_motion, observed_ms}` when `return:"settle"`,
exactly as for `glass_click_element`.

### `glass_key`

Press a key chord.

- `chord` (string, **required**) — e.g. `"ctrl+s"`, `"Return"`, `"alt+F4"`.

### `glass_scroll`

Scroll at window-relative coordinates by wheel notches.

- `x`, `y` (integer, **required**) — window-relative point.
- `dx`, `dy` (integer, range -100–100) — horizontal/vertical scroll in **wheel notches**
  (discrete clicks — normal usage is small integers like 1–5, not pixels). Positive `dy` is
  wheel-down, negative wheel-up; positive `dx` reveals content to the **right**, negative to the
  left. glass clicks `|dx|`/`|dy|` times. Values outside the safe range are rejected before input
  dispatch. How an app maps a notch to its view (lines, pixels, zoom) is the app's choice.
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

**Platform notes:** the Android backend (via the optional on-device companion agent) actuates
arbitrary multi-pointer gestures. The **iOS** backend actuates a **two-finger pinch** — two pointers
whose separation changes — and refuses anything else by name: a rotation, a two-finger pan, a
stationary pair, three or more pointers, two pointers starting at the same point, or fingers
starting closer than 8pt from the centre. The pinch is delivered about the midpoint of the two
start points, so even a gesture that holds one finger still moves both. What glass preserves is the
requested scale factor, not the individual finger paths; the delivered factor differs from the
request by a few percent, and by more as the fingers start closer together. A separation change
under 5% is refused rather than actuated, because it cannot be told apart from that delivery
error. On iOS a `duration_ms` of 0 becomes 300ms, matching the backend's swipe default. It also needs an `idb_companion` that implements
the event — glass reports the binary and its build info if yours does not. Other backends return
`Unsupported`.

### `glass_do`

Prefer `glass_do` whenever at least two upcoming actions or verification waits are already known.
Use `glass_find_elements` for each target that is not already known, retain the returned ids, then
put the known mutations and waits into `glass_do`. Use `glass_a11y_snapshot` only when the task
genuinely needs broad tree inspection. Use standalone tools only when the next step depends on newly
observed state, and inspect the structured outcomes before recovery.

The call runs a bounded, fixed sequence, then optionally observes. The sequence is a static list: it
cannot use variables, references to earlier results, interpolation, branching, loops, retries, or
dynamically generated actions.

- `actions` (array, **required**, 1–64 items) — each item uses one discriminator: `click`, `move`,
  `drag`, `scroll`, `type`, `key`, `settle`, `click_element`, `set_value`, `wait_for_element`, or
  `scroll_to_element`. Each action reuses the fields of its standalone tool, including per-action
  `return` where the standalone tool supports it. A `settle` action uses `interval_ms`,
  `settle_frames`, `tolerance`, `timeout_ms`, `stability_region`, and `ignore`; it targets the active
  window and does not return an image.
- Each action has the same backend support, setup requirements, and `Unsupported` behavior as its
  standalone tool. `glass_do` changes sequencing and outcome reporting, not platform capability.
- `then` (`{ settle?, diff?, screenshot? }`) — terminal observation after every action succeeds,
  performed in that order. Images are returned only when requested by `screenshot` or by a `diff`
  whose `include_image` is true.
- `timeout_ms` (integer, default 30000, range 1–120000) — one absolute deadline shared by all
  actions and terminal observations. A blocking operation's own timeout remains a shorter ceiling
  when it expires first.

The compact UTF-8 JSON encoding of the complete arguments object may be at most 65,536 bytes.
Preflight validation failures return `invalid_sequence` without action or terminal step outcomes.
Once execution starts, the sequence is fail-fast. In a batch, `wait_for_element` or
`scroll_to_element` returning
`matched:false` fails the sequence; the standalone tools keep their soft `{matched:false}` result.

On success, `result` contains `{status, executed, steps, elapsed_ms, then?, terminal_steps?}`.
`steps` records each action's index, discriminator, `completed` status, trusted result, and any
zero-based references into the MCP content blocks.

Failed execution remains an MCP error with `is_error:true`. `executed` counts only successfully
completed actions. The failed step records `attempted`, `side_effects_may_have_occurred`,
optional `result?` evidence produced before the failure, `error.{code,summary,category?}`, and
`content_blocks`; later steps use `status:"unexecuted"`. `effects_rolled_back:false` means Glass
performed no rollback, so landed effects may persist. App-derived names,
descriptions, values, outlines, and images remain untrusted sibling blocks. Non-secret raw error
details also remain untrusted siblings. Failures from `type` and `set_value` instead expose only
sanitized category and summary diagnostics, and submitted text is never echoed in any batch output.

Do not replay a completed action or a failed action with
`side_effects_may_have_occurred:true`. `attempted:false` proves only that the failed action itself
was not dispatched. Inspect the outcomes and current app state before deciding what is safe to run.

Example, using element ids obtained before the call:

```json
{
  "actions": [
    {"action":"set_value","id":12,"text":"Alice"},
    {"action":"wait_for_element","description":"Name","role":"TextField","value":"Alice","timeout_ms":3000},
    {"action":"click_element","id":16,"return":"snapshot"},
    {"action":"wait_for_element","name":"Clicked 1","description":"Counter","timeout_ms":3000}
  ],
  "timeout_ms":10000
}
```

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
`glass_screenshot` then. On Linux the accessibility bus is on by default — only relevant if you
launched with `a11y:false`, in which case relaunch without it. The **iOS** backend
reads the Simulator's accessibility tree over `idb_companion` (install it — see
[setup-ios.md](../how-to/setup-ios.md#input--accessibility)). See
[reference/platforms.md](platforms.md) for per-OS backends (AT-SPI / UI Automation / uiautomator / AX / idb).

### `glass_find_elements`

Find a small ranked set of accessibility elements from one fresh bounded read. Use this when target
text is approximate, duplicated, or not yet identified. Returned ids are actionable with the element
tools and remain valid only until the UI changes.

- `query` (string) — case-insensitive substring over accessible name, description and non-secure
  value; optional when `role` or `states` is present.
- `role` (string) — normalized target role.
- `states` (array of string) — target predicates combined with AND.
- `within` (object) — unique semantic scope with its own `query`, `role`, and `states`.
- `max_results` (integer, default 10, range 1 through 20) — ranked result ceiling before the byte
  budget.
- `max_nodes` (integer) — the same walk-limit control as `glass_a11y_snapshot`.
- `timeout_ms` (integer, default 0) — one read at zero; a positive value waits for at least one
  match.

Ranking is deterministic: exact name, name substring, description substring, value substring, then
filter-only matches, with tree order breaking ties. Every match includes fixed compact context from
up to four ancestors, the adjacent siblings, and up to three children. Target fields and the
`within` fields each combine with AND. A `within` selector that matches more than one element is an
ambiguous-scope error rather than a guessed scope.

A positive soft timeout performs fresh bounded accessibility reads until a match arrives or the
timeout expires, then returns trusted `matched`, `timed_out`, elapsed, scope, completeness,
truncation, and omission counts. Secure values are never searched, ranked, returned, or copied into
context. Application-derived match fields and context are contained in one nonce-delimited
untrusted match block. Complete success and error text is limited to 8 KiB; lower-ranked context and
fields may be shortened, then lower-ranked matches omitted, with the trusted counters reporting
those omissions.

### `glass_a11y_snapshot`

Capture the active window's accessibility tree as compact text. Returns `{}`; the tree itself
rides as an untrusted sibling text block, one line per element:
`#<id> <Role> "<name>" desc="<description>" (x,y wxh) [states]`; pass an `#id` to
`glass_click_element`. `desc="…"` carries a secondary label the platform exposes separately from
the name — help or tooltip text, or the human-readable label where the name is a
developer-assigned id. It appears only where glass's reader sources that label: the AT-SPI (Linux)
reader reads AT-SPI `Description`, the UI Automation (Windows) reader reads `HelpText`, the AX
(macOS) reader reads `AXHelp`, else `AXDescription` where `AXTitle` already supplied the name, the
Android readers read whichever of the element's text and content-description did not become the
name or the value, the on-device companion additionally supplies an editable element's hint where
that leaves it undescribed, and the `idb` (iOS) reader reads the element's hint, falling back to
the label an editable element's identifier displaced. It is omitted when the description duplicates
the name.
`glass_wait_for_element` and `glass_scroll_to_element` accept `description` as a selector. Like
`name`, it is a case-sensitive substring match and does not match a node with no description. All
populated selector fields must match; if several nodes qualify, the first in tree order wins.

On **Android** a description drawn from the element's own two labels needs one node to carry both,
and most controls carry only one — across four stock apps, only one node in roughly three hundred
had both — so on a non-editable element expect `desc` to be absent, not routinely present. An
editable element read through the on-device companion is the exception: its hint is a source of its
own (below), needing no second label. The other readers take the description from a separate
descriptor field, so there a node with a single label can still carry one.

Both Android readers name a control the same way: the visible text is the `name`, or the
content-description where there is no text — except on an editable element, where the text is
already the `value` and the content-description is the `name` instead. A filled field's name does
not change as its text changes, on either reader.

One consequence of that rule: two controls that differ only in their content-description — "Save
draft" and "Save and close", both reading `Save` on screen — now share a `name`, and a `name:`
selector picks the first of them in tree order without reporting that a second matched. Where two
controls read alike, add a `description` or `role` filter where those differ, or address the one you
want by its `id` from a snapshot.

An editable element with no content description falls back to the leaf of its view resource id
rather than staying unnamed — `open_search_view_edit_text`, not the package-qualified
`com.android.settings:id/open_search_view_edit_text`. Settings' search box, for example, reads that
name identically from both readers. Treat the id as a label of last resort: a resource id is not
unique within a tree — ten rows built from one layout can all carry the same one — so it tells
this element apart from unrelated ones, not from its own kind.

The on-device companion adds a second source of `desc` for an editable element: its hint
(Android's placeholder text) becomes the description. That source is companion-only — a
`uiautomator` dump carries no hint attribute at all, so `uiautomator` never supplies one — so a
text field's `desc` is richer through the companion than through `uiautomator`. A field with a
hint but no content description and no resource id has nothing left to name it, so it stays
unnamed but described, rendering as `#35 TextField desc="Search settings"`, which
`glass_a11y_marks` labels from that description (see below).

An element whose platform role glass has no mapping for renders as `Other(<native token>)` — e.g.
`#4 Other(AXDisclosureTriangle) "Details" [enabled]` — so the platform's own token is still
visible; a bare `Other` means the platform named no token at all.

Web content — a browser's page, a WebView's content — arrives under a `Document` element whose
children are the page's own elements (headings, links, fields), addressable like any other. On
Windows a text editor's edit surface uses the same underlying control type and stays a
`TextArea`; the two are told apart by the framework each element reports (read 2026-08-24). A
`Document` with **no** children has nothing inside to address: the web engine has not published
its accessibility tree yet, the page is empty, or the walk did not complete — whether a bound cut
it short or it dropped a subtree it could not read — before reaching its content. The snapshot
says so in a separate notice naming the element's id and bounds, the same way a truncated tree is
disclosed. Where the walk completed, that notice steers to a fresh `glass_a11y_snapshot` first and
to the pixel path only if the page stays empty: on an Android WebView the first snapshot after a
launch was childless and the next held the whole page (read 2026-08-24). An app can also publish a
*placeholder* where the content would be — an element standing in for a web view whose
accessibility is off — and that gets its own notice: nothing behind it can be addressed by id and a
re-snapshot will not change that, so it steers straight to the pixel path. See
[Web content](../explanation/web-content.md) for what each platform's readings say.

- `max_nodes` (integer) — raise the element cap above the default (which protects the token
  budget), or `0` to remove the element-count limit. Omit for the default cap. (Structural
  depth/sibling safety rails still apply, so a pathologically deep tree can still truncate — the
  notice says which limit was hit.) A snapshot renumbers ids, so re-read them after changing this.

Roles are normalized across platforms; see [Accessibility roles by platform](a11y-roles.md) for
what each backend can produce.

### `glass_a11y_marks`

Screenshot of the active window with a numbered Set-of-Mark box on each interactable element, plus a
text legend (`#<id> <Role> "<name>"`). An element with no name but a description is labelled from it
and rendered as `#<id> <Role> desc="<description>"`, the same spelling the snapshot outline uses, so
the legend never shows a description where a matchable `name` would be.

No parameters. Returns `{count}` — the number of marked elements; the image and the legend text
follow as siblings (the legend untrusted-wrapped), per the image ordering above. Same ids as
`glass_a11y_snapshot`. The box is only as precise as the toolkit's a11y geometry (can drift
~10–20px), but the `#id` and the click are exact.

### `glass_click_element`

Address an element by its `#id`. Glass tries the platform's role-appropriate native accessibility
operation first, falling back to a synthetic pointer click at the center of the element's bounds.
For a text editor, the native operation may focus and confirm focus rather than activate it.

- `id` (integer, **required**) — the `#id` from the latest snapshot.
- `return` (string) — `"snapshot"` appends a fresh a11y outline as an untrusted sibling block (and
  refreshes the id cache), `"settle"` folds settle metadata into `result.observed`, or `"none"`
  (default) adds nothing.

Returns `{id, method}` — the addressed `#id` and which path ran (`native-action`/`pointer`).
`native-action` is the stable umbrella label for the native path, not proof that an activation verb
fired. The result also includes `native_fallback` (why the pointer path was used) when it was,
`actuated_id` when the native operation targeted a different element than the one you named (a
control whose label is a separate element, as in Jetpack Compose, resolves to the enclosing control),
and `observed: {settled, saw_motion, observed_ms}` when `return:"settle"`.

### `glass_set_value`

Where the platform can write the value directly this is instant and takes no keystrokes. On a mobile
backend (Android without the on-device accessibility service, and the iOS Simulator) it taps the
element, clears it and types, then reads the element back to confirm — up to three reads, since a
field may commit a frame or two later. Errors if the element isn't editable, changed since the
snapshot, does not hold the requested value afterwards, or the app exposes no accessibility tree.

The does-not-hold error names both values — what you asked for and what the element holds — because
three outcomes look alike without them. An element that **transformed** the write holds your text in
another form and writing again will not change that: a field that reformats (`"1234567890"` becoming
`"(123) 456-7890"`), or one that autocapitalizes (an iOS text field returning `"Hello"` for a typed
`"hello"`, intermittently — it depends on whether the field had finished emptying when the first key
arrived). An element holding **part** of your text dropped a keystroke, and writing again is the fix.
An element holding **what it held before** took no effect from the write, and only then does the
error close with what the backend that made it knows about that: a desktop element's value is often
a read-only projection, so focus it and use `glass_type` instead; a mobile write is a tap and then
keystrokes, so the tap may have missed; and a *Compose* field driven through Android's on-device
accessibility service cannot have its text *replaced*, only set into an empty field.

A cleared field (`text: ""`) that reads back holding its placeholder is the one false failure here —
the clear landed and the accessibility layer substituted the hint. Confirm it with a screenshot
rather than writing again.

One error is not a refusal: *the write went out, but could not be confirmed*. The write reached the
app and the read-back afterwards could not establish where it landed — it failed
outright, several elements now match the one you named, nothing does, the read was cut short, or the
element is no longer editable to read back. **Do not call `glass_set_value` again on that error** —
the app may have the text already, and on a backend that types, writing again types it twice.
Re-snapshot and read the element to
see what it holds. When the message names a node cap, raising `max_nodes` on the snapshot is what
fixes it. Clearing a field (`text: ""`) additionally needs the element to have a `name`: an empty
field is not by itself evidence the clear landed on the element you meant, so a nameless one reports
unconfirmed rather than a success it cannot prove.

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

- `name` (string) — substring of the target's accessible name (selector).
- `description` (string) — substring of the target's accessible description (selector), useful
  for unnamed controls exposed through a hint or help label. At least one of `name`, `description`,
  or `role` is required.
- `role` (string) — role filter, e.g. `"ListItem"`, `"Button"` (selector).
- `value_contains` (string) — additionally require the matched element's value to contain this
  substring; not a standalone selector. Only an element that reports a value can match one: on
  Android's on-device accessibility-service reader that is the editable elements alone, so filter
  a label, button or check box there on `name` or `description` instead.
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

Returns `{matched, elapsed_ms, scrolled{steps, reversed, direction}}` — `direction` is the resolved
(possibly inferred) sweep direction. On a match, the matched element (`{id, role, name, description,
value, bounds, states}`) rides as an untrusted sibling text block, since its `name`/`description`/
`value` are app-controlled; its `id` is usable with `glass_click_element`. No sibling on timeout.

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

Returns `{report, sections, overall}`. `report` is the human-readable diagnostic text above, as a
single string. `sections` is the same checks, structured: each is `{title, backend, checks}`, where
`backend` names the backend the section diagnoses (`"x11"`, `"wayland"`, …) and is `null` — always
present, never omitted — for general checks that apply to every backend. Each entry in `checks` is
`{name, status, detail, remedy?, remedy_action?}` — `status` one of
`"ok"`/`"warn"`/`"fail"`/`"skip"`. `remedy` (human text) and `remedy_action` (a command/URL a tool
can run) are each omitted when the check carries none: an `"ok"` check never has them, and a
`"warn"` or `"fail"` check may still carry neither, one, or both — so read whichever is present
rather than expecting one because the status isn't `"ok"`. `overall` is the single
verdict to branch on — one of `"ok"`/`"warn"`/`"fail"` — and already applies the same severity rule
as the rendered summary, so a failing check in a section for a backend other than the one in use
counts only as a warning: a non-default backend's problem never reads as `overall: "fail"`.

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
- **accessibility** → `glass_find_elements`, `glass_a11y_snapshot`, `glass_a11y_marks`,
  `glass_click_element`, `glass_set_value`, `glass_wait_for_element`, `glass_scroll_to_element`
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
