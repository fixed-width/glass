# web-role-fixture

One page of plain HTML controls, for deciding what the web accessibility vocabulary can
express on each platform. Each control answers one question: what role does each platform's
accessibility API report for it? That answer is what a cell in
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md) records.

The table below lists what each control is for — not what any platform reported. For the
readings themselves, dated per platform and browser, see
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md).

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

Never use a top-level `data:` URL — browsers load it synchronously and fixtures need a stable
`file://` baseline; the platform probe reads it over that URL, so all runs see the same content.

## Verify it works

Open `index.html` in a browser by hand (`firefox file://$PWD/index.html`), then click the button.
The `"not clicked"` text should change to `"clicked"`. A fixture that doesn't work is evidence
of nothing.
