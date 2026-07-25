# Accessibility roles by platform

`glass_a11y_snapshot` normalizes every platform's accessibility vocabulary into one set of
roles. This page states which roles each backend can produce.

Each node also carries the backend's own token — the AT-SPI role, the UIA control type, the AX
role string, the Android widget class, the iOS role string. When glass has no mapping for a
token the role is `Other` and the outline shows the token in brackets — `Other(AXDisclosureTriangle)`
— so nothing is hidden; a `role:` selector just will not match it.

A cell reads:

- `yes` — the backend produces this role.
- `n/a` — the platform has no counterpart. The reason says why: what the platform does instead,
  or that the concept simply does not exist there.
- `gap` — the platform has a counterpart glass does not map yet.

On Android, `Window` holds for the `uiautomator` reader, which wraps the dump in a window root
sized to the app window. The on-device accessibility-service reader instead roots the tree at the
device's own node, which is classified like any other node from its widget class.

<!-- BEGIN GENERATED: role-support -->
| Role | Linux (AT-SPI) | Windows (UIA) | macOS (AX) | Android | iOS |
|---|---|---|---|---|---|
| `Application` | yes | n/a | gap | n/a | yes |
| `Window` | yes | yes | yes | yes | yes |
| `Dialog` | yes | gap | gap | gap | gap |
| `Group` | yes | yes | yes | yes | gap |
| `Button` | yes | yes | yes | yes | yes |
| `ToggleButton` | yes | yes | gap | yes | yes |
| `RadioButton` | yes | yes | yes | yes | gap |
| `CheckBox` | yes | yes | yes | yes | yes |
| `MenuBar` | yes | yes | yes | n/a | n/a |
| `Menu` | yes | yes | yes | gap | gap |
| `MenuItem` | yes | yes | yes | gap | gap |
| `Label` | yes | yes | yes | yes | yes |
| `TextField` | yes | yes | yes | yes | yes |
| `TextArea` | yes | yes | yes | n/a | yes |
| `ComboBox` | yes | yes | yes | yes | gap |
| `List` | yes | yes | yes | yes | gap |
| `ListItem` | yes | yes | yes | gap | gap |
| `Table` | yes | yes | gap | gap | gap |
| `Cell` | yes | gap | yes | gap | yes |
| `Tree` | yes | yes | gap | n/a | n/a |
| `TreeItem` | yes | yes | gap | n/a | n/a |
| `TabList` | yes | yes | yes | gap | yes |
| `Tab` | yes | yes | gap | gap | gap |
| `ScrollBar` | yes | yes | yes | n/a | n/a |
| `Slider` | yes | yes | yes | yes | yes |
| `SpinButton` | yes | yes | gap | gap | gap |
| `ProgressBar` | yes | yes | yes | yes | gap |
| `Image` | yes | yes | yes | yes | yes |
| `Link` | yes | yes | yes | n/a | yes |
| `Separator` | yes | yes | gap | n/a | n/a |
| `Toolbar` | yes | yes | yes | gap | yes |
| `StatusBar` | yes | yes | n/a | n/a | n/a |
| `Heading` | yes | yes | gap | gap | gap |

### Why a cell is not `yes`

- `Application` / Windows (UIA) — n/a: UIA has no application control type; the app root is a Window
- `Application` / macOS (AX) — gap: AXApplication is the app element but is not mapped yet
- `Application` / Android — n/a: Android exposes windows, not an application element
- `Dialog` / Windows (UIA) — gap: UIA marks a dialog with the IsDialog property on a Window; the reader does not read it
- `Dialog` / macOS (AX) — gap: AXSheet, AXPopover and AXDrawer are not mapped yet
- `Dialog` / Android — gap: dialog windows arrive as a generic layout class
- `Dialog` / iOS — gap: alert and action-sheet tokens are not mapped yet
- `Group` / iOS — gap: no container token is mapped yet
- `ToggleButton` / macOS (AX) — gap: AppKit reports a switch as AXCheckBox with an AXSwitch or AXToggle subrole; subroles are not read yet
- `RadioButton` / iOS — gap: no radio token is mapped yet
- `MenuBar` / Android — n/a: Android apps have no menu bar
- `MenuBar` / iOS — n/a: iOS apps have no menu bar
- `Menu` / Android — gap: popup menus arrive as list or layout classes
- `Menu` / iOS — gap: context-menu tokens are not mapped yet
- `MenuItem` / Android — gap: menu entries arrive as their own widget class
- `MenuItem` / iOS — gap: menu-item tokens are not mapped yet
- `TextArea` / Android — n/a: Android uses one editable class for single- and multi-line text
- `ComboBox` / iOS — gap: picker tokens are not mapped yet
- `List` / iOS — gap: no list token is mapped yet
- `ListItem` / Android — gap: list children report their own widget class, not a list-item role
- `ListItem` / iOS — gap: no list-item token is mapped yet
- `Table` / macOS (AX) — gap: mac lists report AXOutline rather than AXTable; AXOutline maps to Tree
- `Table` / Android — gap: table and grid classes collapse into List
- `Table` / iOS — gap: no table token is mapped yet
- `Cell` / Windows (UIA) — gap: UIA's DataItem control type would carry this, but the data grids probed expose their rows as TreeItem
- `Cell` / Android — gap: no cell class is mapped yet
- `Tree` / macOS (AX) — gap: AXOutline is not mapped yet
- `Tree` / Android — n/a: Android has no tree widget
- `Tree` / iOS — n/a: UIKit has no outline view
- `TreeItem` / macOS (AX) — gap: the outline-row subrole is not read yet
- `TreeItem` / Android — n/a: Android has no tree widget
- `TreeItem` / iOS — n/a: UIKit has no outline view
- `TabList` / Android — gap: tab-container classes are not mapped yet
- `Tab` / macOS (AX) — gap: AppKit reports tab items as AXRadioButton inside the tab group; the containing role is not used to disambiguate yet
- `Tab` / Android — gap: tab children are not mapped yet
- `Tab` / iOS — gap: tab-item tokens are not mapped yet
- `ScrollBar` / Android — n/a: Android scrollbars are drawn, not exposed as nodes
- `ScrollBar` / iOS — n/a: UIKit scroll indicators are not accessibility elements
- `SpinButton` / macOS (AX) — gap: AXIncrementor is not mapped yet
- `SpinButton` / Android — gap: the number-picker class is not mapped yet
- `SpinButton` / iOS — gap: the stepper token is not mapped yet
- `ProgressBar` / iOS — gap: no progress token is mapped yet
- `Link` / Android — n/a: Android links are spans inside a text view, not separate nodes
- `Separator` / macOS (AX) — gap: AXSplitter is not mapped yet
- `Separator` / Android — n/a: Android dividers are drawn, not exposed as nodes
- `Separator` / iOS — n/a: UIKit exposes no separator element
- `Toolbar` / Android — gap: the toolbar class is not mapped yet
- `StatusBar` / macOS (AX) — n/a: AppKit exposes status items as menu-bar items
- `StatusBar` / Android — n/a: the system status bar is outside the app tree
- `StatusBar` / iOS — n/a: the system status bar is outside the app tree
- `Heading` / macOS (AX) — gap: AXHeading is not mapped yet
- `Heading` / Android — gap: heading semantics are not mapped yet
- `Heading` / iOS — gap: the header token is not mapped yet
<!-- END GENERATED: role-support -->
