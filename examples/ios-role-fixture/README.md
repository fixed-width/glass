# ios-role-fixture

One screen of stock UIKit controls, a collection view, and a SwiftUI screen, for deciding what
iOS's accessibility vocabulary can express. Each control answers one question: what AX role string
does the Simulator report for it? That answer is what a cell in
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md) records.

The table below is what these controls reported on an iOS 26.5 Simulator through `idb` — a
reading, not a guarantee. Re-run it rather than trusting it; the read step below prints the runtime
it saw, and `build.sh` prints the SDK it built against. The readings are bounded by what idb's
accessibility tree exposes, which may be narrower than what UIKit publishes to VoiceOver.

| Control | Reported |
|---|---|
| `UISegmentedControl` | one `AXTabGroup`, no per-segment element |
| `UIStepper` | two `AXButton`s, `Decrement` and `Increment` |
| `UIProgressView` | `AXGenericElement`, percentage in the value |
| `UIPickerView` | `AXSlider` |
| `UITableView` | `AXGroup`; rows `AXStaticText`, including one exposed as its own element |
| `UICollectionView` | `AXGroup`; cells `AXStaticText` |
| `UIAlertController` — alert and action sheet (buttons) | title, message and buttons loose under `AXApplication` |
| `UIMenu` pull-down (button) | `AXGroup` of `AXButton`s plus a `Dismiss context menu` button |
| `UITabBar` | `AXGroup` labelled `Tab Bar`; no per-item element |
| SwiftUI `List` | `AXGroup`; rows `AXStaticText`, section header `AXHeading` |
| SwiftUI `Picker`, `.inline` | `AXHeading` plus one `AXButton` per option |
| SwiftUI `Picker`, `.menu` | `AXPopUpButton` — a token glass does not map yet |
| `UIButton` with an `accessibilityHint` | `help` carries the hint verbatim: `"Saves and closes the sheet"` |
| `UITextField` with an identifier and a label | `AXTextField`; the label sits in `AXLabel` beside the id |
| `WKWebView` on the shared web page (the `web` tab) | nothing at all — neither the page's elements nor the web view itself appears |

## Build and install

Requires the full Xcode and an iOS Simulator runtime (macOS only):

```bash
./build.sh                                                    # → build/RoleFixture.app
xcrun simctl install booted build/RoleFixture.app
xcrun simctl launch booted tech.fixedwidth.glassrolefixture
```

The tab bar's items are not accessibility elements, so nothing in the tree can be aimed at to
switch tabs — and a synthetic tap at the bar's coordinates does not switch them either. The screen
is chosen at launch instead, by environment variable or by launch argument. Both reach the app
through glass (`AppSpec::env` and `AppSpec::run`'s tail) as well as through `simctl` by hand:

```bash
xcrun simctl launch booted tech.fixedwidth.glassrolefixture --tab=collection    # or: swiftui, web
SIMCTL_CHILD_ROLE_FIXTURE_TAB=swiftui xcrun simctl launch booted tech.fixedwidth.glassrolefixture
```

`simctl launch` forwards everything after the bundle id to the app verbatim, so `--tab swiftui`
works as well as `--tab=swiftui`. A `--tab` with no value, or an unrecognized name, is fatal rather
than a quiet fall back to the first screen. The collection and SwiftUI screens also name themselves
in the tree — `screen-collection` and `screen-swiftui` appear as element identifiers — and the
Controls screen is headed `Controls`.

## The `web` tab

A stock `WKWebView` on `examples/web-role-fixture/index.html` — the same page every platform's web
reading uses. `build.sh` copies it into the bundle, and the view loads it with `loadFileURL`.

```bash
xcrun simctl launch booted tech.fixedwidth.glassrolefixture --tab web
xcrun simctl io booted screenshot /tmp/web-tab.png     # the page renders in full
```

Read back through idb on an iOS 26.5 Simulator (2026-08-24), the whole screen is two nodes:
`AXApplication "Glass Role Fixture"` holding one `AXGroup "Tab Bar"`. The page's heading, button,
inputs, table and nested frame are absent, and so is the `WKWebView` itself — there is no
`AXWebArea`, and no empty container standing in for one. It stays that way for at least 30 seconds
and across taps inside the page. Apple's own Simulator Safari reads the same way: its chrome
(`Back`, `Page Menu`, `Address`, `refresh`, `More`) is exposed and the rendered page is not, so the
absence is in what idb reported here, not in this fixture.

Driving the page through glass is therefore a pixel job on this platform. The reading is
re-taken by `crates/glass-ios/tests/drive_integration.rs`'s
`web_fixture_button_and_field_respond`.

## Read the tree

With `idb_companion` running against the Simulator, using the `fb-idb` client (`idb`, a separate
Python package from the companion glass itself needs):

```bash
xcrun simctl list devices booted            # the runtime this reading is from
idb ui describe-all --udid <udid> --nested --json
```

`--nested` is the format glass reads; without it the response is a flat element list, and none of
the containment above (a group holding static text, buttons loose under the application) is
visible.

Through glass's own probe, which prints a role histogram per app:

```bash
GLASS_A11Y_PROBE_APPS="$PWD/build/RoleFixture.app" \
  cargo test -p glass-ios --test role_probe -- --ignored --nocapture
```

The probe launches the app without arguments, so it reads the Controls screen. It splits
`GLASS_A11Y_PROBE_APPS` on commas and passes each element as a whole app, so an argument cannot be
added there — set `SIMCTL_CHILD_ROLE_FIXTURE_TAB` in its environment to probe another screen.
Driving the app with a launch argument through glass is what
`crates/glass-ios/tests/launch_args_integration.rs` does (`GLASS_IOS_ROLE_FIXTURE` points it at
this app's `.app`).
