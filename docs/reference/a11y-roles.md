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
- `absent` — the platform has no such control to expose. iOS has no radio button, Android no tree
  widget; there is nothing to read.
- `unmarked` — the control exists and was put on screen, and no reading found a token carrying its
  role: it is drawn but not exposed, or folded into a token meaning something else. The reason
  names what it reported instead.
- `elsewhere` — the control exists and is exposed, but outside what glass walks. The system status
  bar belongs to the OS shell, not to the app whose tree is being read.

**`unmarked` is a reading, not a proof.** It says the standard controls were watched on one OS
version through one reader and nothing carried the role — not that nothing can. Two of the three
vocabularies are open, so the question cannot be closed by any amount of reading: an Android app
sets its own `AccessibilityNodeInfo` class name and may report anything, and iOS role strings come
from the Simulator's translator. Only UIA names a closed, documented set of control types. Treat
`unmarked` as "do not expect this role here", not as "this role is impossible here" — and if an
app you drive does expose one, that is a cell to correct, not a contradiction.

Where there is a control to look at, the cell is decided by putting it on screen and reading the
tree back rather than from what a platform's API reference implies. Two example apps hold those
controls, one per question, so the reading is repeatable:
[`examples/android-role-fixture/`](../../examples/android-role-fixture/) and
[`examples/ios-role-fixture/`](../../examples/ios-role-fixture/). A `yes` is the one state that is
map-backed rather than reading-backed: it says a token is mapped, not that an app was seen to
emit it.

Reasons are readings, not guarantees: each says what a control was seen to report, and a platform
is free to change that. When a cell was last read is answered by `git log` over
`crates/glass-core/src/role_support.rs`, and what it was read against by re-running the fixture,
which prints its platform version. What the iOS column can say is also bounded by what `idb`'s
accessibility tree exposes, which may be narrower than what UIKit publishes to VoiceOver.

On Android, both readers report a `Window` root: the `uiautomator` reader wraps its dump in a
window root sized to the app window, and the accessibility-service reader labels the active
window's own root node. The outline does not name that node's widget class — the root has a role
now, and the outline only names the token of an element that has none.

<!-- BEGIN GENERATED: role-support -->
| Role | Linux (AT-SPI) | Windows (UIA) | macOS (AX) | Android | iOS |
|---|---|---|---|---|---|
| `Application` | yes | unmarked | gap | absent | yes |
| `Window` | yes | yes | yes | yes | yes |
| `Dialog` | yes | gap | gap | unmarked | unmarked |
| `Group` | yes | yes | yes | yes | yes |
| `Button` | yes | yes | yes | yes | yes |
| `ToggleButton` | yes | yes | gap | yes | yes |
| `RadioButton` | yes | yes | yes | yes | absent |
| `CheckBox` | yes | yes | yes | yes | yes |
| `MenuBar` | yes | yes | yes | absent | absent |
| `Menu` | yes | yes | yes | unmarked | unmarked |
| `MenuItem` | yes | yes | yes | unmarked | unmarked |
| `Label` | yes | yes | yes | yes | yes |
| `TextField` | yes | yes | yes | yes | yes |
| `TextArea` | yes | yes | yes | unmarked | yes |
| `ComboBox` | yes | yes | yes | yes | gap |
| `List` | yes | yes | yes | yes | unmarked |
| `ListItem` | yes | yes | yes | gap | unmarked |
| `Table` | yes | yes | gap | gap | unmarked |
| `Cell` | yes | gap | yes | gap | yes |
| `Tree` | yes | yes | yes | absent | absent |
| `TreeItem` | yes | yes | yes | absent | absent |
| `TabList` | yes | yes | yes | gap | yes |
| `Tab` | yes | yes | gap | gap | unmarked |
| `ScrollBar` | yes | yes | yes | unmarked | unmarked |
| `Slider` | yes | yes | yes | yes | yes |
| `SpinButton` | yes | yes | gap | gap | unmarked |
| `ProgressBar` | yes | yes | yes | yes | unmarked |
| `Image` | yes | yes | yes | yes | yes |
| `Link` | yes | yes | yes | unmarked | yes |
| `Separator` | yes | yes | yes | unmarked | unmarked |
| `Toolbar` | yes | yes | yes | unmarked | yes |
| `StatusBar` | yes | yes | unmarked | elsewhere | elsewhere |
| `Heading` | yes | gap | yes | gap | yes |

### Why a cell is not `yes`

- `Application` / Windows (UIA) — unmarked (reports `Window` instead): UIA has no application control type; the app root is a Window
- `Application` / macOS (AX) — gap (`AXApplication` arrives unmapped): AXApplication is the app element but is not mapped yet
- `Application` / Android — absent: Android exposes windows, not an application element
- `Dialog` / Windows (UIA) — gap: UIA marks a dialog with the IsDialog property on a Window; the reader does not read it
- `Dialog` / macOS (AX) — gap (`AXSheet` arrives unmapped): AXSheet, AXPopover and AXDrawer are not mapped yet
- `Dialog` / Android — unmarked (reports `android.widget.FrameLayout` instead): a framework AlertDialog's panels report FrameLayout and LinearLayout under android:id/parentPanel, and AccessibilityWindowInfo's window types carry no dialog kind for a reader to fall back on
- `Dialog` / iOS — unmarked: an alert and an action sheet each expose their title, message and buttons directly under the application element, with no container token between
- `ToggleButton` / macOS (AX) — gap (`AXCheckBox` arrives unmapped): AppKit reports a switch as AXCheckBox with an AXSwitch or AXToggle subrole; AXCheckBox is outside the reader's subrole gate, and no probed app emitted either subrole
- `RadioButton` / iOS — absent: UIKit has no radio control; a UISegmentedControl — the nearest equivalent — reports one AXTabGroup with no per-segment element
- `MenuBar` / Android — absent: Android apps have no menu bar
- `MenuBar` / iOS — absent: iOS apps have no menu bar
- `Menu` / Android — unmarked (reports `android.widget.ListView` instead): a popup menu reports android.widget.ListView; no menu token appears anywhere in the tree it opens
- `Menu` / iOS — unmarked (reports `AXGroup` instead): a button's pull-down UIMenu opens as an AXGroup of AXButtons alongside a Dismiss context menu button; no menu token appears
- `MenuItem` / Android — unmarked (reports `android.widget.LinearLayout` instead): a menu entry reports its item view's layout class — a LinearLayout or RelativeLayout holding a TextView title
- `MenuItem` / iOS — unmarked (reports `AXButton` instead): a menu entry reports AXButton, the same token as any other button
- `TextArea` / Android — unmarked (reports `android.widget.EditText` instead): Android uses one editable class for single- and multi-line text
- `ComboBox` / iOS — gap (`AXPopUpButton` arrives unmapped): a menu-style SwiftUI Picker reports AXPopUpButton, which is not mapped yet; the inline style reports a heading with buttons, and a UIPickerView reports AXSlider
- `List` / iOS — unmarked (reports `AXGroup` instead): a UITableView and a SwiftUI List both report AXGroup, and their rows report AXStaticText; no list token appears
- `ListItem` / Android — gap: AccessibilityNodeInfo's CollectionItemInfo marks a list child, and neither reader carries it: the uiautomator dump has no such attribute and the service reader parses only class, text, description and bounds. The child's own widget class is all that arrives
- `ListItem` / iOS — unmarked (reports `AXStaticText` instead): table and collection rows report AXStaticText, including a cell explicitly exposed as its own accessibility element
- `Table` / macOS (AX) — gap: mac lists report AXOutline rather than AXTable; AXOutline maps to Tree
- `Table` / Android — gap (`android.widget.TableLayout` arrives unmapped): android.widget.TableLayout and TableRow both arrive as tokens: TableLayout folds into Group via the container rule, TableRow lands in Other, and GridView maps to List
- `Table` / iOS — unmarked (reports `AXGroup` instead): a table view reports AXGroup like any other container; no table token appears
- `Cell` / Windows (UIA) — gap (`DataItem` arrives unmapped): UIA's DataItem control type would carry this and is not mapped: the data grids probed expose their rows as TreeItem instead, though Edge does report an HTML table's header cells as DataItem
- `Cell` / Android — gap: CollectionItemInfo holds the row and column index, and neither reader carries it: the uiautomator dump has no such attribute and the service reader parses only class, text, description and bounds. No cell class exists to fall back on — a cell is whatever view the app put in the row
- `Tree` / Android — absent: Android has no tree widget
- `Tree` / iOS — absent: UIKit has no outline view
- `TreeItem` / Android — absent: Android has no tree widget
- `TreeItem` / iOS — absent: UIKit has no outline view
- `TabList` / Android — gap (`android.widget.TabWidget` arrives unmapped): android.widget.TabWidget and TabHost do arrive as tokens and are not mapped yet; the Material tab strips seen in stock apps report a plain layout class instead, naming their tabs only by content description
- `Tab` / macOS (AX) — gap (`AXRadioButton` arrives unmapped): AppKit reports tab items as AXRadioButton inside the tab group; the containing role is not used to disambiguate yet
- `Tab` / Android — gap: a framework tab arrives as a LinearLayout with selected=true under a TabWidget parent, which together identify it; the class name alone does not, and a Material tab strip carries the role in the view id instead
- `Tab` / iOS — unmarked (reports `AXGroup` instead): a tab bar reports AXGroup and no per-item element appears in idb's describe; a segmented control reports one AXTabGroup, also with no per-segment element
- `ScrollBar` / Android — unmarked: Android scrollbars are drawn, not exposed as nodes
- `ScrollBar` / iOS — unmarked: UIKit scroll indicators are not accessibility elements
- `SpinButton` / macOS (AX) — gap (`AXIncrementor` arrives unmapped): AXIncrementor is not mapped yet
- `SpinButton` / Android — gap (`android.widget.NumberPicker` arrives unmapped): android.widget.NumberPicker does arrive as a token and is not mapped yet
- `SpinButton` / iOS — unmarked (reports `AXButton` instead): a UIStepper decomposes into two AXButtons labelled Decrement and Increment; no stepper token appears
- `ProgressBar` / iOS — unmarked (reports `AXGenericElement` instead): a UIProgressView reports AXGenericElement carrying its percentage as the value; no progress token appears
- `Link` / Android — unmarked: Android links are spans inside a text view, not separate nodes
- `Separator` / Android — unmarked: Android dividers are drawn, not exposed as nodes
- `Separator` / iOS — unmarked: UIKit exposes no separator element
- `Toolbar` / Android — unmarked (reports `android.view.ViewGroup` instead): android.widget.Toolbar was watched to report android.view.ViewGroup — a subclass inherits the accessibility class name of the framework class it extends, and the AppCompat and Material toolbars override neither, so all three arrive as ViewGroup
- `StatusBar` / macOS (AX) — unmarked: AppKit exposes status items as menu-bar items
- `StatusBar` / Android — elsewhere: the system status bar is outside the app tree
- `StatusBar` / iOS — elsewhere: the system status bar is outside the app tree
- `Heading` / Windows (UIA) — gap: UIA marks a heading with the HeadingLevel property — an h1 arrives as Text carrying level 80051 — and the reader maps by control type alone, so it never sees it. Header and HeaderItem are a grid's column headers, a different concept the normalized set has no role for
- `Heading` / Android — gap: AccessibilityNodeInfo's isHeading marks a heading, and neither reader carries it: the uiautomator dump has no such attribute and the service reader parses only class, text, description and bounds
<!-- END GENERATED: role-support -->
