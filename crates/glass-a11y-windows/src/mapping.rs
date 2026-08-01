//! Pure mapping from UI Automation control-type ids + gathered state facts into glass's
//! normalized `AxRole`/`AxStates`. No UIA calls — unit-tested directly on any host.
//! Control-type ids are the stable UIA `ControlTypeId` constants (50000..=50040); the reader
//! passes the numeric id so this module needs no `uiautomation` dependency.

use glass_core::{AxRole, AxStates};

/// Every documented UIA control type (the contiguous 50000..=50040 range), as
/// `(ControlTypeId, canonical name, mapped role)`.
///
/// The canonical name is the control type's documented English name — it is what the reader puts
/// in `raw_role`, identical on every machine, unlike UIA's localized control-type string. Every
/// documented id is listed, so a control type glass does *not* map to a role still reads as e.g.
/// `DataItem` rather than a bare number; only a vendor-defined or future id falls through to the
/// reader's numeric `UIA:<id>` form.
///
/// The role is `Some` only where glass maps the control type today; `None` means the node gets
/// [`AxRole::Other`] and is identified by its name alone.
pub const CONTROL_TYPES: &[(u32, &str, Option<AxRole>)] = &[
    (50000, "Button", Some(AxRole::Button)),
    (50001, "Calendar", None),
    (50002, "CheckBox", Some(AxRole::CheckBox)),
    (50003, "ComboBox", Some(AxRole::ComboBox)),
    (50004, "Edit", Some(AxRole::TextField)),
    (50005, "Hyperlink", Some(AxRole::Link)),
    (50006, "Image", Some(AxRole::Image)),
    (50007, "ListItem", Some(AxRole::ListItem)),
    (50008, "List", Some(AxRole::List)),
    (50009, "Menu", Some(AxRole::Menu)),
    (50010, "MenuBar", Some(AxRole::MenuBar)),
    (50011, "MenuItem", Some(AxRole::MenuItem)),
    (50012, "ProgressBar", Some(AxRole::ProgressBar)),
    (50013, "RadioButton", Some(AxRole::RadioButton)),
    (50014, "ScrollBar", Some(AxRole::ScrollBar)),
    (50015, "Slider", Some(AxRole::Slider)),
    (50016, "Spinner", Some(AxRole::SpinButton)),
    (50017, "StatusBar", Some(AxRole::StatusBar)),
    (50018, "Tab", Some(AxRole::TabList)),
    (50019, "TabItem", Some(AxRole::Tab)),
    (50020, "Text", Some(AxRole::Label)),
    (50021, "ToolBar", Some(AxRole::Toolbar)),
    (50022, "ToolTip", None),
    (50023, "Tree", Some(AxRole::Tree)),
    (50024, "TreeItem", Some(AxRole::TreeItem)),
    (50025, "Custom", None),
    (50026, "Group", Some(AxRole::Group)),
    (50027, "Thumb", None),
    (50028, "DataGrid", Some(AxRole::Table)),
    (50029, "DataItem", None),
    (50030, "Document", Some(AxRole::TextArea)),
    // A split button is an actionable button carrying a dropdown.
    (50031, "SplitButton", Some(AxRole::Button)),
    (50032, "Window", Some(AxRole::Window)),
    (50033, "Pane", Some(AxRole::Group)),
    // Header/HeaderItem are a grid's column-header bar and its individual column headers, not
    // document headings — deliberately unmapped; see `observed_column_headers_are_not_headings`.
    (50034, "Header", None),
    (50035, "HeaderItem", None),
    (50036, "Table", Some(AxRole::Table)),
    (50037, "TitleBar", None),
    (50038, "Separator", Some(AxRole::Separator)),
    (50039, "SemanticZoom", None),
    (50040, "AppBar", None),
];

/// The [`CONTROL_TYPES`] row for an id, shared by [`map_role`] and [`canonical_name`] so the two
/// can never disagree about which ids are known.
fn control_type(control_type_id: u32) -> Option<&'static (u32, &'static str, Option<AxRole>)> {
    CONTROL_TYPES
        .iter()
        .find(|(id, _, _)| *id == control_type_id)
}

/// Map a UIA `ControlTypeId` plus Toggle-pattern availability to the normalized `AxRole`.
///
/// UIA has no dedicated "toggle button" control type — a formatting-bar button (Bold, Italic,
/// ...) reports as a plain `Button` (50000) and carries the Toggle pattern instead, the same
/// pattern a `CheckBox` carries. A `RadioButton` does not: UIA documents the Toggle pattern as
/// never supported on a radio button, whose on/off is *selection* (SelectionItem), which is why
/// the reader's pattern gate leaves it out. `toggleable` is that pattern's *availability*
/// (see `StateFacts::checkable`), passed in because computing it means a live UIA call this
/// module cannot make. Only `Button` is affected: `CheckBox` (50002) and `RadioButton` (50013)
/// already have their own roles regardless of `toggleable`, and so do the other two control
/// types the reader fetches the pattern for — `MenuItem` (50011), which really can arrive
/// toggle-capable as a checkable menu entry, and `SplitButton` (50031).
///
/// A control type glass does not map — known or not — becomes `AxRole::Other` (the reader keeps
/// the control type's name in `raw_role`).
pub fn map_role(control_type_id: u32, toggleable: bool) -> AxRole {
    if control_type_id == 50000 && toggleable {
        return AxRole::ToggleButton;
    }
    control_type(control_type_id)
        .and_then(|(_, _, role)| *role)
        .unwrap_or(AxRole::Other)
}

/// The control type's canonical English name, or `None` for an id outside the documented range
/// (a vendor-defined or future control type).
pub fn canonical_name(control_type_id: u32) -> Option<&'static str> {
    control_type(control_type_id).map(|(_, name, _)| *name)
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

/// A UIA property the change subscription can register for.
///
/// A closed set rather than raw ids, so [`announcing_property`] and [`SELECTOR_PROPERTIES`] can
/// only name a property this enum carries; with a bare `u32` they could name any number at all.
/// Carrying a variant is not what registers it: `events.rs` builds its registration list from
/// those two declarations, so a variant neither of them names is not registered at all.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum WatchedProperty {
    Name,
    HasKeyboardFocus,
    /// Registered even though the WinForms controls probed on-box never sent it, reaching UI
    /// Automation through the legacy MSAA bridge; a WPF window probed the same way does announce
    /// it, so removing the registration would break the provider it already works for. Where it is
    /// not sent, a `condition: "enabled"` wait falls back to `glass_core`'s once-a-second forced
    /// re-read — latency, not a wrong answer.
    IsEnabled,
    IsOffscreen,
    /// A node's `value` where it comes from `ValuePattern`, which the reader reads for an Edit, a
    /// ComboBox or a Document.
    Value,
    /// The other source of a node's `value`: the reader reads a ProgressBar/Slider/Spinner
    /// position from `RangeValuePattern`, never from `ValuePattern`, and UIA reports a change to it
    /// under a property id of its own — so a `value_contains` wait on one of those is woken by this
    /// and never by [`Value`](WatchedProperty::Value).
    RangeValue,
    ExpandCollapseState,
    SelectionItemIsSelected,
    ToggleState,
}

impl WatchedProperty {
    /// Every variant, for the tests that cross-check this enum against UIA's own property ids.
    ///
    /// Not the registration list — `events.rs` derives that from [`announcing_property`] and
    /// [`SELECTOR_PROPERTIES`] — and nothing reads its order. Hand-written, and nothing makes a new
    /// variant appear here, so `all_lists_every_watched_property_exactly_once` catches an entry
    /// dropped or listed twice; a variant missing from it is one those cross-checks silently stop
    /// covering.
    pub const ALL: [WatchedProperty; 9] = [
        WatchedProperty::Name,
        WatchedProperty::HasKeyboardFocus,
        WatchedProperty::IsEnabled,
        WatchedProperty::IsOffscreen,
        WatchedProperty::Value,
        WatchedProperty::RangeValue,
        WatchedProperty::ExpandCollapseState,
        WatchedProperty::SelectionItemIsSelected,
        WatchedProperty::ToggleState,
    ];

    /// The stable UIA property id, transcribed here independently of `events.rs`'s `UIProperty`
    /// arms. Nothing calls this at run time: its only consumers are the tests that pair the two
    /// tables against each other (see `every_watched_property_has_a_distinct_id`). Numeric for the
    /// same reason [`CONTROL_TYPES`] is: this module compiles on every host, and `uiautomation` is
    /// a `cfg(windows)` dependency.
    pub const fn id(self) -> u32 {
        match self {
            WatchedProperty::Name => 30005,
            WatchedProperty::HasKeyboardFocus => 30008,
            WatchedProperty::IsEnabled => 30010,
            WatchedProperty::IsOffscreen => 30022,
            WatchedProperty::Value => 30045,
            WatchedProperty::RangeValue => 30047,
            WatchedProperty::ExpandCollapseState => 30070,
            WatchedProperty::SelectionItemIsSelected => 30079,
            WatchedProperty::ToggleState => 30086,
        }
    }
}

/// The properties a *selector* matches on, which no [`glass_core::ElementCondition`] names.
///
/// `name:` matches the element's name, and `value_contains:` its value — which the reader reads
/// from either the Value or the RangeValue pattern depending on the control, hence two value
/// properties (see [`RangeValue`](WatchedProperty::RangeValue)). A wait filtered on one of these
/// still has to wake when it changes, so `events.rs` registers these alongside the properties
/// [`announcing_property`] names.
pub const SELECTOR_PROPERTIES: [WatchedProperty; 3] = [
    WatchedProperty::Name,
    WatchedProperty::Value,
    WatchedProperty::RangeValue,
];

/// The property whose change announces `condition`, or `None` where a structure change does.
///
/// Exhaustive, so a new [`glass_core::ElementCondition`] fails to compile here rather than quietly
/// answering `None`. This is half of what the subscription registers — `events.rs` walks every
/// condition through this function and adds [`SELECTOR_PROPERTIES`] — so an arm naming the wrong
/// property registers the wrong property, and waits on that condition then fall back to
/// `glass_core`'s forced re-read: slow rather than wrong, which is the kind of regression that
/// ships.
pub const fn announcing_property(
    condition: glass_core::ElementCondition,
) -> Option<WatchedProperty> {
    use glass_core::ElementCondition as C;
    match condition {
        C::Appears | C::Disappears => None,
        C::Enabled | C::Disabled => Some(WatchedProperty::IsEnabled),
        C::Checked | C::Unchecked => Some(WatchedProperty::ToggleState),
        C::Selected | C::Unselected => Some(WatchedProperty::SelectionItemIsSelected),
        C::Expanded | C::Collapsed => Some(WatchedProperty::ExpandCollapseState),
        C::Focused => Some(WatchedProperty::HasKeyboardFocus),
        C::Visible | C::Hidden => Some(WatchedProperty::IsOffscreen),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn common_control_types_map() {
        assert_eq!(map_role(50000, false), AxRole::Button);
        assert_eq!(map_role(50002, false), AxRole::CheckBox);
        assert_eq!(map_role(50004, false), AxRole::TextField);
        assert_eq!(map_role(50011, false), AxRole::MenuItem);
        assert_eq!(map_role(50018, false), AxRole::TabList);
        assert_eq!(map_role(50019, false), AxRole::Tab);
        assert_eq!(map_role(50032, false), AxRole::Window);
        assert_eq!(map_role(50020, false), AxRole::Label);
        assert_eq!(map_role(50031, false), AxRole::Button); // SplitButton
    }
    #[test]
    fn unmapped_control_type_is_other() {
        assert_eq!(map_role(50001, false), AxRole::Other); // Calendar
        assert_eq!(map_role(99999, false), AxRole::Other);
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
    fn document_maps_from_an_observed_token() {
        // Observed on a stock text editor — see the probe test in
        // crates/glass-windows/tests/onbox.rs.
        assert_eq!(map_role(50030, false), AxRole::TextArea);
    }

    #[test]
    fn unobserved_control_types_stay_unmapped() {
        // DataItem is documented but no probed app emitted it — a data grid's rows arrived as
        // TreeItem. No observation, no arm.
        assert_eq!(map_role(50029, false), AxRole::Other);
        assert_eq!(canonical_name(50029), Some("DataItem"));
    }

    #[test]
    fn observed_column_headers_are_not_headings() {
        // Header/HeaderItem WERE observed (a stock task manager's and file manager's list
        // views), so this pin is not about missing evidence: UIA's Header is a grid's
        // column-header bar and HeaderItem one of its column headers, whereas every other
        // backend's Heading means a document/section heading (AT-SPI maps only Role::Heading
        // and lets ColumnHeader fall through). Mapping these would make `role:"heading"` match
        // a file list's column header here and nothing at all on Linux for the same app, so
        // they stay `Other` — identified by the name UIA already gives them.
        assert_eq!(map_role(50034, false), AxRole::Other);
        assert_eq!(map_role(50035, false), AxRole::Other);
    }

    #[test]
    // UIA has no switch control type (see `CONTROL_TYPES`), so a switch and a toggle button are the
    // same shape here — which is why glass normalizes a switch onto `ToggleButton` rather than a
    // `Switch` role this backend could not fill. What a real WinUI `ToggleSwitch` reports has not
    // been probed on this backend.
    fn toggleable_button_maps_to_toggle_button() {
        // Observed on a stock text editor's formatting bar: Button nodes carrying the Toggle
        // pattern (Bold, Italic, Strikethrough, Link, Clear formatting).
        assert_eq!(map_role(50000, true), AxRole::ToggleButton);
    }

    #[test]
    fn toggleable_checkbox_stays_checkbox() {
        // The toggle-capable rule is scoped to Button; CheckBox already has its own role and
        // the probe explicitly excluded it.
        assert_eq!(map_role(50002, true), AxRole::CheckBox);
    }

    #[test]
    fn toggleable_radio_button_stays_radio_button() {
        // Same scoping as CheckBox above: RadioButton already has its own role and the probe
        // explicitly excluded it too.
        assert_eq!(map_role(50013, true), AxRole::RadioButton);
    }

    #[test]
    fn toggleable_menu_item_stays_menu_item() {
        // Not hypothetical: the reader fetches the Toggle pattern for MenuItem, and a checkable
        // menu entry really does carry it, so `toggleable == true` reaches here in production.
        // "Only Button is affected" has to hold for it.
        assert_eq!(map_role(50011, true), AxRole::MenuItem);
    }

    #[test]
    fn toggleable_split_button_stays_button() {
        // The fourth control type in the reader's Toggle-pattern gate. A SplitButton maps to
        // Button either way — the toggle-capable rule is scoped to control type 50000 alone.
        assert_eq!(map_role(50031, true), AxRole::Button);
    }

    #[test]
    fn control_types_have_no_duplicate_ids() {
        for (i, (id, _, _)) in CONTROL_TYPES.iter().enumerate() {
            assert!(
                !CONTROL_TYPES[i + 1..]
                    .iter()
                    .any(|(other, _, _)| other == id),
                "control type {id} listed twice"
            );
        }
    }

    #[test]
    fn every_documented_control_type_is_listed() {
        // The documented range is contiguous; a hole would mean a real control type reporting
        // as an opaque number.
        for id in 50000..=50040u32 {
            assert!(
                canonical_name(id).is_some(),
                "control type {id} has no name"
            );
        }
        assert_eq!(CONTROL_TYPES.len(), 41);
    }

    #[test]
    fn canonical_name_is_stable_and_locale_free() {
        assert_eq!(canonical_name(50000), Some("Button"));
        assert_eq!(canonical_name(50036), Some("Table"));
        // Document and Header are what the reader puts in `raw_role` for two control types a
        // probe run has to be able to read back by name — Header especially, since it maps to
        // no role at all (see `observed_column_headers_are_not_headings`) and the name is the
        // only thing identifying it.
        assert_eq!(canonical_name(50030), Some("Document"));
        assert_eq!(canonical_name(50034), Some("Header"));
        assert_eq!(canonical_name(49999), None);
        assert_eq!(canonical_name(50041), None);
    }

    #[test]
    fn a_known_but_unmapped_control_type_still_has_a_name() {
        // DataItem is exactly the id the role-support matrix's remaining Windows Cell gap points
        // at, and what a probe run has to read; reporting it as `UIA:50029` would defeat that.
        assert_eq!(canonical_name(50029), Some("DataItem"));
        assert_eq!(map_role(50029, false), AxRole::Other);
    }

    #[test]
    fn map_matches_declared_column() {
        use glass_core::AxRole;
        use glass_core::role_support::{AxBackend, RoleSupport, support};
        for role in AxRole::ALL {
            // Only a row carrying `Some(role)` produces a role; a named-but-unmapped control
            // type yields `Other` and must not count as coverage. `ToggleButton` is the one
            // role no `CONTROL_TYPES` row produces on its own — it comes from `map_role`'s
            // Button-plus-Toggle-pattern rule — so it is checked by calling the real function
            // with the fact that triggers it, not by a hardcoded exception.
            let mapped = CONTROL_TYPES.iter().any(|(_, _, r)| *r == Some(role))
                || map_role(50000, true) == role;
            match support(role, AxBackend::Windows).expect("declared in ROLE_SUPPORT") {
                RoleSupport::Mapped => {
                    assert!(
                        mapped,
                        "{role:?} declared Mapped but no control type maps to it"
                    )
                }
                cell => {
                    assert!(
                        !mapped,
                        "{role:?} is produced by a control type but the matrix does not declare it"
                    );
                    // Windows is the one column where a named token is checked for existence,
                    // not just for what it maps to: `CONTROL_TYPES` carries every documented UIA
                    // control type by name, so a typo or an invented name fails here rather than
                    // resolving quietly to `Other` the way a bad AX role or widget class would.
                    if let Some(token) = cell.named_token() {
                        let named = CONTROL_TYPES
                            .iter()
                            .find(|(_, name, _)| *name == token)
                            .unwrap_or_else(|| {
                                panic!("{role:?} names {token}, not a UIA control type")
                            });
                        assert_ne!(
                            named.2,
                            Some(role),
                            "{role:?} is declared out of reach naming {token}, which maps to it"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn every_watched_property_has_a_distinct_id() {
        // `id()` feeds no registration; it exists to be a second, independent transcription of the
        // ids `events.rs`'s `uia_property` arms name, which is the only thing that makes the
        // pairing check there discriminating. Two variants sharing an id would let a swapped arm
        // satisfy that check — and the ids are hand-written, so nothing else would notice.
        let ids: Vec<u32> = WatchedProperty::ALL.iter().map(|p| p.id()).collect();
        let mut sorted = ids.clone();
        sorted.sort_unstable();
        sorted.dedup();
        assert_eq!(
            sorted.len(),
            ids.len(),
            "two WatchedProperty variants share an id: {ids:?}"
        );
    }

    /// One slot per [`WatchedProperty`] variant. Deliberately not `WatchedProperty::ALL.len()`:
    /// sizing the check off the array under test lets an entry dropped from the *end* shrink the
    /// check along with it and pass.
    const WATCHED_COUNT: usize = 9;

    /// Adding a [`WatchedProperty`] fails to compile in this match, which forces a decision about
    /// where it sits in [`WatchedProperty::ALL`]. Mirrors `glass_core`'s `condition_index`.
    const fn watched_index(p: WatchedProperty) -> usize {
        match p {
            WatchedProperty::Name => 0,
            WatchedProperty::HasKeyboardFocus => 1,
            WatchedProperty::IsEnabled => 2,
            WatchedProperty::IsOffscreen => 3,
            WatchedProperty::Value => 4,
            WatchedProperty::RangeValue => 5,
            WatchedProperty::ExpandCollapseState => 6,
            WatchedProperty::SelectionItemIsSelected => 7,
            WatchedProperty::ToggleState => 8,
        }
    }

    #[test]
    fn all_lists_every_watched_property_exactly_once() {
        // `ALL` is what drives the id cross-checks below, and it is hand-written: a variant
        // dropped from it is a variant those checks silently stop covering, and one listed twice
        // hides whichever entry it displaced.
        let mut seen = [false; WATCHED_COUNT];
        for p in WatchedProperty::ALL {
            let i = watched_index(p);
            assert!(!seen[i], "{p:?} appears twice in WatchedProperty::ALL");
            seen[i] = true;
        }
        assert!(
            seen.iter().all(|s| *s),
            "WatchedProperty::ALL is missing a variant"
        );
    }

    #[test]
    fn the_selector_properties_cover_name_and_both_value_sources() {
        // `name:` and `value_contains:` are selectors, not conditions, so no `ElementCondition`
        // names them: `SELECTOR_PROPERTIES` is the hand-written half of what `events.rs`
        // registers, and only this says what belongs in it. Two value properties, not one,
        // because the reader has two sources for a node's `value` — `ValuePattern` for an
        // editable control, `RangeValuePattern` for a ProgressBar/Slider/Spinner — and
        // `value_contains` matches whichever one the element carries.
        assert!(SELECTOR_PROPERTIES.contains(&WatchedProperty::Name));
        assert!(SELECTOR_PROPERTIES.contains(&WatchedProperty::Value));
        assert!(SELECTOR_PROPERTIES.contains(&WatchedProperty::RangeValue));
    }

    #[test]
    fn appearing_and_vanishing_are_structural_not_properties() {
        // The structure-changed handler covers these; claiming a property for them would mean
        // registering one that never fires for the case.
        assert_eq!(
            announcing_property(glass_core::ElementCondition::Appears),
            None
        );
        assert_eq!(
            announcing_property(glass_core::ElementCondition::Disappears),
            None
        );
    }
}
