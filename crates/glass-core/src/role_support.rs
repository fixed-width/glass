//! What accessibility role each backend can produce — the declared parity matrix.
//!
//! A backend maps native tokens (AT-SPI roles, UIA control types, AX role strings, Android
//! widget classes, iOS role strings) onto [`AxRole`]. Coverage differs per platform: for some
//! roles nothing in the backend's vocabulary carries them, others simply are not mapped yet.
//! This module states which is which, so the difference is a checked fact rather than folklore.
//!
//! Where a control exists to look at, which state a cell gets is decided by observation rather
//! than by what a platform's API reference implies: the control is put on screen and the tree is
//! read back. `examples/android-role-fixture/` and `examples/ios-role-fixture/` hold those
//! controls so a cell can be re-read.
//!
//! What that can and cannot establish is the point of [`Basis`]. A cell saying the control was
//! watched arriving unmarked is a bounded reading — one OS version, one reader, the controls
//! tried — and on Android and iOS it could never be more than that, because those vocabularies
//! are open: an app sets its own `AccessibilityNodeInfo` class name, and iOS role strings come
//! from the Simulator's translator. None of these cells claims a role is impossible; they claim
//! nothing was found carrying it.
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

    /// Human-readable column heading, naming the native vocabulary.
    ///
    /// Also the compile-time guard for [`AxBackend::ALL`]: every completeness claim on the matrix
    /// is quantified over `ALL`, so a backend missing from it would silently weaken all of them,
    /// and this exhaustive match stops compiling until a new variant is classified here. (A
    /// never-called guard function did that job before; it could not be tested, only compiled.)
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
    /// Nothing glass reads carries the role, so no mapping could reach it. [`Basis`] says on what
    /// footing — which is not the same footing in every cell, and never an exhaustiveness proof.
    NotApplicable {
        /// What kind of unavailability this is: the control does not exist, exists but arrives
        /// unmarked, or is exposed somewhere glass does not walk.
        basis: Basis,
        /// The native token the control was observed to report in this role's place, in the
        /// form that backend's own resolver accepts. `None` where nothing stands in for it —
        /// the platform has no such control (no menu bar on iOS), or the concept sits outside
        /// what glass walks at all (the system status bar).
        instead: Option<&'static str>,
        /// What was read, and why it forecloses the role.
        why: &'static str,
    },
    /// Closing this is glass's own work: the platform already carries the role somewhere glass
    /// reads or could read — a token that arrives unmapped, a documented field the reader
    /// ignores, a protocol its own companion could send.
    Gap {
        /// The native token that arrives carrying this role's meaning and is not mapped to it,
        /// in the form that backend's own resolver accepts. `None` where the carrier is not a
        /// token at all but a field or property the reader skips — UIA's `HeadingLevel`,
        /// `AccessibilityNodeInfo`'s `CollectionItemInfo` — which `why` then names.
        unmapped: Option<&'static str>,
        /// What is there and unread, and what it would take to reach it.
        why: &'static str,
    },
}

impl RoleSupport {
    /// The native token this cell names, whichever variant it is — what the control reports
    /// instead, or what arrives unmapped. `None` for [`RoleSupport::Mapped`] and for cells whose
    /// carrier is a field rather than a token.
    ///
    /// This is what makes a cell checkable: each backend's column test resolves the token through
    /// its own map and asserts it does NOT produce this role, so a cell claiming a role is out of
    /// reach while naming a token that maps straight to it fails the build.
    pub fn named_token(self) -> Option<&'static str> {
        match self {
            RoleSupport::Mapped => None,
            RoleSupport::NotApplicable { instead: token, .. }
            | RoleSupport::Gap {
                unmapped: token, ..
            } => token,
        }
    }

    /// The reason text, or `None` for [`RoleSupport::Mapped`].
    pub fn why(self) -> Option<&'static str> {
        match self {
            RoleSupport::Mapped => None,
            RoleSupport::NotApplicable { why, .. } | RoleSupport::Gap { why, .. } => Some(why),
        }
    }
}

/// On what footing a [`RoleSupport::NotApplicable`] cell stands. None of these is a proof of
/// impossibility: they record what reading a real control found, bounded by the controls tried,
/// the OS version, and the reader. Two of the three vocabularies are open — an Android app sets
/// its own `AccessibilityNodeInfo` class name, and iOS role strings come from the Simulator's
/// translator — so on those columns no amount of reading could close the question. Only UIA
/// names a closed, documented set of control types.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Basis {
    /// The platform has no such control to expose — a radio button on iOS, a tree on Android.
    /// The claim is about the platform's controls, so no reading of an app could overturn it
    /// short of an app building the concept itself out of other controls.
    Absent,
    /// The control exists and is on screen, and no reading found a token carrying its role: it is
    /// drawn but unmarked, or folded into a token that means something else. This is the bounded
    /// one — it says what was watched, not what is possible.
    Unmarked,
    /// The control exists and is exposed, but outside what glass walks — the system status bar
    /// belongs to the OS shell, not to the app whose tree is being read.
    Elsewhere,
}

/// Role coverage per backend. Cells are ordered as [`AxBackend::ALL`], and the row type is sized
/// from it so a new backend is a compile error here rather than a silent column mismatch.
pub const ROLE_SUPPORT: &[(AxRole, [RoleSupport; AxBackend::ALL.len()])] = {
    use AxRole as R;
    use Basis::{Absent, Elsewhere, Unmarked};
    use RoleSupport::{Gap, Mapped, NotApplicable};
    &[
        (
            R::Application,
            [
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("Window"),
                    why: "UIA has no application control type; the app root is a Window",
                },
                Gap {
                    unmapped: Some("AXApplication"),
                    why: "AXApplication is the app element but is not mapped yet",
                },
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "Android exposes windows, not an application element",
                },
                Mapped,
            ],
        ),
        (R::Window, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::Dialog,
            [
                Mapped,
                Gap {
                    unmapped: None,
                    why: "UIA marks a dialog with the IsDialog property on a Window; the reader does \
                     not read it",
                },
                Gap {
                    unmapped: Some("AXSheet"),
                    why: "AXSheet, AXPopover and AXDrawer are not mapped yet",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("android.widget.FrameLayout"),
                    why: "a framework AlertDialog's panels report FrameLayout and LinearLayout under \
                     android:id/parentPanel, and AccessibilityWindowInfo's window types carry no \
                     dialog kind for a reader to fall back on",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "an alert and an action sheet each expose their title, message and buttons \
                     directly under the application element, with no container token between",
                },
            ],
        ),
        (R::Group, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (R::Button, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (R::ToggleButton, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::RadioButton,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "UIKit has no radio control; a UISegmentedControl — the nearest equivalent \
                     — reports one AXTabGroup with no per-segment element",
                },
            ],
        ),
        (R::CheckBox, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::MenuBar,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "Android apps have no menu bar",
                },
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "iOS apps have no menu bar",
                },
            ],
        ),
        (
            R::Menu,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("android.widget.ListView"),
                    why: "a popup menu reports android.widget.ListView; no menu token appears \
                     anywhere in the tree it opens",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXGroup"),
                    why: "a button's pull-down UIMenu opens as an AXGroup of AXButtons alongside a \
                     Dismiss context menu button; no menu token appears",
                },
            ],
        ),
        (
            R::MenuItem,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("android.widget.LinearLayout"),
                    why: "a menu entry reports its item view's layout class — a LinearLayout or \
                     RelativeLayout holding a TextView title",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXButton"),
                    why: "a menu entry reports AXButton, the same token as any other button",
                },
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
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("android.widget.EditText"),
                    why: "Android uses one editable class for single- and multi-line text",
                },
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
                Gap {
                    unmapped: Some("AXPopUpButton"),
                    why: "a menu-style SwiftUI Picker reports AXPopUpButton, which is not mapped yet; \
                     the inline style reports a heading with buttons, and a UIPickerView reports \
                     AXSlider",
                },
            ],
        ),
        (
            R::List,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXGroup"),
                    why: "a UITableView and a SwiftUI List both report AXGroup, and their rows report \
                     AXStaticText; no list token appears",
                },
            ],
        ),
        (
            R::ListItem,
            [
                Mapped,
                Mapped,
                Mapped,
                Gap {
                    unmapped: None,
                    why: "AccessibilityNodeInfo's CollectionItemInfo marks a list child, and neither \
                     reader carries it: the uiautomator dump has no such attribute and the \
                     service reader parses only class, text, description and bounds. The child's \
                     own widget class is all that arrives",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXStaticText"),
                    why: "table and collection rows report AXStaticText, including a cell explicitly \
                     exposed as its own accessibility element",
                },
            ],
        ),
        (
            R::Table,
            [
                Mapped,
                Mapped,
                Gap {
                    unmapped: None,
                    why: "mac lists report AXOutline rather than AXTable; AXOutline maps to Tree",
                },
                Gap {
                    unmapped: Some("android.widget.TableLayout"),
                    why: "android.widget.TableLayout and TableRow both arrive as tokens: TableLayout \
                     folds into Group via the container rule, TableRow lands in Other, and \
                     GridView maps to List",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXGroup"),
                    why: "a table view reports AXGroup like any other container; no table token \
                     appears",
                },
            ],
        ),
        (
            R::Cell,
            [
                Mapped,
                Gap {
                    unmapped: Some("DataItem"),
                    why: "UIA's DataItem control type would carry this and is not mapped: the data \
                     grids probed expose their rows as TreeItem instead, though Edge does report \
                     an HTML table's header cells as DataItem",
                },
                Mapped,
                Gap {
                    unmapped: None,
                    why: "CollectionItemInfo holds the row and column index, and neither reader \
                     carries it: the uiautomator dump has no such attribute and the service \
                     reader parses only class, text, description and bounds. No cell class \
                     exists to fall back on — a cell is whatever view the app put in the row",
                },
                Mapped,
            ],
        ),
        (
            R::Tree,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "Android has no tree widget",
                },
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "UIKit has no outline view",
                },
            ],
        ),
        (
            R::TreeItem,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "Android has no tree widget",
                },
                NotApplicable {
                    basis: Absent,
                    instead: None,
                    why: "UIKit has no outline view",
                },
            ],
        ),
        (
            R::TabList,
            [
                Mapped,
                Mapped,
                Mapped,
                Gap {
                    unmapped: Some("android.widget.TabWidget"),
                    why: "android.widget.TabWidget and TabHost do arrive as tokens and are not mapped \
                     yet; the Material tab strips seen in stock apps report a plain layout class \
                     instead, naming their tabs only by content description",
                },
                Mapped,
            ],
        ),
        (
            R::Tab,
            [
                Mapped,
                Mapped,
                Gap {
                    unmapped: Some("AXRadioButton"),
                    why: "AppKit reports tab items as AXRadioButton inside the tab group; the \
                     containing role is not used to disambiguate yet",
                },
                Gap {
                    unmapped: None,
                    why: "a framework tab arrives as a LinearLayout with selected=true under a \
                     TabWidget parent, which together identify it; the class name alone does \
                     not, and a Material tab strip carries the role in the view id instead",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXGroup"),
                    why: "a tab bar reports AXGroup and no per-item element appears in idb's \
                     describe; a segmented control reports one AXTabGroup, also with no \
                     per-segment element",
                },
            ],
        ),
        (
            R::ScrollBar,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "Android scrollbars are drawn, not exposed as nodes",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "UIKit scroll indicators are not accessibility elements",
                },
            ],
        ),
        (R::Slider, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::SpinButton,
            [
                Mapped,
                Mapped,
                Gap {
                    unmapped: Some("AXIncrementor"),
                    why: "AXIncrementor is not mapped yet",
                },
                Gap {
                    unmapped: Some("android.widget.NumberPicker"),
                    why: "android.widget.NumberPicker does arrive as a token and is not mapped yet",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXButton"),
                    why: "a UIStepper decomposes into two AXButtons labelled Decrement and \
                     Increment; no stepper token appears",
                },
            ],
        ),
        (
            R::ProgressBar,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("AXGenericElement"),
                    why: "a UIProgressView reports AXGenericElement carrying its percentage as the \
                     value; no progress token appears",
                },
            ],
        ),
        (R::Image, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::Link,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "Android links are spans inside a text view, not separate nodes",
                },
                Mapped,
            ],
        ),
        (
            R::Separator,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "Android dividers are drawn, not exposed as nodes",
                },
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "UIKit exposes no separator element",
                },
            ],
        ),
        (
            R::Toolbar,
            [
                Mapped,
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: Some("android.view.ViewGroup"),
                    why: "android.widget.Toolbar was watched to report android.view.ViewGroup — a \
                     subclass inherits the accessibility class name of the framework class it \
                     extends, and the AppCompat and Material toolbars override neither, so all \
                     three arrive as ViewGroup",
                },
                Mapped,
            ],
        ),
        (
            R::StatusBar,
            [
                Mapped,
                Mapped,
                NotApplicable {
                    basis: Unmarked,
                    instead: None,
                    why: "AppKit exposes status items as menu-bar items",
                },
                NotApplicable {
                    basis: Elsewhere,
                    instead: None,
                    why: "the system status bar is outside the app tree",
                },
                NotApplicable {
                    basis: Elsewhere,
                    instead: None,
                    why: "the system status bar is outside the app tree",
                },
            ],
        ),
        (
            R::Heading,
            [
                Mapped,
                Gap {
                    unmapped: None,
                    why: "UIA marks a heading with the HeadingLevel property — an h1 arrives as Text \
                     carrying level 80051 — and the reader maps by control type alone, so it \
                     never sees it. Header and HeaderItem are a grid's column headers, a \
                     different concept the normalized set has no role for",
                },
                Mapped,
                Gap {
                    unmapped: None,
                    why: "AccessibilityNodeInfo's isHeading marks a heading, and neither reader \
                     carries it: the uiautomator dump has no such attribute and the service \
                     reader parses only class, text, description and bounds",
                },
                Mapped,
            ],
        ),
        (
            R::Document,
            [
                Mapped,
                Gap {
                    unmapped: None,
                    why: "UIA's Document control type maps to TextArea, read from a text \
                     editor's edit surface; what a web document reports on this backend has \
                     not been read, so the cell waits on that reading",
                },
                Mapped,
                Mapped,
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
            // `absent`, `unmarked` and `elsewhere` render apart rather than collapsing into one
            // `n/a`: they are different claims, and only the middle one is a bounded reading.
            let mark = match cell {
                RoleSupport::Mapped => "yes",
                RoleSupport::NotApplicable { basis, .. } => match basis {
                    Basis::Absent => "absent",
                    Basis::Unmarked => "unmarked",
                    Basis::Elsewhere => "elsewhere",
                },
                RoleSupport::Gap { .. } => "gap",
            };
            let _ = write!(out, " {mark} |");
        }
        out.push('\n');
    }

    out.push_str("\n### Why a cell is not `yes`\n\n");
    for (role, cells) in ROLE_SUPPORT {
        for (i, cell) in cells.iter().enumerate() {
            // The token, where a cell names one, is rendered as its own clause rather than left
            // buried in the prose: it is the fact a reader with an `Other(...)` in front of them
            // is looking for, and the fact each backend's column test checks.
            let (kind, token) = match cell {
                RoleSupport::Mapped => continue,
                RoleSupport::NotApplicable { basis, instead, .. } => {
                    let kind = match basis {
                        Basis::Absent => "absent",
                        Basis::Unmarked => "unmarked",
                        Basis::Elsewhere => "elsewhere",
                    };
                    (kind, instead.map(|t| format!("reports `{t}` instead")))
                }
                RoleSupport::Gap { unmapped, .. } => {
                    ("gap", unmapped.map(|t| format!("`{t}` arrives unmapped")))
                }
            };
            let reason = cell.why().expect("a non-Mapped cell has a reason");
            let token = token.map(|t| format!(" ({t})")).unwrap_or_default();
            let _ = writeln!(
                out,
                "- `{role:?}` / {} — {kind}{token}: {reason}",
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
    fn all_lists_each_backend_exactly_once() {
        // `ALL` is what every completeness claim is quantified over, and `label`'s exhaustive match
        // is what stops a new variant being added without being classified. Distinct labels are the
        // observable half: a duplicated entry would quietly halve a claim's coverage.
        let mut labels: Vec<&str> = AxBackend::ALL.iter().map(|b| b.label()).collect();
        let listed = labels.len();
        labels.sort_unstable();
        labels.dedup();
        assert_eq!(labels.len(), listed, "a backend is listed twice in ALL");
        assert_eq!(
            labels,
            [
                "Android",
                "Linux (AT-SPI)",
                "Windows (UIA)",
                "iOS",
                "macOS (AX)"
            ]
        );
    }

    #[test]
    fn a_cell_names_the_token_it_carries() {
        // A cell that stopped reporting its token would make every backend column test vacuous:
        // each resolves this token through its own map and asserts it does not produce the role.
        assert_eq!(RoleSupport::Mapped.named_token(), None);
        assert_eq!(
            RoleSupport::Gap {
                unmapped: Some("AXSheet"),
                why: "not mapped yet",
            }
            .named_token(),
            Some("AXSheet")
        );
        assert_eq!(
            RoleSupport::NotApplicable {
                basis: Basis::Unmarked,
                instead: Some("AXGroup"),
                why: "reports a plain container",
            }
            .named_token(),
            Some("AXGroup")
        );
        // A cell whose carrier is a field rather than a token names none.
        assert_eq!(
            RoleSupport::Gap {
                unmapped: None,
                why: "the reader does not read the property",
            }
            .named_token(),
            None
        );
    }

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
                let Some(reason) = cell.why() else { continue };
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
        // iOS has no menu bar to expose, so this one is Absent rather than merely unmarked —
        // the basis is part of what the cell claims, so pin it.
        assert!(matches!(
            support(AxRole::MenuBar, AxBackend::Ios),
            Some(RoleSupport::NotApplicable {
                basis: Basis::Absent,
                ..
            })
        ));
    }

    #[test]
    fn a_cell_naming_a_token_names_one_token_and_not_prose() {
        // The token is machine-checked by each backend's column test, so it has to be a token:
        // a phrase slipped into the field would resolve to `Other` and pass every check
        // vacuously.
        for (role, cells) in ROLE_SUPPORT {
            for (i, cell) in cells.iter().enumerate() {
                let Some(token) = cell.named_token() else {
                    continue;
                };
                let backend = AxBackend::ALL[i];
                assert!(
                    !token.contains(' ') && !token.is_empty(),
                    "{role:?}/{backend:?}: {token:?} is prose, not a single native token"
                );
            }
        }
    }

    #[test]
    fn an_absent_control_reports_nothing_in_its_place() {
        // `Absent` says the platform has no such control. A control that does not exist cannot
        // have been watched reporting something else, so naming a token there would mean one of
        // the two is wrong — most likely the basis, which is the stronger claim of the pair.
        for (role, cells) in ROLE_SUPPORT {
            for (i, cell) in cells.iter().enumerate() {
                if let RoleSupport::NotApplicable {
                    basis: Basis::Absent,
                    instead: Some(token),
                    ..
                } = cell
                {
                    panic!(
                        "{role:?}/{:?}: declared Absent but names {token} as what it reports \
                         instead — if a control reported that, it is Unmarked, not Absent",
                        AxBackend::ALL[i]
                    );
                }
            }
        }
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

    #[test]
    fn document_row_declares_every_backend() {
        for backend in AxBackend::ALL {
            assert!(
                support(AxRole::Document, backend).is_some(),
                "{backend:?} has no Document cell"
            );
        }
        // Windows keeps UIA Document on TextArea until a web document is read there.
        // `unmapped` stays `None`: UIA's Document IS mapped, just to another role, so
        // "`Document` arrives unmapped" would send a reader hunting for `Other(Document)`.
        assert!(matches!(
            support(AxRole::Document, AxBackend::Windows),
            Some(RoleSupport::Gap { unmapped: None, .. })
        ));
    }
}
