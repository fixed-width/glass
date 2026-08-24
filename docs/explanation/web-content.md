# Web content

A browser tab or an embedded web view is not like the rest of a native UI: the accessibility tree
for it is built by a web engine (Chromium, WebKit, Gecko) sitting inside the app, on its own
schedule, not by the toolkit glass reads everywhere else. That difference is why web content gets
its own page rather than a line in the roles reference.

## Publication is lazy, and conditional

A web engine does not build its accessibility tree just because a page loaded. Most engines build
it only once they believe an assistive technology is present — a signal that follows something
other than the page load. Until an engine reaches that belief, the tree it hands to the OS's
accessibility layer is empty or absent, no matter how populated the rendered page is. glass treats
this as a mechanism to handle, not a guarantee: it discloses what it actually read rather than
inferring what should be there from the DOM the agent can't see.

## What glass does at launch

Nothing extra, on every platform. glass never pokes a browser to turn its accessibility on. On
Linux, the private AT-SPI bus that `glass_start` already spawns for every a11y-enabled session *is*
the presence signal — a browser asking whether an AT is running finds one already there, so there
is no separate step for web content. On Windows, macOS and Android the engines examined published
at baseline against the readers glass already runs, with no extra lever pulled to get there.

## What an agent sees

Web content shows up in a `glass_a11y_snapshot` outline as one of three shapes, plus a fourth,
older case that isn't specific to web content at all:

- **A populated `Document`.** The engine has published its tree; the page's headings, links and
  fields are the `Document`'s children, addressable by id like any other element.
- **A childless `Document`, plus a notice.** The engine hasn't published yet, the page is genuinely
  empty, or the walk was cut short before reaching the content — the outline alone can't tell those
  apart. The notice names the element by id and bounds and asks for a fresh snapshot first; only if
  the page is still empty on that re-read does it steer to pixels.
- **A withheld placeholder, plus a different notice.** Some apps publish a stand-in element for a
  web view whose accessibility is off entirely. Nothing behind a placeholder can ever be addressed
  by id — a re-snapshot won't change that — so this notice goes straight to `glass_screenshot`, then
  `glass_click` at x,y.
- **No accessibility elements at all** (`empty_guidance`) is the pre-existing case for any app that
  publishes no tree — canvas apps, toolkits that need a11y turned on. It is not web-specific, and a
  page can also fall into it if the surrounding app itself exposes nothing.

## Readings, by platform

Every claim below is what a specific engine build did on the date read, not a fact about the
engine — a later version, or the same engine under a different embedder, can differ.

| Platform | Engines read (2026-08-24) | What was read |
|---|---|---|
| Linux (AT-SPI) | Firefox 153 | Publishes `document web` at baseline (0.3–0.9s) on both X11 and Wayland; a nested `<iframe>` is its own `Document`. |
| Linux (AT-SPI) | Brave 151 (Chromium) | Publishes a null placeholder child while renderer accessibility is off — read as withheld content, not an empty page. |
| Windows (UIA) | Edge 151, Brave 151, Firefox 154 | Publish at baseline (0.4–3.3s). A web page's `Document` and a text editor's edit surface report the same UIA control type; they're told apart per element by the `FrameworkId` each reports (`Chrome`/`Gecko` vs. `Win32`). Clicks and `set_value` land. |
| macOS (AX) | Safari 26.5 (WebKit) | Publishes `AXWebArea` in the very first snapshot — reading the tree is itself what materializes the lazily built web area. `AXPress` clicks land; `set_value` on a web input is refused honestly (`AxValueNotApplied`) — type into it instead. |
| Android | both readers | A WebView's page arrives as `Document` twice — the host view and the page root. The first snapshot right after launch can show a childless `Document`; the next one holds the page. |
| iOS (idb) | WKWebView | No element at all — not the view, not the page, not an empty web area. See below. |

## What stays open: iOS

Every other platform's failure mode is a `Document` that arrived empty or as a placeholder — a
node the agent can see and act on the disclosure for. On iOS, a `WKWebView`'s page contributes
nothing to the tree `idb` hands back: no view, no content, no node to attach a notice to. The
matrix records this as `unmarked` rather than `absent`, because the control is on screen and real —
it just never surfaces through this reading path. Closing this gap needs a different signal than
the childless-`Document` mechanism the other platforms use, since there is no element here for that
mechanism to act on.
