#![forbid(unsafe_code)]
//! Pure mapping from AXUIElement role strings + gathered state facts into glass's
//! normalized `AxRole`/`AxStates`. No AXUIElement/objc2 calls — unit-tested directly on
//! the Linux dev box. AX role strings (`kAXRoleAttribute`'s value) are the stable
//! `"AXButton"`/`"AXTextField"`/... constants; the reader passes the string so this
//! module needs no macOS-only dependency.

use glass_core::{AxRole, AxStates};

/// Every AX role string glass maps. AX role strings are canonical constants, so lookup is
/// case-sensitive.
pub const ROLE_TOKENS: &[(&str, AxRole)] = &[
    ("AXButton", AxRole::Button),
    ("AXCheckBox", AxRole::CheckBox),
    ("AXRadioButton", AxRole::RadioButton),
    ("AXRadioGroup", AxRole::Group),
    ("AXTextField", AxRole::TextField),
    ("AXTextArea", AxRole::TextArea),
    ("AXStaticText", AxRole::Label),
    ("AXWindow", AxRole::Window),
    ("AXGroup", AxRole::Group),
    ("AXMenu", AxRole::Menu),
    ("AXMenuItem", AxRole::MenuItem),
    ("AXMenuBar", AxRole::MenuBar),
    ("AXImage", AxRole::Image),
    ("AXLink", AxRole::Link),
    ("AXSlider", AxRole::Slider),
    ("AXComboBox", AxRole::ComboBox),
    ("AXPopUpButton", AxRole::ComboBox),
    ("AXList", AxRole::List),
    ("AXRow", AxRole::ListItem),
    ("AXCell", AxRole::Cell),
    ("AXToolbar", AxRole::Toolbar),
    ("AXTabGroup", AxRole::TabList),
    ("AXProgressIndicator", AxRole::ProgressBar),
    ("AXScrollBar", AxRole::ScrollBar),
    ("AXOutline", AxRole::Tree),
    ("AXScrollArea", AxRole::Group),
    ("AXSplitGroup", AxRole::Group),
    ("AXSplitter", AxRole::Separator),
    ("AXHeading", AxRole::Heading),
    ("AXMenuButton", AxRole::Button),
];

/// AppKit uses `AXSubrole` to say what an element really is only for a handful of base roles —
/// an `AXRow` is a table row or an outline row, an `AXWindow` is a plain window or a dialog or
/// a sheet, an `AXTextField` may be a search field. Every other role is already unambiguous,
/// and each subrole read is an AX IPC round-trip, so the reader asks only for these.
pub fn subrole_matters(ax_role: &str) -> bool {
    matches!(
        ax_role,
        "AXWindow" | "AXTextField" | "AXRow" | "AXGroup" | "AXUnknown"
    )
}

/// Map an AX role string, plus its `AXSubrole` when the reader took one, to the normalized
/// `AxRole`; unmapped roles become `AxRole::Other` (the reader keeps the token in `raw_role`).
pub fn map_role(ax_role: &str, subrole: Option<&str>) -> AxRole {
    // A row's subrole is what separates an outline row from a table row; AppKit reports both
    // as AXRow. Every other disambiguation the probe found is expressible in the table.
    if ax_role == "AXRow" && subrole == Some("AXOutlineRow") {
        return AxRole::TreeItem;
    }
    ROLE_TOKENS
        .iter()
        .find(|(token, _)| *token == ax_role)
        .map(|(_, role)| *role)
        .unwrap_or(AxRole::Other)
}

/// Plain state facts the reader gathers from an AXUIElement (no objc2/AX types here, so
/// this stays unit-testable on Linux). Field names mirror `glass_core::AxStates` 1:1.
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxStateFacts {
    pub enabled: bool,
    pub focused: bool,
    pub focusable: bool,
    pub selected: bool,
    pub checked: bool,
    pub checkable: bool,
    pub expanded: bool,
    pub editable: bool,
    pub visible: bool,
}

/// Map gathered facts to the normalized `AxStates`.
pub fn map_states(f: &AxStateFacts) -> AxStates {
    AxStates {
        focused: f.focused,
        focusable: f.focusable,
        enabled: f.enabled,
        visible: f.visible,
        selected: f.selected,
        checked: f.checked,
        checkable: f.checkable,
        expanded: f.expanded,
        editable: f.editable,
    }
}

/// macOS `(checkable, checked)` from the normalized role and its `AXValue` as an integer. A
/// checkbox/radio/switch exposes `AXValue` as `0` (off) or `1` (on); a mixed/indeterminate
/// checkbox reports some other value (AppKit's mixed state is not `0`/`1` — its exact AX
/// encoding, `2`/`-1`/…, is deliberately not relied on here). Claims `checkable` ONLY for a
/// determinate `0`/`1` (the #170 invariant); every other value, and an unread `None`, →
/// `(false, false)`, so a mixed or unreadable box matches neither `condition:"checked"` nor
/// `"unchecked"`. A macOS `NSSwitch` reports role `AXCheckBox`, so `CheckBox` covers switches.
pub fn checkable_checked(role: AxRole, ax_value: Option<i64>) -> (bool, bool) {
    match role {
        AxRole::CheckBox | AxRole::RadioButton => match ax_value {
            Some(1) => (true, true),
            Some(0) => (true, false),
            _ => (false, false),
        },
        _ => (false, false),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::AxRole;

    #[test]
    fn maps_common_ax_roles() {
        assert_eq!(map_role("AXButton", None), AxRole::Button);
        assert_eq!(map_role("AXCheckBox", None), AxRole::CheckBox);
        assert_eq!(map_role("AXTextField", None), AxRole::TextField);
        assert_eq!(map_role("AXTextArea", None), AxRole::TextArea);
        assert_eq!(map_role("AXStaticText", None), AxRole::Label);
        assert_eq!(map_role("AXWindow", None), AxRole::Window);
    }

    #[test]
    fn unmapped_role_is_other() {
        assert_eq!(map_role("AXRuler", None), AxRole::Other);
        assert_eq!(map_role("", None), AxRole::Other);
    }

    #[test]
    fn maps_states() {
        let f = AxStateFacts {
            enabled: true,
            focused: true,
            editable: true,
            ..Default::default()
        };
        let s = map_states(&f);
        assert!(s.enabled && s.focused && s.editable);
        assert!(!s.checked);
    }

    #[test]
    fn maps_additional_ax_roles() {
        assert_eq!(map_role("AXRadioButton", None), AxRole::RadioButton);
        assert_eq!(map_role("AXGroup", None), AxRole::Group);
        assert_eq!(map_role("AXMenu", None), AxRole::Menu);
        assert_eq!(map_role("AXMenuItem", None), AxRole::MenuItem);
        assert_eq!(map_role("AXMenuBar", None), AxRole::MenuBar);
        assert_eq!(map_role("AXImage", None), AxRole::Image);
        assert_eq!(map_role("AXLink", None), AxRole::Link);
        assert_eq!(map_role("AXSlider", None), AxRole::Slider);
        assert_eq!(map_role("AXComboBox", None), AxRole::ComboBox);
        assert_eq!(map_role("AXPopUpButton", None), AxRole::ComboBox);
        assert_eq!(map_role("AXList", None), AxRole::List);
        assert_eq!(map_role("AXRow", None), AxRole::ListItem);
        assert_eq!(map_role("AXCell", None), AxRole::Cell);
        assert_eq!(map_role("AXToolbar", None), AxRole::Toolbar);
        assert_eq!(map_role("AXTabGroup", None), AxRole::TabList);
        assert_eq!(map_role("AXRadioGroup", None), AxRole::Group);
        assert_eq!(map_role("AXProgressIndicator", None), AxRole::ProgressBar);
        assert_eq!(map_role("AXScrollBar", None), AxRole::ScrollBar);
    }

    #[test]
    fn checkable_fact_maps_through() {
        let f = AxStateFacts {
            checkable: true,
            checked: true,
            ..Default::default()
        };
        assert!(map_states(&f).checkable && map_states(&f).checked);
        assert!(!map_states(&AxStateFacts::default()).checkable);
    }

    #[test]
    fn checkable_and_checked_are_independent_fields() {
        // checkable != checked — a fixture like this catches a swapped-field bug that
        // `checkable_fact_maps_through`'s checkable+checked-together fixture cannot.
        let f = AxStateFacts {
            checkable: true,
            checked: false,
            ..Default::default()
        };
        let s = map_states(&f);
        assert!(s.checkable && !s.checked);
    }

    #[test]
    fn visible_and_selected_and_checked_map() {
        let f = AxStateFacts {
            visible: true,
            selected: true,
            checked: true,
            expanded: true,
            ..Default::default()
        };
        let s = map_states(&f);
        assert!(s.visible && s.selected && s.checked && s.expanded);
        assert!(!s.enabled && !s.focused && !s.focusable && !s.editable);
    }

    #[test]
    fn role_tokens_have_no_duplicate_strings() {
        for (i, (token, _)) in ROLE_TOKENS.iter().enumerate() {
            assert!(
                !ROLE_TOKENS[i + 1..].iter().any(|(other, _)| other == token),
                "{token} listed twice"
            );
        }
    }

    #[test]
    fn map_matches_declared_column() {
        use glass_core::role_support::{support, AxBackend, RoleSupport};
        for role in AxRole::ALL {
            // The outline-row/table-row split happens outside the token table (see
            // `map_role`'s subrole check), so table membership alone would miss TreeItem.
            let mapped = ROLE_TOKENS.iter().any(|(_, r)| *r == role)
                || map_role("AXRow", Some("AXOutlineRow")) == role;
            match support(role, AxBackend::MacOs).expect("declared in ROLE_SUPPORT") {
                RoleSupport::Mapped => {
                    assert!(mapped, "{role:?} declared Mapped but no AX role maps to it")
                }
                RoleSupport::NotApplicable(_) | RoleSupport::Gap(_) => assert!(
                    !mapped,
                    "{role:?} is produced by an AX role but the matrix does not declare it"
                ),
            }
        }
    }

    #[test]
    fn subrole_is_read_only_where_it_disambiguates() {
        // Reading AXSubrole costs an AX IPC round-trip per node, so the reader takes it only
        // for the base roles where AppKit uses a subrole to say what an element really is.
        for role in ["AXWindow", "AXTextField", "AXRow", "AXGroup", "AXUnknown"] {
            assert!(subrole_matters(role), "{role} must be gated in");
        }
        for role in ["AXButton", "AXStaticText", "AXCell", "AXImage", ""] {
            assert!(
                !subrole_matters(role),
                "{role} must not pay for a subrole read"
            );
        }
    }

    #[test]
    fn a_subrole_that_does_not_disambiguate_leaves_the_mapping_unchanged() {
        // AXRow/AXOutlineRow is the one subrole combination that changes the mapped role (see
        // `an_outline_row_is_a_tree_item_a_plain_row_is_a_list_item`); every other role ignores
        // whatever subrole the reader happened to pass.
        assert_eq!(map_role("AXButton", Some("AXAnything")), AxRole::Button);
    }

    #[test]
    fn observed_tokens_map() {
        // Every token here was observed in a stock-app probe run. Nothing is mapped that a
        // real app did not emit.
        assert_eq!(map_role("AXOutline", None), AxRole::Tree);
        assert_eq!(map_role("AXScrollArea", None), AxRole::Group);
        assert_eq!(map_role("AXSplitGroup", None), AxRole::Group);
        assert_eq!(map_role("AXSplitter", None), AxRole::Separator);
        assert_eq!(map_role("AXHeading", None), AxRole::Heading);
        assert_eq!(map_role("AXMenuButton", None), AxRole::Button);
    }

    #[test]
    fn an_outline_row_is_a_tree_item_a_plain_row_is_a_list_item() {
        assert_eq!(map_role("AXRow", Some("AXOutlineRow")), AxRole::TreeItem);
        assert_eq!(map_role("AXRow", None), AxRole::ListItem);
        assert_eq!(map_role("AXRow", Some("AXTableRow")), AxRole::ListItem);
    }

    #[test]
    fn an_unobserved_token_stays_unmapped() {
        // AXTable never appeared in any probed app — the lists are outlines. No arm.
        assert_eq!(map_role("AXTable", None), AxRole::Other);
        // A column is a table sub-structure with no counterpart in the normalized set.
        assert_eq!(map_role("AXColumn", None), AxRole::Other);
    }

    #[test]
    fn macos_checkable_checked_only_claims_a_determinate_toggle() {
        use AxRole::*;
        assert_eq!(checkable_checked(CheckBox, Some(1)), (true, true));
        assert_eq!(checkable_checked(CheckBox, Some(0)), (true, false));
        assert_eq!(checkable_checked(RadioButton, Some(1)), (true, true));
        assert_eq!(checkable_checked(RadioButton, Some(0)), (true, false));
        // A mixed/indeterminate value (whatever AppKit's AX encoding — 2, -1, …), an unread
        // value, or a non-checkable role → neither (the #170 invariant): a mixed or unreadable
        // box must not match `condition:"unchecked"`.
        assert_eq!(checkable_checked(CheckBox, Some(2)), (false, false));
        assert_eq!(checkable_checked(CheckBox, Some(-1)), (false, false));
        assert_eq!(checkable_checked(CheckBox, None), (false, false));
        assert_eq!(checkable_checked(Button, Some(1)), (false, false));
        assert_eq!(checkable_checked(Slider, Some(1)), (false, false));
    }
}
