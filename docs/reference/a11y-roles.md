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

On Android, both readers report a `Window` root: the `uiautomator` reader wraps its dump in a
window root sized to the app window, and the accessibility-service reader labels the active
window's own root node. The outline does not name that node's widget class — the root has a role
now, and the outline only names the token of an element that has none.

<!-- BEGIN GENERATED: role-support -->
| Role | Linux (AT-SPI) | Windows (UIA) | macOS (AX) | Android | iOS |
|---|---|---|---|---|---|
| `Application` | yes | n/a | gap | n/a | yes |
| `Window` | yes | yes | yes | yes | yes |
| `Dialog` | yes | gap | gap | gap | gap |
| `Group` | yes | yes | yes | yes | yes |
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
| `Tree` | yes | yes | yes | n/a | n/a |
| `TreeItem` | yes | yes | yes | n/a | n/a |
| `TabList` | yes | yes | yes | gap | yes |
| `Tab` | yes | yes | gap | gap | gap |
| `ScrollBar` | yes | yes | yes | n/a | n/a |
| `Slider` | yes | yes | yes | yes | yes |
| `SpinButton` | yes | yes | gap | gap | gap |
| `ProgressBar` | yes | yes | yes | yes | gap |
| `Image` | yes | yes | yes | yes | yes |
| `Link` | yes | yes | yes | n/a | yes |
| `Separator` | yes | yes | yes | n/a | n/a |
| `Toolbar` | yes | yes | yes | gap | yes |
| `StatusBar` | yes | yes | n/a | n/a | n/a |
| `Heading` | yes | gap | yes | gap | yes |

### Why a cell is not `yes`

- `Application` / Windows (UIA) — n/a: UIA has no application control type; the app root is a Window
- `Application` / macOS (AX) — gap: AXApplication is the app element but is not mapped yet
- `Application` / Android — n/a: Android exposes windows, not an application element
- `Dialog` / Windows (UIA) — gap: UIA marks a dialog with the IsDialog property on a Window; the reader does not read it
- `Dialog` / macOS (AX) — gap: AXSheet, AXPopover and AXDrawer are not mapped yet
- `Dialog` / Android — gap: dialog windows arrive as a generic layout class
- `Dialog` / iOS — gap: an alert exposes its buttons and text directly under the application element; no alert, sheet or popover token appears
- `ToggleButton` / macOS (AX) — gap: AppKit reports a switch as AXCheckBox with an AXSwitch or AXToggle subrole; AXCheckBox is outside the reader's subrole gate, and no probed app emitted either subrole
- `RadioButton` / iOS — gap: no radio token is mapped yet
- `MenuBar` / Android — n/a: Android apps have no menu bar
- `MenuBar` / iOS — n/a: iOS apps have no menu bar
- `Menu` / Android — gap: popup menus arrive as list or layout classes
- `Menu` / iOS — gap: context-menu tokens are not mapped yet
- `MenuItem` / Android — gap: menu entries arrive as their own widget class
- `MenuItem` / iOS — gap: menu-item tokens are not mapped yet
- `TextArea` / Android — n/a: Android uses one editable class for single- and multi-line text
- `ComboBox` / iOS — gap: picker tokens are not mapped yet
- `List` / iOS — gap: collections arrive as AXGroup, which maps to Group; no list token appears
- `ListItem` / Android — gap: list children report their own widget class, not a list-item role
- `ListItem` / iOS — gap: a collection's children report their own element role, not a list-item role
- `Table` / macOS (AX) — gap: mac lists report AXOutline rather than AXTable; AXOutline maps to Tree
- `Table` / Android — gap: table and grid classes collapse into List
- `Table` / iOS — gap: collections arrive as AXGroup, which maps to Group; no table token appears
- `Cell` / Windows (UIA) — gap: UIA's DataItem control type would carry this, but the data grids probed expose their rows as TreeItem
- `Cell` / Android — gap: no cell class is mapped yet
- `Tree` / Android — n/a: Android has no tree widget
- `Tree` / iOS — n/a: UIKit has no outline view
- `TreeItem` / Android — n/a: Android has no tree widget
- `TreeItem` / iOS — n/a: UIKit has no outline view
- `TabList` / Android — gap: a tab container reports a plain layout class; the tab role lives in the view's id and selected state, not in the class name
- `Tab` / macOS (AX) — gap: AppKit reports tab items as AXRadioButton inside the tab group; the containing role is not used to disambiguate yet
- `Tab` / Android — gap: a tab reports a plain layout class with selected=true; nothing in the class name marks it as a tab
- `Tab` / iOS — gap: the tab bars seen in probing arrived as AXGroup, named by the element identifier rather than the role, and no tab-item token appeared
- `ScrollBar` / Android — n/a: Android scrollbars are drawn, not exposed as nodes
- `ScrollBar` / iOS — n/a: UIKit scroll indicators are not accessibility elements
- `SpinButton` / macOS (AX) — gap: AXIncrementor is not mapped yet
- `SpinButton` / Android — gap: the number-picker class is not mapped yet
- `SpinButton` / iOS — gap: the stepper token is not mapped yet
- `ProgressBar` / iOS — gap: no progress token is mapped yet
- `Link` / Android — n/a: Android links are spans inside a text view, not separate nodes
- `Separator` / Android — n/a: Android dividers are drawn, not exposed as nodes
- `Separator` / iOS — n/a: UIKit exposes no separator element
- `Toolbar` / Android — gap: the toolbar class is not mapped yet
- `StatusBar` / macOS (AX) — n/a: AppKit exposes status items as menu-bar items
- `StatusBar` / Android — n/a: the system status bar is outside the app tree
- `StatusBar` / iOS — n/a: the system status bar is outside the app tree
- `Heading` / Windows (UIA) — gap: UIA's Header and HeaderItem are grid column headers, not document headings; the normalized set has no column-header role
- `Heading` / Android — gap: neither reader exposes heading semantics: the uiautomator dump has no heading attribute, and the service protocol does not carry isHeading
<!-- END GENERATED: role-support -->
