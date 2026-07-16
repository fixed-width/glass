//! Pure mapping from AT-SPI `Role`/`State` into glass's normalized
//! `AxRole`/`AxStates`. No D-Bus — unit-tested directly.

use atspi_common::{Role, State, StateSet};
use glass_core::{AxRole, AxStates};

/// Map an AT-SPI role to the normalized `AxRole`; unmapped roles become
/// `AxRole::Other` (the caller keeps the raw AT-SPI role-name string alongside).
pub(crate) fn map_role(role: Role) -> AxRole {
    match role {
        Role::Application => AxRole::Application,
        Role::Frame | Role::Window | Role::InternalFrame => AxRole::Window,
        Role::Dialog | Role::Alert | Role::FileChooser | Role::ColorChooser | Role::FontChooser => {
            AxRole::Dialog
        }
        Role::Panel
        | Role::Filler
        | Role::Viewport
        | Role::SplitPane
        | Role::Grouping
        | Role::RootPane
        | Role::LayeredPane
        | Role::ScrollPane => AxRole::Group,
        Role::Button | Role::PushButtonMenu => AxRole::Button,
        Role::ToggleButton => AxRole::ToggleButton,
        Role::RadioButton | Role::RadioMenuItem => AxRole::RadioButton,
        Role::CheckBox | Role::CheckMenuItem => AxRole::CheckBox,
        Role::MenuBar => AxRole::MenuBar,
        Role::Menu | Role::PopupMenu => AxRole::Menu,
        Role::MenuItem | Role::TearoffMenuItem => AxRole::MenuItem,
        Role::Label | Role::Static => AxRole::Label,
        Role::Entry | Role::PasswordText | Role::Autocomplete | Role::Editbar => AxRole::TextField,
        Role::Text | Role::Paragraph | Role::DocumentText => AxRole::TextArea,
        Role::ComboBox => AxRole::ComboBox,
        Role::List | Role::ListBox => AxRole::List,
        Role::ListItem => AxRole::ListItem,
        Role::Table | Role::TreeTable => AxRole::Table,
        Role::TableCell => AxRole::Cell,
        Role::Tree => AxRole::Tree,
        Role::TreeItem => AxRole::TreeItem,
        Role::PageTabList => AxRole::TabList,
        Role::PageTab => AxRole::Tab,
        Role::ScrollBar => AxRole::ScrollBar,
        Role::Slider => AxRole::Slider,
        Role::SpinButton => AxRole::SpinButton,
        Role::ProgressBar | Role::LevelBar => AxRole::ProgressBar,
        Role::Image | Role::Icon => AxRole::Image,
        Role::Link => AxRole::Link,
        Role::Separator => AxRole::Separator,
        Role::ToolBar => AxRole::Toolbar,
        Role::StatusBar => AxRole::StatusBar,
        Role::Heading => AxRole::Heading,
        _ => AxRole::Other,
    }
}

/// Map an AT-SPI state set to the normalized `AxStates`.
pub(crate) fn map_states(states: &StateSet) -> AxStates {
    AxStates {
        focused: states.contains(State::Focused),
        focusable: states.contains(State::Focusable),
        enabled: states.contains(State::Enabled) || states.contains(State::Sensitive),
        visible: states.contains(State::Showing),
        selected: states.contains(State::Selected),
        checked: states.contains(State::Checked),
        checkable: states.contains(State::Checkable),
        expanded: states.contains(State::Expanded),
        editable: states.contains(State::Editable),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_roles_map() {
        assert_eq!(map_role(Role::Frame), AxRole::Window);
        assert_eq!(map_role(Role::Dialog), AxRole::Dialog);
        assert_eq!(map_role(Role::Button), AxRole::Button);
        assert_eq!(map_role(Role::CheckBox), AxRole::CheckBox);
        assert_eq!(map_role(Role::RadioButton), AxRole::RadioButton);
        assert_eq!(map_role(Role::Label), AxRole::Label);
        assert_eq!(map_role(Role::Entry), AxRole::TextField);
        assert_eq!(map_role(Role::MenuItem), AxRole::MenuItem);
        assert_eq!(map_role(Role::PageTabList), AxRole::TabList);
        assert_eq!(map_role(Role::Application), AxRole::Application);
    }

    #[test]
    fn unknown_role_is_other() {
        assert_eq!(map_role(Role::Calendar), AxRole::Other);
    }

    #[test]
    fn states_map_to_flags() {
        let s =
            StateSet::new(State::Focusable | State::Enabled | State::Sensitive | State::Focused);
        let m = map_states(&s);
        assert!(m.focusable && m.enabled && m.focused);
        assert!(!m.checked && !m.selected);
    }

    #[test]
    fn checked_and_editable_map() {
        let m = map_states(&StateSet::new(State::Checked | State::Editable));
        assert!(m.checked && m.editable);
        assert!(!m.focused);
    }

    #[test]
    fn checkable_from_atspi_state() {
        let on = StateSet::new(State::Checkable | State::Checked);
        let m = map_states(&on);
        assert!(m.checkable && m.checked);
        let plain = map_states(&StateSet::empty());
        assert!(!plain.checkable);
    }

    #[test]
    fn checkable_and_checked_are_independent_fields() {
        // Checkable but NOT checked — a fixture with checkable != checked catches a
        // swapped-field bug that `checkable_from_atspi_state`'s checkable+checked-together
        // fixture cannot.
        let m = map_states(&StateSet::new(State::Checkable));
        assert!(m.checkable && !m.checked);
    }
}
