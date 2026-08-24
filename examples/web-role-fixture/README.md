# web-role-fixture

One page of plain HTML controls, for deciding what the web accessibility vocabulary can
express on each platform. Each control answers one question: what role does each platform's
accessibility API report for it? That answer is what a cell in
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md) records.

The table below lists what each control is for — not what any platform reported. For the readings
themselves, dated per platform and browser, see
[docs/explanation/web-content.md](../../docs/explanation/web-content.md); `a11y-roles.md` holds the
matrix cell each one settles.

| Control | Purpose |
|---|---|
| link | Hyperlink navigation |
| button | Interactive control (click me) |
| text input | Text field with label |
| text area | Multi-line text field with label |
| checkbox | Toggle control with label |
| select | Dropdown list with options |
| table | Data grid with headers and cells |
| iframe | Nested document |

## How each platform loads it

- **Linux, macOS, Windows browsers**: `file://` URL pointing to this directory's `index.html`
- **Android fixture**: Copied into the fixture's assets at build time; fixture loads it via WebView
- **iOS fixture**: Copied into the fixture's assets at build time; fixture loads it via WKWebView

Never use a top-level `data:` URL. Firefox refused a top-level `data:` navigation
(`security.data_uri.block_toplevel_data_uri_navigation`) and the page stayed blank while every
other signal looked healthy, so the run read as an engine publishing nothing (read 2026-07 on
Firefox). Keeping the fixture a file gives every platform the same stable baseline: the desktop
probes read it over `file://`, the mobile fixtures from the app's own assets.

## Verify it works

Open `index.html` in a browser by hand (`firefox file://$PWD/index.html`), then click the button.
The `"not clicked"` text should change to `"clicked"`. A fixture that doesn't work is evidence
of nothing.
