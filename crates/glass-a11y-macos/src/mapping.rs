#![forbid(unsafe_code)]
//! Pure mapping from AXUIElement role strings + gathered state facts into glass's
//! normalized `AxRole`/`AxStates`. No AXUIElement/objc2 calls — unit-tested directly on any
//! host. AX role strings (`kAXRoleAttribute`'s value) are the stable
//! `"AXButton"`/`"AXTextField"`/... constants; the reader passes the string so this
//! module needs no macOS-only dependency.

use glass_core::{AxRole, AxStates, normalize_description};

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
    // The root of a web engine's subtree.
    ("AXWebArea", AxRole::Document),
];

/// Subroles that decide a role, and the base roles that can carry one.
///
/// A switch is the case: measured on macOS 26.5, an AppKit `NSSwitch` reports `AXButton` and a
/// SwiftUI/system switch reports `AXCheckBox`, both with subrole `AXSwitch` — the base role varies
/// by toolkit, the subrole does not.
///
/// The carrying roles live here rather than in a second list so [`subrole_matters`] cannot drift
/// from the mapping: a subrole added with no base role to carry it would otherwise be a no-op the
/// whole suite endorses.
const SUBROLE_TOKENS: &[(&str, AxRole, &[&str])] = &[(
    "AXSwitch",
    AxRole::ToggleButton,
    &["AXButton", "AXCheckBox"],
)];

/// Whether the reader should read this base role's `AXSubrole`.
///
/// Each subrole read is an AX IPC round-trip on every matching node, so the gate is exactly the
/// roles whose subrole [`map_role`] consults: `AXRow`, where `AXOutlineRow` separates an outline row
/// ([`AxRole::TreeItem`]) from a plain table row ([`AxRole::ListItem`]), plus whatever carries a
/// [`SUBROLE_TOKENS`] entry.
///
/// `AXButton` is the expensive entry — buttons are the commonest interactive node. Measured on macOS
/// 26.5 against a settings-style app: 48 of 359 nodes gated, walk wall-clock 42.8ms to 45.4ms.
///
/// AppKit gives other base roles subroles too (an `AXWindow` is a plain window or a dialog or a
/// sheet, an `AXTextField` may be a search field), and they belong here as soon as something maps
/// them — until then reading them would spend a round-trip per node on a value nothing reads.
pub fn subrole_matters(ax_role: &str) -> bool {
    ax_role == "AXRow"
        || SUBROLE_TOKENS
            .iter()
            .any(|(_, _, bases)| bases.contains(&ax_role))
}

/// Map an AX role string, plus its `AXSubrole` when the reader took one, to the normalized
/// `AxRole`; unmapped roles become `AxRole::Other` (the reader keeps the token in `raw_role`).
pub fn map_role(ax_role: &str, subrole: Option<&str>) -> AxRole {
    // A row's subrole is what separates an outline row from a table row; AppKit reports both
    // as AXRow.
    if ax_role == "AXRow" && subrole == Some("AXOutlineRow") {
        return AxRole::TreeItem;
    }
    // A switch's subrole outranks its base role, which is AXButton or AXCheckBox depending on the
    // toolkit that drew it.
    if let Some(sub) = subrole
        && let Some((_, role, _)) = SUBROLE_TOKENS
            .iter()
            .find(|(token, _, bases)| *token == sub && bases.contains(&ax_role))
    {
        return *role;
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
    pub secure: bool,
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
        secure: f.secure,
    }
}

/// Whether this role can carry a checked state, and so whether the reader should spend an AX
/// round-trip reading its `AXValue`.
///
/// One list, not two: the reader gates its read on the same predicate [`checkable_checked`] judges
/// with, because a second copy could be dropped with every test still green.
pub fn role_carries_checked(role: AxRole) -> bool {
    matches!(
        role,
        AxRole::CheckBox | AxRole::RadioButton | AxRole::ToggleButton
    )
}

/// macOS `(checkable, checked)` from the normalized role and its `AXValue` as an integer. A
/// checkbox/radio/switch exposes `AXValue` as `0` (off) or `1` (on); a mixed/indeterminate
/// checkbox reports some other value (AppKit's mixed state is not `0`/`1` — its exact AX
/// encoding, `2`/`-1`/…, is deliberately not relied on here). Claims `checkable` ONLY for a
/// determinate `0`/`1` (the #170 invariant); every other value, and an unread `None`, →
/// `(false, false)`, so a mixed or unreadable box matches neither `condition:"checked"` nor
/// `"unchecked"`. `ToggleButton` is in the list because that is what a switch maps to, and the
/// AppKit variant — an `AXButton` — carried no checked state at all before that.
pub fn checkable_checked(role: AxRole, ax_value: Option<i64>) -> (bool, bool) {
    if !role_carries_checked(role) {
        return (false, false);
    }
    match ax_value {
        Some(1) => (true, true),
        Some(0) => (true, false),
        _ => (false, false),
    }
}

/// A node's `name`: its `AXTitle`, else its `AXDescription` (`setAccessibilityLabel` surfaces as
/// `AXDescription`). Never `AXValue` — volatile, and the name is half the `AxTarget` fingerprint
/// `set_value`/`invoke` re-walk against.
///
/// The description arrives as a reader because it costs an AX round-trip and a titled node never
/// needs it.
///
/// The one place the precedence lives: [`labels`] delegates its name slot here rather than
/// restating it, so the walk and the two fingerprint sites cannot rank the two attributes
/// differently and reject an element that never moved. The reads themselves are still spelled at
/// each site.
///
/// Precondition: both labels arrive already empty-filtered — the reader's `read_label` folds `""`.
/// A whitespace-only label is a name, matching the Windows and Linux readers' `nonempty`; the
/// Android reader deliberately differs, counting a blank label as absent.
pub fn node_name(
    title: Option<String>,
    read_description: impl FnOnce() -> Option<String>,
) -> Option<String> {
    title.or_else(read_description)
}

/// A node's `(name, description)`, decided together because which attribute is left to describe a
/// node depends on which one named it.
///
/// `AXDescription` costs at most one read: [`node_name`] spends it only on an untitled node, and
/// the description slot only on a titled node with no `AXHelp`. Hoisting that read above the
/// decision leaves every output byte-identical and buys one extra round-trip on every titled node
/// that has help text, so only a read count catches the edit. The reader is one-shot: whichever
/// slot spends it, the other finds it gone.
///
/// The returned description is normalized — blank, or a repeat of `name`, becomes `None`. An
/// `AXHelp` that normalizes away does not fall back to `AXDescription`, because retrying it would
/// spend the read this gate exists to save.
pub fn labels(
    title: Option<String>,
    read_description: impl FnOnce() -> Option<String>,
    read_help: impl FnOnce() -> Option<String>,
) -> (Option<String>, Option<String>) {
    let mut unread_description = Some(read_description);
    let name = node_name(title, || unread_description.take().and_then(|read| read()));
    let secondary = read_help().or_else(|| unread_description.take().and_then(|read| read()));
    let description = secondary.and_then(|raw| normalize_description(&raw, name.as_deref()));
    (name, description)
}

#[cfg(test)]
mod tests {
    use std::cell::Cell;

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
    fn a_web_area_is_a_document() {
        assert_eq!(map_role("AXWebArea", None), AxRole::Document);
    }

    #[test]
    fn a_switch_is_a_togglebutton_whichever_base_role_carries_it() {
        // `AXToggle` is deliberately absent: AppKit documents it for on/off *buttons*, and no probe
        // has reported it, so mapping it would reclassify ordinary toggle buttons on a guess.
        assert_eq!(map_role("AXButton", Some("AXSwitch")), AxRole::ToggleButton);
        assert_eq!(
            map_role("AXCheckBox", Some("AXSwitch")),
            AxRole::ToggleButton
        );
    }

    #[test]
    fn a_plain_button_or_checkbox_keeps_its_role() {
        assert_eq!(map_role("AXButton", None), AxRole::Button);
        assert_eq!(map_role("AXCheckBox", None), AxRole::CheckBox);
        // A subrole nothing maps must not disturb the base role — AppKit puts several on buttons.
        assert_eq!(map_role("AXButton", Some("AXZoomButton")), AxRole::Button);
        assert_eq!(map_role("AXButton", Some("AXToggle")), AxRole::Button);
        // And a mapped subrole on a base role that does not carry it is not a switch either.
        assert_eq!(map_role("AXRow", Some("AXSwitch")), AxRole::ListItem);
        assert_eq!(
            map_role("AXCheckBox", Some("AXSomethingElse")),
            AxRole::CheckBox
        );
    }

    #[test]
    fn the_reader_and_the_judgement_agree_on_which_roles_carry_checked() {
        // Quantified over every role: a hand-picked list cannot detect the divergence the shared
        // predicate exists to prevent.
        for role in AxRole::ALL {
            assert_eq!(
                role_carries_checked(role),
                checkable_checked(role, Some(1)).0,
                "{role:?} is judged by one and not the other"
            );
        }
        assert!(role_carries_checked(AxRole::ToggleButton));
        assert!(!role_carries_checked(AxRole::Button));
    }

    #[test]
    fn an_appkit_switch_gains_the_checked_state_it_never_had() {
        // It reports AXButton, so before this it mapped to Button — and `checkable_checked` and the
        // reader's AXValue read both key off the role, so no checked state was read at all.
        assert_eq!(
            checkable_checked(AxRole::ToggleButton, Some(1)),
            (true, true)
        );
        assert_eq!(
            checkable_checked(AxRole::ToggleButton, Some(0)),
            (true, false)
        );
        // The #170 invariant still holds for it: indeterminate claims neither.
        assert_eq!(
            checkable_checked(AxRole::ToggleButton, Some(2)),
            (false, false)
        );
        assert_eq!(
            checkable_checked(AxRole::ToggleButton, None),
            (false, false)
        );
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
        use glass_core::role_support::{AxBackend, RoleSupport, support};
        for role in AxRole::ALL {
            // Two roles are produced outside the role-token table (see `map_role`'s subrole
            // checks): TreeItem from an outline row, and ToggleButton from a switch's subrole.
            // Table membership alone would call both unmapped.
            // Each clause calls the mapper rather than reading a table, so a cell stays honest
            // when the wiring changes: a subrole lookup deleted from `map_role` must fail this,
            // not merely fail the dedicated test.
            let mapped = ROLE_TOKENS.iter().any(|(_, r)| *r == role)
                || map_role("AXRow", Some("AXOutlineRow")) == role
                || SUBROLE_TOKENS.iter().any(|(sub, _, bases)| {
                    bases.iter().any(|base| map_role(base, Some(sub)) == role)
                });
            match support(role, AxBackend::MacOs).expect("declared in ROLE_SUPPORT") {
                RoleSupport::Mapped => {
                    assert!(mapped, "{role:?} declared Mapped but no AX role maps to it")
                }
                cell => {
                    assert!(
                        !mapped,
                        "{role:?} is produced by an AX role but the matrix does not declare it"
                    );
                    // The named AX role must not resolve to the very role the cell says is out
                    // of reach. No subrole is passed: a cell naming a bare role is claiming that
                    // role's own mapping. Largely a second line behind the `!mapped` assertion
                    // above; a misspelt AX role resolves to `Other` and passes, which only an
                    // on-box read catches.
                    if let Some(token) = cell.named_token() {
                        assert_ne!(
                            map_role(token, None),
                            role,
                            "{role:?} is declared out of reach naming {token}, which maps to it"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn subrole_is_read_only_where_it_disambiguates() {
        // Reading AXSubrole costs an AX IPC round-trip per node, so the reader takes it only for
        // the base roles whose subrole `map_role` consults: a row (outline vs table) and the two
        // that can carry AXSwitch — an AppKit switch is an AXButton, a SwiftUI one an AXCheckBox.
        for role in ["AXRow", "AXButton", "AXCheckBox"] {
            assert!(subrole_matters(role), "{role} must be gated in");
        }
        // AXWindow/AXTextField/AXGroup/AXUnknown do carry meaningful subroles, but nothing maps
        // them, so the read would be paid for and thrown away.
        for role in [
            "AXWindow",
            "AXTextField",
            "AXGroup",
            "AXUnknown",
            "AXStaticText",
            "AXCell",
            "AXImage",
            "",
        ] {
            assert!(
                !subrole_matters(role),
                "{role} must not pay for a subrole read"
            );
        }
    }

    #[test]
    fn a_subrole_that_does_not_disambiguate_leaves_the_mapping_unchanged() {
        // A subrole outside `SUBROLE_TOKENS` changes nothing about the mapped role (see
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

    /// A label reader that counts its calls. Which attributes `labels` reads is invisible in its
    /// return value, so a test that only inspects the tuple cannot tell the gated reader from one
    /// that reads everything.
    fn counted<'a>(
        text: Option<&'a str>,
        calls: &'a Cell<usize>,
    ) -> impl FnOnce() -> Option<String> + 'a {
        move || {
            calls.set(calls.get() + 1);
            text.map(str::to_string)
        }
    }

    /// The same reader without the counter, for the tests that judge only the returned pair.
    fn reader(text: Option<&str>) -> impl FnOnce() -> Option<String> + '_ {
        move || text.map(str::to_string)
    }

    #[test]
    fn a_titled_node_is_named_by_its_title_and_described_by_its_help() {
        let (name, description) = labels(
            Some("Save".to_string()),
            reader(Some("a description")),
            reader(Some("Saves the document")),
        );
        assert_eq!(name.as_deref(), Some("Save"));
        assert_eq!(description.as_deref(), Some("Saves the document"));
    }

    #[test]
    fn a_titled_node_without_help_is_described_by_its_description() {
        let (name, description) = labels(
            Some("Save".to_string()),
            reader(Some("Saves the document")),
            reader(None),
        );
        assert_eq!(name.as_deref(), Some("Save"));
        assert_eq!(description.as_deref(), Some("Saves the document"));
    }

    #[test]
    fn an_untitled_node_is_named_by_its_description() {
        let (name, description) = labels(None, reader(Some("Close")), reader(Some("Closes it")));
        assert_eq!(name.as_deref(), Some("Close"));
        assert_eq!(description.as_deref(), Some("Closes it"));
    }

    #[test]
    fn a_titled_node_with_help_never_reads_its_description() {
        // The gate this extraction exists to keep verified. Hoist the read out of the decision
        // (`let d = read_description();`, then use `d` in both slots) and every output stays
        // byte-identical, so this count is the only assertion that can fail.
        let reads = Cell::new(0);
        let (name, description) = labels(
            Some("Save".to_string()),
            counted(Some("a description"), &reads),
            reader(Some("Saves the document")),
        );
        assert_eq!(reads.get(), 0, "AXDescription must not be read at all here");
        assert_eq!(name.as_deref(), Some("Save"));
        assert_eq!(description.as_deref(), Some("Saves the document"));
    }

    #[test]
    fn a_whitespace_only_help_is_dropped_without_retrying_the_description() {
        // The description is present and must stay unread: the fallback fires on an absent
        // `AXHelp`, not on one that normalizes away.
        let (name, description) = labels(
            Some("Save".to_string()),
            reader(Some("Saves the document")),
            reader(Some("   ")),
        );
        assert_eq!(name.as_deref(), Some("Save"));
        assert_eq!(description, None);
    }

    #[test]
    fn a_help_that_repeats_the_name_is_dropped_without_retrying_the_description() {
        let (name, description) = labels(
            Some("Save".to_string()),
            reader(Some("Saves the document")),
            reader(Some("Save")),
        );
        assert_eq!(name.as_deref(), Some("Save"));
        assert_eq!(description, None);
    }

    #[test]
    fn a_whitespace_only_title_still_names_the_node() {
        // Pinned rather than trimmed: the Android reader counts a blank label as absent, so this
        // is a live divergence between backends, not a rule this function gets to settle.
        let (name, description) = labels(
            Some("   ".to_string()),
            reader(Some("a description")),
            reader(Some("Closes it")),
        );
        assert_eq!(name.as_deref(), Some("   "));
        assert_eq!(description.as_deref(), Some("Closes it"));
    }

    #[test]
    fn every_title_description_help_combination_lands_where_the_reader_put_it() {
        // Exhaustive over the eight present/absent combinations: a hand-picked sample cannot show
        // that an arm was dropped rather than merely unexercised. The ninth row pins
        // `normalize_description`'s trim at this seam, which a blankness-only lookalike would lose.
        let cases = [
            (Some("t"), Some("d"), Some("h"), Some("t"), Some("h")),
            (Some("t"), Some("d"), None, Some("t"), Some("d")),
            (Some("t"), None, Some("h"), Some("t"), Some("h")),
            (Some("t"), None, None, Some("t"), None),
            (None, Some("d"), Some("h"), Some("d"), Some("h")),
            (None, Some("d"), None, Some("d"), None),
            (None, None, Some("h"), None, Some("h")),
            (None, None, None, None, None),
            (Some("t"), None, Some("  h  "), Some("t"), Some("h")),
        ];
        for (title, description, help, want_name, want_description) in cases {
            let (name, got_description) =
                labels(title.map(str::to_string), reader(description), reader(help));
            assert_eq!(
                (name.as_deref(), got_description.as_deref()),
                (want_name, want_description),
                "title={title:?} description={description:?} help={help:?}"
            );
        }
    }

    #[test]
    fn the_fingerprint_name_and_the_walked_name_agree_on_every_combination() {
        // `set_value`/`invoke` re-derive a name through `node_name` to fingerprint the element
        // `walk` recorded through `labels`; a divergence would reject an element that never moved.
        // Blank labels are in the loop because a trim added to one side and not the other is the
        // realistic way the two drift.
        for title in [Some("t"), Some("   "), None] {
            for description in [Some("d"), Some("  "), None] {
                for help in [Some("h"), None] {
                    let walked =
                        labels(title.map(str::to_string), reader(description), reader(help)).0;
                    let fingerprint = node_name(title.map(str::to_string), reader(description));
                    assert_eq!(
                        walked, fingerprint,
                        "title={title:?} description={description:?} help={help:?}"
                    );
                }
            }
        }
    }

    #[test]
    fn a_titled_node_needs_no_description_read_to_be_named() {
        // `set_value` and `invoke` pay this read on the element they are about to act on.
        let reads = Cell::new(0);
        assert_eq!(
            node_name(Some("Save".to_string()), counted(Some("d"), &reads)).as_deref(),
            Some("Save")
        );
        assert_eq!(reads.get(), 0);
    }
}
