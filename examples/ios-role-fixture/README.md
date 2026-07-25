# ios-role-fixture

One screen of stock UIKit controls (plus a SwiftUI screen), for deciding what iOS's accessibility
vocabulary can express. Each control answers one question: what AX role string does the Simulator
report for it? That answer is what a cell in
[docs/reference/a11y-roles.md](../../docs/reference/a11y-roles.md) records — a role is marked
`n/a` there only where a control was watched to arrive carrying no token for it.

| Control | Reports (iOS 26.5 Simulator) |
|---|---|
| `UISegmentedControl` | one `AXTabGroup`, no per-segment element |
| `UIStepper` | two `AXButton`s, `Decrement` and `Increment` |
| `UIProgressView` | `AXGenericElement`, percentage in the value |
| `UIPickerView` | `AXSlider` |
| `UITableView` | `AXGroup`; rows `AXStaticText`, including one exposed as its own element |
| `UICollectionView` | `AXGroup`; cells `AXStaticText` |
| `UIAlertController` (button) | title, message and buttons loose under `AXApplication` |
| `UIMenu` (button) | `AXGroup` of `AXButton`s plus a `Dismiss context menu` button |
| `UITabBar` | `AXGroup` labelled `Tab Bar`; its items are not elements at all |
| SwiftUI `List` | `AXGroup`; rows `AXStaticText`, section header `AXHeading` |

## Build and install

Requires the full Xcode and an iOS Simulator runtime (macOS only):

```bash
./build.sh                                                    # → build/RoleFixture.app
xcrun simctl install booted build/RoleFixture.app
xcrun simctl launch booted tech.fixedwidth.glassrolefixture
```

A synthetic tap on a tab bar item does not switch tabs — the items are not accessibility elements
— so the screen is chosen at launch:

```bash
xcrun simctl launch booted tech.fixedwidth.glassrolefixture --tab collection   # or: swiftui
```

## Read the tree

With `idb_companion` running against the Simulator:

```bash
idb ui describe-all --json
```

Through glass's own probe, which prints a role histogram per app:

```bash
GLASS_A11Y_PROBE_APPS="$PWD/build/RoleFixture.app" \
  cargo test -p glass-ios --test role_probe -- --ignored --nocapture
```
