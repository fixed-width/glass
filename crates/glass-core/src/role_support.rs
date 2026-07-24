//! What accessibility role each backend can produce — the declared parity matrix.
//!
//! A backend maps native tokens (AT-SPI roles, UIA control types, AX role strings, Android
//! widget classes, iOS role strings) onto [`AxRole`]. Coverage differs per platform: some
//! roles have no counterpart at all, others simply are not mapped yet. This module states
//! which is which, so the difference is a checked fact rather than folklore.
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
    /// The platform has no counterpart. The reason says what it does instead.
    NotApplicable(&'static str),
    /// The platform has a counterpart glass does not map yet. The reason says what is missing.
    Gap(&'static str),
}

/// Role coverage per backend. Cells are ordered as [`AxBackend::ALL`].
pub const ROLE_SUPPORT: &[(AxRole, [RoleSupport; 5])] =
    {
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
                Gap("UIA marks a dialog with the IsDialog property on a Window; not read yet"),
                Gap("AXSheet, AXPopover and AXDrawer are not mapped yet"),
                Gap("dialog windows arrive as a generic layout class"),
                Gap("alert and action-sheet tokens are not mapped yet"),
            ],
        ),
        (
            R::Group,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                Gap("no container token is mapped yet"),
            ],
        ),
        (R::Button, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::ToggleButton,
            [
                Mapped,
                Gap("UIA exposes a toggle as a control type plus the Toggle pattern, which the \
                     map does not key on yet"),
                NotApplicable("AppKit reports switches as AXCheckBox; there is no separate \
                               toggle-button role"),
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
                Gap("no radio token is mapped yet"),
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
                Gap("popup menus arrive as list or layout classes"),
                Gap("context-menu tokens are not mapped yet"),
            ],
        ),
        (
            R::MenuItem,
            [
                Mapped,
                Mapped,
                Mapped,
                Gap("menu entries arrive as their own widget class"),
                Gap("menu-item tokens are not mapped yet"),
            ],
        ),
        (R::Label, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (R::TextField, [Mapped, Mapped, Mapped, Mapped, Mapped]),
        (
            R::TextArea,
            [
                Mapped,
                Gap("the UIA Document control type is not mapped yet"),
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
                Gap("picker tokens are not mapped yet"),
            ],
        ),
        (
            R::List,
            [Mapped, Mapped, Mapped, Mapped, Gap("no list token is mapped yet")],
        ),
        (
            R::ListItem,
            [
                Mapped,
                Mapped,
                Mapped,
                Gap("list children report their own widget class, not a list-item role"),
                Gap("no list-item token is mapped yet"),
            ],
        ),
        (
            R::Table,
            [
                Mapped,
                Mapped,
                Gap("AXTable is not mapped yet"),
                Gap("table and grid classes collapse into List"),
                Gap("no table token is mapped yet"),
            ],
        ),
        (
            R::Cell,
            [
                Mapped,
                Gap("the UIA DataItem control type is not mapped yet"),
                Mapped,
                Gap("no cell class is mapped yet"),
                Mapped,
            ],
        ),
        (
            R::Tree,
            [
                Mapped,
                Mapped,
                Gap("AXOutline is not mapped yet"),
                NotApplicable("Android has no tree widget"),
                NotApplicable("UIKit has no outline view"),
            ],
        ),
        (
            R::TreeItem,
            [
                Mapped,
                Mapped,
                Gap("the outline-row subrole is not read yet"),
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
                Gap("tab-container classes are not mapped yet"),
                Mapped,
            ],
        ),
        (
            R::Tab,
            [
                Mapped,
                Mapped,
                Gap("AppKit reports tab items as AXRadioButton inside the tab group; the \
                     containing role is not used to disambiguate yet"),
                Gap("tab children are not mapped yet"),
                Gap("tab-item tokens are not mapped yet"),
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
                Gap("the number-picker class is not mapped yet"),
                Gap("the stepper token is not mapped yet"),
            ],
        ),
        (
            R::ProgressBar,
            [
                Mapped,
                Mapped,
                Mapped,
                Mapped,
                Gap("no progress token is mapped yet"),
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
                Gap("AXSplitter is not mapped yet"),
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
                Gap("the toolbar class is not mapped yet"),
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
                Gap("the UIA Header control types are not mapped yet"),
                Gap("AXHeading is not mapped yet"),
                Gap("heading semantics are not mapped yet"),
                Gap("the header token is not mapped yet"),
            ],
        ),
    ]
    };

/// Declared support for one role on one backend. Panics only if [`ROLE_SUPPORT`] is missing a
/// row, which the module's own test forbids.
pub fn support(role: AxRole, backend: AxBackend) -> RoleSupport {
    let idx = AxBackend::ALL
        .iter()
        .position(|b| *b == backend)
        .expect("AxBackend::ALL covers every backend");
    ROLE_SUPPORT
        .iter()
        .find(|(r, _)| *r == role)
        .map(|(_, cells)| cells[idx])
        .unwrap_or_else(|| panic!("ROLE_SUPPORT has no row for {role:?}"))
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
            RoleSupport::Mapped
        );
        assert!(matches!(
            support(AxRole::MenuBar, AxBackend::Ios),
            RoleSupport::NotApplicable(_)
        ));
    }

    #[test]
    fn linux_column_is_complete() {
        // AT-SPI is the reference vocabulary: every role must be reachable there.
        for role in AxRole::ALL {
            assert_eq!(
                support(role, AxBackend::Linux),
                RoleSupport::Mapped,
                "{role:?} unmapped on the reference backend"
            );
        }
    }
}
