# Accessibility roles by platform

`glass_a11y_snapshot` normalizes every platform's accessibility vocabulary into one set of
roles. This page states which roles each backend can produce.

Each node also carries the backend's own token — the AT-SPI role, the UIA control type, the AX
role string, the Android widget class, the iOS role string. When glass has no mapping for a
token the role is `Other` and the outline shows the token in brackets — `Other(AXDisclosureTriangle)`
— so nothing is hidden; a `role:` selector just will not match it.

A cell reads:

- `yes` — the backend produces this role.
- `n/a` — no token in the vocabulary this backend reads carries the role, so no mapping could
  reach it. That covers a concept the platform does not have and one it draws on screen without
  marking; the reason says which, and what the control reports instead.
- `gap` — the vocabulary does carry it and glass does not map it yet. The reason names what is
  there and unread.

Each cell is decided by putting the control on screen and reading the tree back, not from what a
platform's API reference implies exists — a role is `n/a` only where the control was watched to
arrive carrying no token for it. Two example apps hold those controls, one per question, so the
reading is repeatable: [`examples/android-role-fixture/`](../../examples/android-role-fixture/)
and [`examples/ios-role-fixture/`](../../examples/ios-role-fixture/).

On Android, both readers report a `Window` root: the `uiautomator` reader wraps its dump in a
window root sized to the app window, and the accessibility-service reader labels the active
window's own root node. The outline does not name that node's widget class — the root has a role
now, and the outline only names the token of an element that has none.

<!-- BEGIN GENERATED: role-support -->
| Role | Linux (AT-SPI) | Windows (UIA) | macOS (AX) | Android | iOS |
|---|---|---|---|---|---|
| `Application` | yes | n/a | gap | n/a | yes |
| `Window` | yes | yes | yes | yes | yes |
| `Dialog` | yes | gap | gap | n/a | n/a |
| `Group` | yes | yes | yes | yes | yes |
| `Button` | yes | yes | yes | yes | yes |
| `ToggleButton` | yes | yes | gap | yes | yes |
| `RadioButton` | yes | yes | yes | yes | n/a |
| `CheckBox` | yes | yes | yes | yes | yes |
| `MenuBar` | yes | yes | yes | n/a | n/a |
| `Menu` | yes | yes | yes | n/a | n/a |
| `MenuItem` | yes | yes | yes | n/a | n/a |
| `Label` | yes | yes | yes | yes | yes |
| `TextField` | yes | yes | yes | yes | yes |
| `TextArea` | yes | yes | yes | n/a | yes |
| `ComboBox` | yes | yes | yes | yes | n/a |
| `List` | yes | yes | yes | yes | n/a |
| `ListItem` | yes | yes | yes | gap | n/a |
| `Table` | yes | yes | gap | gap | n/a |
| `Cell` | yes | gap | yes | gap | yes |
| `Tree` | yes | yes | yes | n/a | n/a |
| `TreeItem` | yes | yes | yes | n/a | n/a |
| `TabList` | yes | yes | yes | gap | yes |
| `Tab` | yes | yes | gap | gap | n/a |
| `ScrollBar` | yes | yes | yes | n/a | n/a |
| `Slider` | yes | yes | yes | yes | yes |
| `SpinButton` | yes | yes | gap | gap | n/a |
| `ProgressBar` | yes | yes | yes | yes | n/a |
| `Image` | yes | yes | yes | yes | yes |
| `Link` | yes | yes | yes | n/a | yes |
| `Separator` | yes | yes | yes | n/a | n/a |
| `Toolbar` | yes | yes | yes | n/a | yes |
| `StatusBar` | yes | yes | n/a | n/a | n/a |
| `Heading` | yes | gap | yes | gap | yes |

### Why a cell is not `yes`

- `Application` / Windows (UIA) — n/a: UIA has no application control type; the app root is a Window
- `Application` / macOS (AX) — gap: AXApplication is the app element but is not mapped yet
- `Application` / Android — n/a: Android exposes windows, not an application element
- `Dialog` / Windows (UIA) — gap: UIA marks a dialog with the IsDialog property on a Window; the reader does not read it
- `Dialog` / macOS (AX) — gap: AXSheet, AXPopover and AXDrawer are not mapped yet
- `Dialog` / Android — n/a: an AlertDialog's panels report FrameLayout and LinearLayout, and Android has no dialog window type; nothing in the vocabulary marks a dialog
- `Dialog` / iOS — n/a: an alert exposes its title, message and buttons directly under the application element; no alert, sheet or popover token appears
- `ToggleButton` / macOS (AX) — gap: AppKit reports a switch as AXCheckBox with an AXSwitch or AXToggle subrole; AXCheckBox is outside the reader's subrole gate, and no probed app emitted either subrole
- `RadioButton` / iOS — n/a: UIKit has no radio control; a UISegmentedControl — the nearest equivalent — reports one AXTabGroup with no per-segment element
- `MenuBar` / Android — n/a: Android apps have no menu bar
- `MenuBar` / iOS — n/a: iOS apps have no menu bar
- `Menu` / Android — n/a: a popup menu reports android.widget.ListView; no menu token appears anywhere in the tree it opens
- `Menu` / iOS — n/a: a UIMenu opens as an AXGroup of AXButtons alongside a Dismiss context menu button; no menu token appears
- `MenuItem` / Android — n/a: a menu entry reports its item view's layout class — a LinearLayout or RelativeLayout holding a TextView title
- `MenuItem` / iOS — n/a: a menu entry reports AXButton, the same token as any other button
- `TextArea` / Android — n/a: Android uses one editable class for single- and multi-line text
- `ComboBox` / iOS — n/a: a UIPickerView reports AXSlider and a SwiftUI Picker reports a heading with buttons; no picker token appears
- `List` / iOS — n/a: a UITableView and a SwiftUI List both report AXGroup, and their rows report AXStaticText; no list token appears
- `ListItem` / Android — gap: a list child reports its own widget class; the row and column live in AccessibilityNodeInfo's CollectionItemInfo, which the uiautomator dump has no attribute for and the service protocol does not send
- `ListItem` / iOS — n/a: table and collection rows report AXStaticText, including a cell explicitly exposed as its own accessibility element
- `Table` / macOS (AX) — gap: mac lists report AXOutline rather than AXTable; AXOutline maps to Tree
- `Table` / Android — gap: android.widget.TableLayout and TableRow do arrive — the container rule folds any leaf ending in Layout into Group — and GridView maps to List; CollectionInfo's row and column counts reach neither reader
- `Table` / iOS — n/a: a table view reports AXGroup like any other container; no table token appears
- `Cell` / Windows (UIA) — gap: UIA's DataItem control type would carry this, but the data grids probed expose their rows as TreeItem
- `Cell` / Android — gap: no cell class exists — a cell is whatever view the app put in the row; the row and column index live in CollectionItemInfo, which reaches neither reader
- `Tree` / Android — n/a: Android has no tree widget
- `Tree` / iOS — n/a: UIKit has no outline view
- `TreeItem` / Android — n/a: Android has no tree widget
- `TreeItem` / iOS — n/a: UIKit has no outline view
- `TabList` / Android — gap: android.widget.TabWidget and TabHost do arrive as tokens and are not mapped yet; a Material tab strip reports a plain layout class instead
- `Tab` / macOS (AX) — gap: AppKit reports tab items as AXRadioButton inside the tab group; the containing role is not used to disambiguate yet
- `Tab` / Android — gap: a tab reports a plain layout class with selected=true — under a TabWidget parent in the framework widget, named only by the view id in a Material tab strip — so nothing in the class name marks it as a tab
- `Tab` / iOS — n/a: a tab bar reports AXGroup and its items are not exposed as elements at all; a segmented control reports one AXTabGroup with no per-segment element
- `ScrollBar` / Android — n/a: Android scrollbars are drawn, not exposed as nodes
- `ScrollBar` / iOS — n/a: UIKit scroll indicators are not accessibility elements
- `SpinButton` / macOS (AX) — gap: AXIncrementor is not mapped yet
- `SpinButton` / Android — gap: android.widget.NumberPicker does arrive as a token and is not mapped yet
- `SpinButton` / iOS — n/a: a UIStepper decomposes into two AXButtons labelled Decrement and Increment; no stepper token appears
- `ProgressBar` / iOS — n/a: a UIProgressView reports AXGenericElement carrying its percentage as the value; no progress token appears
- `Link` / Android — n/a: Android links are spans inside a text view, not separate nodes
- `Separator` / Android — n/a: Android dividers are drawn, not exposed as nodes
- `Separator` / iOS — n/a: UIKit exposes no separator element
- `Toolbar` / Android — n/a: android.widget.Toolbar reports android.view.ViewGroup, so no toolbar token reaches either reader
- `StatusBar` / macOS (AX) — n/a: AppKit exposes status items as menu-bar items
- `StatusBar` / Android — n/a: the system status bar is outside the app tree
- `StatusBar` / iOS — n/a: the system status bar is outside the app tree
- `Heading` / Windows (UIA) — gap: UIA's Header and HeaderItem are grid column headers, not document headings; the normalized set has no column-header role
- `Heading` / Android — gap: neither reader exposes heading semantics: the uiautomator dump has no heading attribute, and the service protocol does not carry isHeading
<!-- END GENERATED: role-support -->
