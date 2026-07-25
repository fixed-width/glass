# Accessibility roles by platform

`glass_a11y_snapshot` normalizes every platform's accessibility vocabulary into one set of
roles. This page states which roles each backend can produce.

Each node also carries the backend's own token — the AT-SPI role, the UIA control type, the AX
role string, the Android widget class, the iOS role string. When glass has no mapping for a
token the role is `Other` and the outline shows the token in brackets — `Other(AXDisclosureTriangle)`
— so nothing is hidden; a `role:` selector just will not match it.

A cell reads:

- `yes` — glass maps a native token to this role.
- `gap` — closing it is glass's own work. The platform carries the role somewhere glass reads or
  could read — a token that arrives unmapped, a documented field the reader ignores, a protocol
  its own companion could send — and the reason names what is there and unread.
- `n/a` — closing it would take a platform change. Either the platform has no such control, or it
  draws one its accessibility layer never marks; the reason says which, and what the control
  reports instead.

Where there is a control to look at, `gap` and `n/a` are decided by putting it on screen and
reading the tree back rather than from what a platform's API reference implies. Two example apps
hold those controls, one per question, so the reading is repeatable:
[`examples/android-role-fixture/`](../../examples/android-role-fixture/) and
[`examples/ios-role-fixture/`](../../examples/ios-role-fixture/). Cells for a control a platform
simply does not have — a radio button on iOS, a tree on Android — rest on its vocabulary instead
and say so. A `yes` is map-backed, not reading-backed: it says a token is mapped, not that an app
was seen to emit it.

Reasons are readings, and readings age. The Android and iOS columns were last read on 2026-07-25,
against an API 34 emulator and an iOS 26.5 Simulator. What the iOS column can say is bounded by
what `idb`'s accessibility tree exposes, which may be narrower than what UIKit publishes to
VoiceOver.

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
| `ComboBox` | yes | yes | yes | yes | gap |
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
- `Dialog` / Android — n/a: a framework AlertDialog's panels report FrameLayout and LinearLayout under android:id/parentPanel, and AccessibilityWindowInfo's window types carry no dialog kind for a reader to fall back on
- `Dialog` / iOS — n/a: an alert and an action sheet each expose their title, message and buttons directly under the application element, with no container token between
- `ToggleButton` / macOS (AX) — gap: AppKit reports a switch as AXCheckBox with an AXSwitch or AXToggle subrole; AXCheckBox is outside the reader's subrole gate, and no probed app emitted either subrole
- `RadioButton` / iOS — n/a: UIKit has no radio control; a UISegmentedControl — the nearest equivalent — reports one AXTabGroup with no per-segment element
- `MenuBar` / Android — n/a: Android apps have no menu bar
- `MenuBar` / iOS — n/a: iOS apps have no menu bar
- `Menu` / Android — n/a: a popup menu reports android.widget.ListView; no menu token appears anywhere in the tree it opens
- `Menu` / iOS — n/a: a button's pull-down UIMenu opens as an AXGroup of AXButtons alongside a Dismiss context menu button; no menu token appears
- `MenuItem` / Android — n/a: a menu entry reports its item view's layout class — a LinearLayout or RelativeLayout holding a TextView title
- `MenuItem` / iOS — n/a: a menu entry reports AXButton, the same token as any other button
- `TextArea` / Android — n/a: Android uses one editable class for single- and multi-line text
- `ComboBox` / iOS — gap: a menu-style SwiftUI Picker reports AXPopUpButton, which is not mapped yet; the inline style reports a heading with buttons, and a UIPickerView reports AXSlider
- `List` / iOS — n/a: a UITableView and a SwiftUI List both report AXGroup, and their rows report AXStaticText; no list token appears
- `ListItem` / Android — gap: AccessibilityNodeInfo's CollectionItemInfo marks a list child, and neither reader carries it: the uiautomator dump has no such attribute and the service reader parses only class, text, description and bounds. The child's own widget class is all that arrives
- `ListItem` / iOS — n/a: table and collection rows report AXStaticText, including a cell explicitly exposed as its own accessibility element
- `Table` / macOS (AX) — gap: mac lists report AXOutline rather than AXTable; AXOutline maps to Tree
- `Table` / Android — gap: android.widget.TableLayout and TableRow both arrive as tokens: TableLayout folds into Group via the container rule, TableRow lands in Other, and GridView maps to List
- `Table` / iOS — n/a: a table view reports AXGroup like any other container; no table token appears
- `Cell` / Windows (UIA) — gap: UIA's DataItem control type would carry this, but the data grids probed expose their rows as TreeItem
- `Cell` / Android — gap: CollectionItemInfo holds the row and column index, and neither reader carries it: the uiautomator dump has no such attribute and the service reader parses only class, text, description and bounds. No cell class exists to fall back on — a cell is whatever view the app put in the row
- `Tree` / Android — n/a: Android has no tree widget
- `Tree` / iOS — n/a: UIKit has no outline view
- `TreeItem` / Android — n/a: Android has no tree widget
- `TreeItem` / iOS — n/a: UIKit has no outline view
- `TabList` / Android — gap: android.widget.TabWidget and TabHost do arrive as tokens and are not mapped yet; the Material tab strips seen in stock apps report a plain layout class instead, naming their tabs only by content description
- `Tab` / macOS (AX) — gap: AppKit reports tab items as AXRadioButton inside the tab group; the containing role is not used to disambiguate yet
- `Tab` / Android — gap: a framework tab arrives as a LinearLayout with selected=true under a TabWidget parent, which together identify it; the class name alone does not, and a Material tab strip carries the role in the view id instead
- `Tab` / iOS — n/a: a tab bar reports AXGroup and no per-item element appears in idb's describe; a segmented control reports one AXTabGroup, also with no per-segment element
- `ScrollBar` / Android — n/a: Android scrollbars are drawn, not exposed as nodes
- `ScrollBar` / iOS — n/a: UIKit scroll indicators are not accessibility elements
- `SpinButton` / macOS (AX) — gap: AXIncrementor is not mapped yet
- `SpinButton` / Android — gap: android.widget.NumberPicker does arrive as a token and is not mapped yet
- `SpinButton` / iOS — n/a: a UIStepper decomposes into two AXButtons labelled Decrement and Increment; no stepper token appears
- `ProgressBar` / iOS — n/a: a UIProgressView reports AXGenericElement carrying its percentage as the value; no progress token appears
- `Link` / Android — n/a: Android links are spans inside a text view, not separate nodes
- `Separator` / Android — n/a: Android dividers are drawn, not exposed as nodes
- `Separator` / iOS — n/a: UIKit exposes no separator element
- `Toolbar` / Android — n/a: android.widget.Toolbar was watched to report android.view.ViewGroup — a subclass inherits the accessibility class name of the framework class it extends, and the AppCompat and Material toolbars override neither, so all three arrive as ViewGroup
- `StatusBar` / macOS (AX) — n/a: AppKit exposes status items as menu-bar items
- `StatusBar` / Android — n/a: the system status bar is outside the app tree
- `StatusBar` / iOS — n/a: the system status bar is outside the app tree
- `Heading` / Windows (UIA) — gap: UIA's Header and HeaderItem are grid column headers, not document headings; the normalized set has no column-header role
- `Heading` / Android — gap: AccessibilityNodeInfo's isHeading marks a heading, and neither reader carries it: the uiautomator dump has no such attribute and the service reader parses only class, text, description and bounds
<!-- END GENERATED: role-support -->
