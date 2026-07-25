//! What accessibility role each backend can produce — the declared parity matrix.
//!
//! A backend maps native tokens (AT-SPI roles, UIA control types, AX role strings, Android
//! widget classes, iOS role strings) onto [`AxRole`]. Coverage differs per platform: for some
//! roles nothing in the backend's vocabulary carries them, others simply are not mapped yet.
//! This module states which is which, so the difference is a checked fact rather than folklore.
//!
//! Where a control exists to look at, which of the two a cell gets is decided by observation
//! rather than by what a platform's API reference implies: the control is put on screen and the
//! tree is read back. `examples/android-role-fixture/` and `examples/ios-role-fixture/` hold
//! those controls so a cell can be re-read. Cells for a control the platform simply does not
//! have — a radio button on iOS, a tree on Android — rest on the platform's own vocabulary
//! instead, and say so.
//!
//! Reasons are readings, not guarantees: each says what a control was seen to report, and a
//! platform is free to change that. When a cell was last read is `git log`'s to answer — a date
//! written here would be one more thing to update and the first thing to go stale — and what it
//! was read against is the fixture run's, which prints its platform version. What the iOS column
//! can say is also bounded by what `idb`'s `accessibility_info` exposes, which may be narrower
//! than what UIKit publishes to VoiceOver.
//!
//! Each backend crate has a unit test that walks its own column and asserts its token table
//! agrees with what is declared here, so a new mapping cannot land without updating the
//! matrix and vice versa. `docs/reference/a11y-roles.md` renders the same data.

use crate::accessibility::AxRole;

/// The accessibility backends glass ships. Index order matches the cell arrays in
/// [`ROLE_SUPPORT`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum AxBackend {
    /// AT-SPI, shared by X11 and Wayland.
    Linux,
    /// UI Automation.
    Windows,
    /// AXUIElement.
    MacOs,
    /// `uiautomator` dumps and the on-device AccessibilityService, which share one map.
    Android,
    /// The Simulator's tree, read through `idb`.
    Ios,
}

impl AxBackend {
    /// Every backend, in the cell order used by [`ROLE_SUPPORT`].
    pub const ALL: [AxBackend; 5] = [
        AxBackend::Linux,
        AxBackend::Windows,
        AxBackend::MacOs,
        AxBackend::Android,
        AxBackend::Ios,
    ];

    /// Compile-time guard for [`AxBackend::ALL`] — never called, and exists only for its
    /// exhaustive match. Every completeness claim on the matrix is quantified over `ALL`, so a
    /// new backend that is not added there would silently weaken all of them; this match stops
    /// compiling until the new variant is listed here and in `ALL`.
    #[expect(dead_code, reason = "exists only for its exhaustive match")]
    fn all_is_exhaustive(backend: AxBackend) {
        match backend {
            AxBackend::Linux
            | AxBackend::Windows
            | AxBackend::MacOs
            | AxBackend::Android
            | AxBackend::Ios => {}
        }
    }

    /// Human-readable column heading, naming the native vocabulary.
    pub fn label(self) -> &'static str {
        match self {
            AxBackend::Linux => "Linux (AT-SPI)",
            AxBackend::Windows => "Windows (UIA)",
            AxBackend::MacOs => "macOS (AX)",
            AxBackend::Android => "Android",
            AxBackend::Ios => "iOS",
        }
    }
}

/// Whether a backend can produce a given [`AxRole`].
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RoleSupport {
    /// At least one native token maps to this role.
    Mapped,
    /// Closing this would take a platform change, not a glass change: either the platform has
    /// no such control, or it draws one the accessibility layer never marks. The reason says
    /// which, and what the control was observed to report instead.
    NotApplicable(&'static str),
    /// Closing this is glass's own work: the platform already carries the role somewhere glass
    /// reads or could read — a token that arrives unmapped, a documented field the reader
    /// ignores, a protocol its own companion could send. The reason names what is there and
    /// unread.
    Gap(&'static str),
}

/// Role coverage per backend. Cells are ordered as [`AxBackend::ALL`], and the row type is sized
/// from it so a new backend is a compile error here rather than a silent column mismatch.
pub const ROLE_SUPPORT: &[(AxRole, [RoleSupport; AxBackend::ALL.len()])] = {
    use AxRole as R;
    use RoleSupport::{Gap, Mapped, NotApplicable};
    &[
        (
            R::Application,
            [
                Mapped,
                NotApplicable("UIA has no application control type; the app root is a Window"),
                Gap("AXApplication is the app element but is not mapped yet"),
                NotApplicable("Android exposes windows, not an application element"),
                Mapped,
            ],
        ),
        (R::Window, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::Dialog,
            [
                Mapped,
                Gap(
                    "UIA marks a dialog with the IsDialog property on a Window; the reader does \
                     not read it",
                ),
                Gap("AXSheet, AXPopover and AXDrawer are not mapped yet"),
                NotApplicable(
                    "a framework AlertDialog's panels report FrameLayout and LinearLayout under \
                     android:id/parentPanel, and AccessibilityWindowInfo's window types carry no \
                     dialog kind for a reader to fall back on",
                ),
                NotApplicable(
                    "an alert and an action sheet each expose their title, message and buttons \
                     directly under the application element, with no container token between",
                ),
            ],
        ),
        (R::Group, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (R::Button, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::ToggleButton,
            [
                Mapped,
                Mapped,
                Gap(
                    "AppKit reports a switch as AXCheckBox with an AXSwitch or AXToggle \
                     subrole; AXCheckBox is outside the reader's subrole gate, and no probed \
                     app emitted either subrole",
                ),
                Mapped,
                Mapped,
            ],
        ),
        (
            R::RadioButton,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                NotApplicable(
                    "UIKit has no radio control; a UISegmentedControl — the nearest equivalent \
                     — reports one AXTabGroup with no per-segment element",
                ),
            ],
        ),
        (R::CheckBox, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::MenuBar,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android apps have no menu bar"),
                NotApplicable("iOS apps have no menu bar"),
            ],
        ),
        (
            R::Menu,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable(
                    "a popup menu reports android.widget.ListView; no menu token appears \
                     anywhere in the tree it opens",
                ),
                NotApplicable(
                    "a button's pull-down UIMenu opens as an AXGroup of AXButtons alongside a \
                     Dismiss context menu button; no menu token appears",
                ),
            ],
        ),
        (
            R::MenuItem,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable(
                    "a menu entry reports its item view's layout class — a LinearLayout or \
                     RelativeLayout holding a TextView title",
                ),
                NotApplicable("a menu entry reports AXButton, the same token as any other button"),
            ],
        ),
        (R::Label, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (R::TextField, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::TextArea,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android uses one editable class for single- and multi-line text"),
                Mapped,
            ],
        ),
        (
            R::ComboBox,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                Gap(
                    "a menu-style SwiftUI Picker reports AXPopUpButton, which is not mapped yet; \
                     the inline style reports a heading with buttons, and a UIPickerView reports \
                     AXSlider",
                ),
            ],
        ),
        (
            R::List,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                NotApplicable(
                    "a UITableView and a SwiftUI List both report AXGroup, and their rows report \
                     AXStaticText; no list token appears",
                ),
            ],
        ),
        (
            R::ListItem,
            [
                Mapped,
                Mapped,
                Mapped,
                Gap(
                    "AccessibilityNodeInfo's CollectionItemInfo marks a list child, and neither \
                     reader carries it: the uiautomator dump has no such attribute and the \
                     service reader parses only class, text, description and bounds. The child's \
                     own widget class is all that arrives",
                ),
                NotApplicable(
                    "table and collection rows report AXStaticText, including a cell explicitly \
                     exposed as its own accessibility element",
                ),
            ],
        ),
        (
            R::Table,
            [
                Mapped,
                Mapped,
                Gap("mac lists report AXOutline rather than AXTable; AXOutline maps to Tree"),
                Gap(
                    "android.widget.TableLayout and TableRow both arrive as tokens: TableLayout \
                     folds into Group via the container rule, TableRow lands in Other, and \
                     GridView maps to List",
                ),
                NotApplicable(
                    "a table view reports AXGroup like any other container; no table token \
                     appears",
                ),
            ],
        ),
        (
            R::Cell,
            [
                Mapped,
                Gap(
                    "UIA's DataItem control type would carry this and is not mapped: the data \
                     grids probed expose their rows as TreeItem instead, though Edge does report \
                     an HTML table's header cells as DataItem",
                ),
                Mapped,
                Gap(
                    "CollectionItemInfo holds the row and column index, and neither reader \
                     carries it: the uiautomator dump has no such attribute and the service \
                     reader parses only class, text, description and bounds. No cell class \
                     exists to fall back on — a cell is whatever view the app put in the row",
                ),
                Mapped,
            ],
        ),
        (
            R::Tree,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android has no tree widget"),
                NotApplicable("UIKit has no outline view"),
            ],
        ),
        (
            R::TreeItem,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android has no tree widget"),
                NotApplicable("UIKit has no outline view"),
            ],
        ),
        (
            R::TabList,
            [
                Mapped,
                Mapped,
                Mapped,
                Gap(
                    "android.widget.TabWidget and TabHost do arrive as tokens and are not mapped \
                     yet; the Material tab strips seen in stock apps report a plain layout class \
                     instead, naming their tabs only by content description",
                ),
                Mapped,
            ],
        ),
        (
            R::Tab,
            [
                Mapped,
                Mapped,
                Gap(
                    "AppKit reports tab items as AXRadioButton inside the tab group; the \
                     containing role is not used to disambiguate yet",
                ),
                Gap(
                    "a framework tab arrives as a LinearLayout with selected=true under a \
                     TabWidget parent, which together identify it; the class name alone does \
                     not, and a Material tab strip carries the role in the view id instead",
                ),
                NotApplicable(
                    "a tab bar reports AXGroup and no per-item element appears in idb's \
                     describe; a segmented control reports one AXTabGroup, also with no \
                     per-segment element",
                ),
            ],
        ),
        (
            R::ScrollBar,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android scrollbars are drawn, not exposed as nodes"),
                NotApplicable("UIKit scroll indicators are not accessibility elements"),
            ],
        ),
        (R::Slider, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::SpinButton,
            [
                Mapped,
                Mapped,
                Gap("AXIncrementor is not mapped yet"),
                Gap("android.widget.NumberPicker does arrive as a token and is not mapped yet"),
                NotApplicable(
                    "a UIStepper decomposes into two AXButtons labelled Decrement and \
                     Increment; no stepper token appears",
                ),
            ],
        ),
        (
            R::ProgressBar,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                NotApplicable(
                    "a UIProgressView reports AXGenericElement carrying its percentage as the \
                     value; no progress token appears",
                ),
            ],
        ),
        (R::Image, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::Link,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android links are spans inside a text view, not separate nodes"),
                Mapped,
            ],
        ),
        (
            R::Separator,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable("Android dividers are drawn, not exposed as nodes"),
                NotApplicable("UIKit exposes no separator element"),
            ],
        ),
        (
            R::Toolbar,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable(
                    "android.widget.Toolbar was watched to report android.view.ViewGroup — a \
                     subclass inherits the accessibility class name of the framework class it \
                     extends, and the AppCompat and Material toolbars override neither, so all \
                     three arrive as ViewGroup",
                ),
                Mapped,
            ],
        ),
        (
            R::StatusBar,
            [
                Mapped,
                Mapped,
                NotApplicable("AppKit exposes status items as menu-bar items"),
                NotApplicable("the system status bar is outside the app tree"),
                NotApplicable("the system status bar is outside the app tree"),
            ],
        ),
        (
            R::Heading,
            [
                Mapped,
                Gap(
                    "UIA marks a heading with the HeadingLevel property — an h1 arrives as Text \
                     carrying level 80051 — and the reader maps by control type alone, so it \
                     never sees it. Header and HeaderItem are a grid's column headers, a \
                     different concept the normalized set has no role for",
                ),
                Mapped,
                Gap(
                    "AccessibilityNodeInfo's isHeading marks a heading, and neither reader \
                     carries it: the uiautomator dump has no such attribute and the service \
                     reader parses only class, text, description and bounds",
                ),
                Mapped,
            ],
        ),
    ]
};

/// Declared support for one role on one backend, or `None` when [`ROLE_SUPPORT`] has no row for
/// `role`.
///
/// [`AxRole::Other`] is the sink for unmapped native tokens rather than a mapping target, so it
/// deliberately has no row: asking about it — which is what walking a real snapshot's roles
/// does — answers `None` instead of failing. Total; the backend lookup cannot fail because
/// [`AxBackend::ALL`] is exhaustive.
pub fn support(role: AxRole, backend: AxBackend) -> Option<RoleSupport> {
    let idx = AxBackend::ALL.iter().position(|b| *b == backend)?;
    ROLE_SUPPORT
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, cells)| cells[idx])
}

/// Render [`ROLE_SUPPORT`] as the markdown block in `docs/reference/a11y-roles.md`: a
/// role × backend table of `yes` / `n/a` / `gap`, then the reason for every cell that is not
/// `yes`. A test asserts the doc matches this output.
pub fn render_markdown() -> String {
    use std::fmt::Write as _;
    let mut out = String::new();

    out.push_str("| Role |");
    for b in AxBackend::ALL {
        let _ = write!(out, " {} |", b.label());
    }
    out.push_str("\n|---|");
    out.push_str(&"---|".repeat(AxBackend::ALL.len()));
    out.push('\n');

    for (role, cells) in ROLE_SUPPORT {
        let _ = write!(out, "| `{role:?}` |");
        for cell in cells {
            let mark = match cell {
                RoleSupport::Mapped => "yes",
                RoleSupport::NotApplicable(_) => "n/a",
                RoleSupport::Gap(_) => "gap",
            };
            let _ = write!(out, " {mark} |");
        }
        out.push('\n');
    }

    out.push_str("\n### Why a cell is not `yes`\n\n");
    for (role, cells) in ROLE_SUPPORT {
        for (i, cell) in cells.iter().enumerate() {
            let (kind, reason) = match cell {
                RoleSupport::Mapped => continue,
                RoleSupport::NotApplicable(r) => ("n/a", *r),
                RoleSupport::Gap(r) => ("gap", *r),
            };
            let _ = writeln!(
                out,
                "- `{role:?}` / {} — {kind}: {reason}",
                AxBackend::ALL[i].label()
            );
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_role_has_exactly_one_row() {
        for role in AxRole::ALL {
            let rows = ROLE_SUPPORT.iter().filter(|(r, _)| *r == role).count();
            assert_eq!(rows, 1, "{role:?} must appear exactly once in ROLE_SUPPORT");
        }
        assert_eq!(ROLE_SUPPORT.len(), AxRole::ALL.len());
    }

    #[test]
    fn other_is_not_a_row() {
        // `Other` is the sink for unmapped tokens, not a mapping target.
        assert!(!ROLE_SUPPORT.iter().any(|(r, _)| *r == AxRole::Other));
    }

    #[test]
    fn unsupported_cells_carry_a_real_reason() {
        for (role, cells) in ROLE_SUPPORT {
            for (i, cell) in cells.iter().enumerate() {
                let reason = match cell {
                    RoleSupport::Mapped => continue,
                    RoleSupport::NotApplicable(r) | RoleSupport::Gap(r) => *r,
                };
                let backend = AxBackend::ALL[i];
                assert!(
                    reason.len() >= 10,
                    "{role:?}/{backend:?}: reason too thin to be useful: {reason:?}"
                );
                let lower = reason.to_ascii_lowercase();
                assert!(
                    !lower.contains("tbd") && !lower.contains("todo"),
                    "{role:?}/{backend:?}: placeholder reason {reason:?}"
                );
            }
        }
    }

    #[test]
    fn support_reads_the_right_cell() {
        assert_eq!(
            support(AxRole::Button, AxBackend::Linux),
            Some(RoleSupport::Mapped)
        );
        assert!(matches!(
            support(AxRole::MenuBar, AxBackend::Ios),
            Some(RoleSupport::NotApplicable(_))
        ));
    }

    #[test]
    fn support_of_other_is_none_rather_than_a_panic() {
        // `Other` has no row by design, and a caller asking about every role in a real
        // snapshot will hit it — that must answer `None`, not blow up mid-walk.
        assert_eq!(support(AxRole::Other, AxBackend::Linux), None);
    }

    #[test]
    fn linux_column_is_complete() {
        // AT-SPI is the reference vocabulary: every role must be reachable there.
        for role in AxRole::ALL {
            assert_eq!(
                support(role, AxBackend::Linux).expect("declared in ROLE_SUPPORT"),
                RoleSupport::Mapped,
                "{role:?} unmapped on the reference backend"
            );
        }
    }
}
