//! Pure mapping from UI Automation control-type ids + gathered state facts into glass's
//! normalized `AxRole`/`AxStates`. No UIA calls — unit-tested directly on the Linux dev box.
//! Control-type ids are the stable UIA `ControlTypeId` constants (50000..=50040); the reader
//! passes the numeric id so this module needs no `uiautomation` dependency.

use glass_core::{AxRole, AxStates};

/// Every UIA control type glass maps, as `(ControlTypeId, canonical name, role)`. The
/// canonical name is the control type's documented English name — it is what the reader puts
/// in `raw_role`, so an unmapped-but-known control type is still legible to a caller and
/// identical on every machine, unlike UIA's localized control-type string.
pub const ROLE_TOKENS: &[(u32, &str, AxRole)] = &[
    (50000, "Button", AxRole::Button),
    (50002, "CheckBox", AxRole::CheckBox),
    (50003, "ComboBox", AxRole::ComboBox),
    (50004, "Edit", AxRole::TextField),
    (50005, "Hyperlink", AxRole::Link),
    (50006, "Image", AxRole::Image),
    (50007, "ListItem", AxRole::ListItem),
    (50008, "List", AxRole::List),
    (50009, "Menu", AxRole::Menu),
    (50010, "MenuBar", AxRole::MenuBar),
    (50011, "MenuItem", AxRole::MenuItem),
    (50012, "ProgressBar", AxRole::ProgressBar),
    (50013, "RadioButton", AxRole::RadioButton),
    (50014, "ScrollBar", AxRole::ScrollBar),
    (50015, "Slider", AxRole::Slider),
    (50016, "Spinner", AxRole::SpinButton),
    (50017, "StatusBar", AxRole::StatusBar),
    (50018, "Tab", AxRole::TabList),
    (50019, "TabItem", AxRole::Tab),
    (50020, "Text", AxRole::Label),
    (50021, "ToolBar", AxRole::Toolbar),
    (50023, "Tree", AxRole::Tree),
    (50024, "TreeItem", AxRole::TreeItem),
    (50026, "Group", AxRole::Group),
    (50028, "DataGrid", AxRole::Table),
    // A split button is an actionable button carrying a dropdown.
    (50031, "SplitButton", AxRole::Button),
    (50032, "Window", AxRole::Window),
    (50033, "Pane", AxRole::Group),
    (50036, "Table", AxRole::Table),
    (50038, "Separator", AxRole::Separator),
];

/// Map a UIA `ControlTypeId` to the normalized `AxRole`; unmapped ids become
/// `AxRole::Other` (the reader keeps the canonical control-type name in `raw_role`).
pub fn map_role(control_type_id: u32) -> AxRole {
    ROLE_TOKENS
        .iter()
        .find(|(id, _, _)| *id == control_type_id)
        .map(|(_, _, role)| *role)
        .unwrap_or(AxRole::Other)
}

/// The control type's canonical English name, or `None` for an id glass does not know.
pub fn canonical_name(control_type_id: u32) -> Option<&'static str> {
    ROLE_TOKENS
        .iter()
        .find(|(id, _, _)| *id == control_type_id)
        .map(|(_, name, _)| *name)
}

/// Plain state facts the reader gathers from a UIA element (no `uiautomation` types here,
/// so this stays unit-testable on Linux). `editable` is the reader's derived
/// "text control AND not read-only"; `toggled_on` is `TogglePattern.ToggleState == On`;
/// `checkable` is Toggle-pattern *availability* — the pattern is present on the element,
/// independent of whether its current toggle state was actually readable.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct StateFacts {
    pub enabled: bool,
    pub offscreen: bool,
    pub focused: bool,
    pub focusable: bool,
    pub selected: bool,
    pub toggled_on: bool,
    pub expanded: bool,
    pub editable: bool,
    pub checkable: bool,
}

/// Map gathered facts to the normalized `AxStates`.
pub fn map_states(f: &StateFacts) -> AxStates {
    AxStates {
        focused: f.focused,
        focusable: f.focusable,
        enabled: f.enabled,
        visible: !f.offscreen,
        selected: f.selected,
        checked: f.toggled_on,
        checkable: f.checkable,
        expanded: f.expanded,
        editable: f.editable,
    }
}

/// Render a `RangeValuePattern` numeric value (a slider/spinner/progress position) as the node's
/// `value` string. Uses `f64`'s shortest round-tripping `Display`, so a whole number has no
/// trailing `.0` (a slider at `50.0` → `"50"`, matching `value_contains:"50"`) while a fractional
/// position keeps its digits (`50.5` → `"50.5"`).
pub fn format_range_value(v: f64) -> String {
    format!("{v}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_control_types_map() {
        assert_eq!(map_role(50000), AxRole::Button);
        assert_eq!(map_role(50002), AxRole::CheckBox);
        assert_eq!(map_role(50004), AxRole::TextField);
        assert_eq!(map_role(50011), AxRole::MenuItem);
        assert_eq!(map_role(50018), AxRole::TabList);
        assert_eq!(map_role(50019), AxRole::Tab);
        assert_eq!(map_role(50032), AxRole::Window);
        assert_eq!(map_role(50020), AxRole::Label);
        assert_eq!(map_role(50031), AxRole::Button); // SplitButton
    }
    #[test]
    fn unmapped_control_type_is_other() {
        assert_eq!(map_role(50001), AxRole::Other); // Calendar
        assert_eq!(map_role(99999), AxRole::Other);
    }
    #[test]
    fn offscreen_clears_visible_and_toggle_sets_checked() {
        let f = StateFacts {
            enabled: true,
            offscreen: true,
            toggled_on: true,
            ..Default::default()
        };
        let s = map_states(&f);
        assert!(s.enabled && s.checked);
        assert!(!s.visible);
    }
    #[test]
    fn focus_and_editable_map() {
        let f = StateFacts {
            focused: true,
            focusable: true,
            editable: true,
            ..Default::default()
        };
        let s = map_states(&f);
        assert!(s.focused && s.focusable && s.editable);
        assert!(!s.selected && !s.checked);
    }
    #[test]
    fn checkable_from_toggle_pattern_fact() {
        let f = StateFacts {
            checkable: true,
            toggled_on: true,
            ..Default::default()
        };
        assert!(map_states(&f).checkable && map_states(&f).checked);
        assert!(!map_states(&StateFacts::default()).checkable);
    }

    #[test]
    fn checkable_and_checked_are_independent_fields() {
        // checkable != toggled_on — a fixture like this catches a swapped-field bug that
        // `checkable_from_toggle_pattern_fact`'s checkable+toggled_on-together fixture cannot.
        let f = StateFacts {
            checkable: true,
            toggled_on: false,
            ..Default::default()
        };
        let s = map_states(&f);
        assert!(s.checkable && !s.checked);
    }

    #[test]
    fn range_value_formats_without_trailing_zero() {
        assert_eq!(format_range_value(50.0), "50");
        assert_eq!(format_range_value(0.0), "0");
        assert_eq!(format_range_value(100.0), "100");
        assert_eq!(format_range_value(50.5), "50.5");
        assert_eq!(format_range_value(-3.0), "-3");
    }

    #[test]
    fn role_tokens_have_no_duplicate_ids() {
        for (i, (id, _, _)) in ROLE_TOKENS.iter().enumerate() {
            assert!(
                !ROLE_TOKENS[i + 1..].iter().any(|(other, _, _)| other == id),
                "control type {id} listed twice"
            );
        }
    }

    #[test]
    fn canonical_name_is_stable_and_locale_free() {
        assert_eq!(canonical_name(50000), Some("Button"));
        assert_eq!(canonical_name(50036), Some("Table"));
        assert_eq!(canonical_name(49999), None);
    }

    #[test]
    fn map_matches_declared_column() {
        use glass_core::role_support::{support, AxBackend, RoleSupport};
        use glass_core::AxRole;
        for role in AxRole::ALL {
            let mapped = ROLE_TOKENS.iter().any(|(_, _, r)| *r == role);
            match support(role, AxBackend::Windows).expect("declared in ROLE_SUPPORT") {
                RoleSupport::Mapped => {
                    assert!(
                        mapped,
                        "{role:?} declared Mapped but no control type maps to it"
                    )
                }
                RoleSupport::NotApplicable(_) | RoleSupport::Gap(_) => assert!(
                    !mapped,
                    "{role:?} is produced by a control type but the matrix does not declare it"
                ),
            }
        }
    }
}
