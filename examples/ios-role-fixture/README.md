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
xcrun simctl launch booted tech.fixedwidth.glassrolefixture --tab=collection    # or: swiftui
SIMCTL_CHILD_ROLE_FIXTURE_TAB=swiftui xcrun simctl launch booted tech.fixedwidth.glassrolefixture
```

The value must be joined with `=`. `simctl launch` forwards `--tab=swiftui` but drops the value of
a separated `--tab swiftui`, which would otherwise have launched the Controls screen while looking
like it had worked; the app treats that, and an unrecognized name, as fatal. The collection and
SwiftUI screens also name themselves in the tree — `screen-collection` and `screen-swiftui` appear
as element identifiers — and the Controls screen is headed `Controls`.

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

The probe launches the app without arguments, so it reads the Controls screen; set
`SIMCTL_CHILD_ROLE_FIXTURE_TAB` in its environment to probe another, or pass `--tab=<name>` after
the `.app` path in `GLASS_A11Y_PROBE_APPS`-driven runs that build their own `AppSpec`.
