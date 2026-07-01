//! Pure mapping from AXUIElement role strings + gathered state facts into glass's
//! normalized `AxRole`/`AxStates`. No AXUIElement/objc2 calls — unit-tested directly on
//! the Linux dev box. AX role strings (`kAXRoleAttribute`'s value) are the stable
//! `"AXButton"`/`"AXTextField"`/... constants; the reader passes the string so this
//! module needs no macOS-only dependency.

use glass_core::{AxRole, AxStates};

/// Map an AX role string to the normalized `AxRole`; unmapped roles become
/// `AxRole::Other` (the reader keeps the original string in `raw_role`). AX role
/// strings are canonical, so the match is case-sensitive.
pub fn map_role(ax_role: &str) -> AxRole {
    match ax_role {
        "AXButton" => AxRole::Button,
        "AXCheckBox" => AxRole::CheckBox,
        "AXRadioButton" => AxRole::RadioButton,
        "AXRadioGroup" => AxRole::Group,
        "AXTextField" => AxRole::TextField,
        "AXTextArea" => AxRole::TextArea,
        "AXStaticText" => AxRole::Label,
        "AXWindow" => AxRole::Window,
        "AXGroup" => AxRole::Group,
        "AXMenu" => AxRole::Menu,
        "AXMenuItem" => AxRole::MenuItem,
        "AXMenuBar" => AxRole::MenuBar,
        "AXImage" => AxRole::Image,
        "AXLink" => AxRole::Link,
        "AXSlider" => AxRole::Slider,
        "AXComboBox" => AxRole::ComboBox,
        "AXPopUpButton" => AxRole::ComboBox,
        "AXList" => AxRole::List,
        "AXRow" => AxRole::ListItem,
        "AXCell" => AxRole::Cell,
        "AXToolbar" => AxRole::Toolbar,
        "AXTabGroup" => AxRole::TabList,
        "AXProgressIndicator" => AxRole::ProgressBar,
        "AXScrollBar" => AxRole::ScrollBar,
        _ => AxRole::Other,
    }
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
        expanded: f.expanded,
        editable: f.editable,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::AxRole;

    #[test]
    fn maps_common_ax_roles() {
        assert_eq!(map_role("AXButton"), AxRole::Button);
        assert_eq!(map_role("AXCheckBox"), AxRole::CheckBox);
        assert_eq!(map_role("AXTextField"), AxRole::TextField);
        assert_eq!(map_role("AXTextArea"), AxRole::TextArea);
        assert_eq!(map_role("AXStaticText"), AxRole::Label);
        assert_eq!(map_role("AXWindow"), AxRole::Window);
    }

    #[test]
    fn unmapped_role_is_other() {
        assert_eq!(map_role("AXRuler"), AxRole::Other);
        assert_eq!(map_role(""), AxRole::Other);
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
        assert_eq!(map_role("AXRadioButton"), AxRole::RadioButton);
        assert_eq!(map_role("AXGroup"), AxRole::Group);
        assert_eq!(map_role("AXMenu"), AxRole::Menu);
        assert_eq!(map_role("AXMenuItem"), AxRole::MenuItem);
        assert_eq!(map_role("AXMenuBar"), AxRole::MenuBar);
        assert_eq!(map_role("AXImage"), AxRole::Image);
        assert_eq!(map_role("AXLink"), AxRole::Link);
        assert_eq!(map_role("AXSlider"), AxRole::Slider);
        assert_eq!(map_role("AXComboBox"), AxRole::ComboBox);
        assert_eq!(map_role("AXPopUpButton"), AxRole::ComboBox);
        assert_eq!(map_role("AXList"), AxRole::List);
        assert_eq!(map_role("AXRow"), AxRole::ListItem);
        assert_eq!(map_role("AXCell"), AxRole::Cell);
        assert_eq!(map_role("AXToolbar"), AxRole::Toolbar);
        assert_eq!(map_role("AXTabGroup"), AxRole::TabList);
        assert_eq!(map_role("AXRadioGroup"), AxRole::Group);
        assert_eq!(map_role("AXProgressIndicator"), AxRole::ProgressBar);
        assert_eq!(map_role("AXScrollBar"), AxRole::ScrollBar);
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
}
