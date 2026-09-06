use super::{AxRole, ElementInfo, SemanticState};

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
pub struct AxStateCoverage {
    pub enabled: bool,
    pub visible: bool,
    pub checkable: bool,
    pub checked: bool,
    pub selected: bool,
    pub expanded: bool,
    pub focused: bool,
    pub focusable: bool,
    pub editable: bool,
}

impl AxStateCoverage {
    pub const NONE: Self = Self {
        enabled: false,
        visible: false,
        checkable: false,
        checked: false,
        selected: false,
        expanded: false,
        focused: false,
        focusable: false,
        editable: false,
    };

    pub fn covers_selector_state(self, state: SemanticState) -> bool {
        match state {
            SemanticState::Enabled | SemanticState::Disabled => self.enabled,
            SemanticState::Visible | SemanticState::Hidden => self.visible,
            SemanticState::Checked | SemanticState::Unchecked => self.checkable && self.checked,
            SemanticState::Selected | SemanticState::Unselected => self.selected,
            SemanticState::Expanded | SemanticState::Collapsed => self.expanded,
            SemanticState::Focused => self.focused,
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionabilityVerdict {
    Passed,
    Failed,
    Unproven,
}

impl ActionabilityVerdict {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Passed => "passed",
            Self::Failed => "failed",
            Self::Unproven => "unproven",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionabilityCheckName {
    Unique,
    Enabled,
    Visible,
    FocusEligible,
    Stable,
    InWindow,
    NonOccluded,
    Focused,
    BackendFingerprint,
}

impl ActionabilityCheckName {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::Unique => "unique",
            Self::Enabled => "enabled",
            Self::Visible => "visible",
            Self::FocusEligible => "focus_eligible",
            Self::Stable => "stable",
            Self::InWindow => "in_window",
            Self::NonOccluded => "non_occluded",
            Self::Focused => "focused",
            Self::BackendFingerprint => "backend_fingerprint",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ActionabilitySource {
    SemanticResolution,
    NormalizedState,
    GeometrySamples,
    WindowGeometry,
    BackendHitProbe,
    BackendRewalk,
    ConfirmationPoll,
    LegacyCache,
}

impl ActionabilitySource {
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::SemanticResolution => "semantic_resolution",
            Self::NormalizedState => "normalized_state",
            Self::GeometrySamples => "geometry_samples",
            Self::WindowGeometry => "window_geometry",
            Self::BackendHitProbe => "backend_hit_probe",
            Self::BackendRewalk => "backend_rewalk",
            Self::ConfirmationPoll => "confirmation_poll",
            Self::LegacyCache => "legacy_cache",
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ActionabilityCheck {
    pub name: ActionabilityCheckName,
    pub verdict: ActionabilityVerdict,
    pub required: bool,
    pub source: ActionabilitySource,
}

impl ActionabilityCheck {
    pub const fn new(
        name: ActionabilityCheckName,
        verdict: ActionabilityVerdict,
        required: bool,
        source: ActionabilitySource,
    ) -> Self {
        Self {
            name,
            verdict,
            required,
            source,
        }
    }

    pub fn blocks_dispatch(self) -> bool {
        self.required && self.verdict == ActionabilityVerdict::Failed
    }
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ActionabilityReport {
    pub checks: Vec<ActionabilityCheck>,
}

impl ActionabilityReport {
    pub fn push(&mut self, check: ActionabilityCheck) {
        self.checks.push(check);
    }

    pub fn blocking(&self) -> Option<ActionabilityCheck> {
        self.checks
            .iter()
            .copied()
            .find(|check| check.blocks_dispatch())
    }

    pub(crate) fn evaluate_click(
        target: &ElementInfo,
        coverage: AxStateCoverage,
        stable: Option<bool>,
        window: (u32, u32),
        hit: PointerHit,
        legacy_id: bool,
        pointer: bool,
    ) -> Self {
        evaluate_actionability(
            if pointer {
                ActionabilityOperation::PointerClick
            } else {
                ActionabilityOperation::NativeClick
            },
            target,
            coverage,
            stable,
            window,
            hit,
            legacy_id,
        )
    }

    pub(crate) fn evaluate_set_value(
        target: &ElementInfo,
        coverage: AxStateCoverage,
        window: (u32, u32),
        legacy_id: bool,
    ) -> Self {
        evaluate_actionability(
            ActionabilityOperation::SetValue,
            target,
            coverage,
            None,
            window,
            PointerHit::Inconclusive,
            legacy_id,
        )
    }

    pub(crate) fn evaluate_targeted_type(
        target: &ElementInfo,
        coverage: AxStateCoverage,
        stable: Option<bool>,
        window: (u32, u32),
        pointer: bool,
    ) -> Self {
        evaluate_actionability(
            if pointer {
                ActionabilityOperation::PointerType
            } else {
                ActionabilityOperation::NativeType
            },
            target,
            coverage,
            stable,
            window,
            PointerHit::Inconclusive,
            false,
        )
    }

    /// Value/type resolution requires positive eligibility, even when unproven checks do not block.
    pub(crate) fn eligible_for_resolution(&self) -> bool {
        self.checks.iter().any(|check| {
            check.name == ActionabilityCheckName::FocusEligible
                && check.verdict == ActionabilityVerdict::Passed
        }) && self.blocking().is_none()
    }

    pub(crate) fn record_targeted_type_eligibility(
        &mut self,
        target: &ElementInfo,
        coverage: AxStateCoverage,
    ) {
        let check = ActionabilityCheck::new(
            ActionabilityCheckName::FocusEligible,
            targeted_type_eligibility(target, coverage),
            true,
            ActionabilitySource::NormalizedState,
        );
        if let Some(existing) = self
            .checks
            .iter_mut()
            .find(|existing| existing.name == ActionabilityCheckName::FocusEligible)
        {
            *existing = check;
        } else {
            let position = self
                .checks
                .iter()
                .position(|existing| existing.name == ActionabilityCheckName::Stable)
                .unwrap_or(self.checks.len());
            self.checks.insert(position, check);
        }
    }

    pub(crate) fn record_focus_confirmation(&mut self, coverage: AxStateCoverage, confirmed: bool) {
        self.push(ActionabilityCheck::new(
            ActionabilityCheckName::Focused,
            covered_flag(coverage.focused, confirmed),
            true,
            ActionabilitySource::ConfirmationPoll,
        ));
    }

    pub(crate) fn pass_backend_fingerprint(&mut self) {
        if let Some(check) = self
            .checks
            .iter_mut()
            .find(|check| check.name == ActionabilityCheckName::BackendFingerprint)
        {
            check.verdict = ActionabilityVerdict::Passed;
        }
    }

    pub(crate) fn fail_in_window(&mut self) {
        if let Some(check) = self
            .checks
            .iter_mut()
            .find(|check| check.name == ActionabilityCheckName::InWindow)
        {
            check.verdict = ActionabilityVerdict::Failed;
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerHit {
    Target,
    AcceptedAncestor,
    Other,
    Inconclusive,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ActionabilityOperation {
    NativeClick,
    PointerClick,
    NativeType,
    PointerType,
    SetValue,
}

impl ActionabilityOperation {
    fn uses_pointer(self) -> bool {
        matches!(self, Self::PointerClick | Self::PointerType)
    }

    fn requires_focus_eligibility(self) -> bool {
        matches!(self, Self::NativeType | Self::PointerType | Self::SetValue)
    }
}

fn covered_flag(covered: bool, value: bool) -> ActionabilityVerdict {
    if !covered {
        ActionabilityVerdict::Unproven
    } else if value {
        ActionabilityVerdict::Passed
    } else {
        ActionabilityVerdict::Failed
    }
}

fn targeted_type_eligibility(
    target: &ElementInfo,
    coverage: AxStateCoverage,
) -> ActionabilityVerdict {
    if matches!(target.role, AxRole::TextField | AxRole::TextArea)
        || (coverage.editable && target.states.editable)
    {
        ActionabilityVerdict::Passed
    } else if coverage.editable {
        ActionabilityVerdict::Failed
    } else {
        ActionabilityVerdict::Unproven
    }
}

fn set_value_eligibility(target: &ElementInfo, coverage: AxStateCoverage) -> ActionabilityVerdict {
    if matches!(
        target.role,
        AxRole::ComboBox | AxRole::Slider | AxRole::SpinButton
    ) || matches!(
        target.role,
        AxRole::ToggleButton | AxRole::RadioButton | AxRole::CheckBox
    ) || (coverage.editable && target.states.editable)
        || (coverage.checkable && target.states.checkable)
    {
        ActionabilityVerdict::Passed
    } else if coverage.editable && coverage.checkable {
        ActionabilityVerdict::Failed
    } else {
        ActionabilityVerdict::Unproven
    }
}

fn evaluate_actionability(
    operation: ActionabilityOperation,
    target: &ElementInfo,
    coverage: AxStateCoverage,
    stable: Option<bool>,
    window: (u32, u32),
    hit: PointerHit,
    legacy_id: bool,
) -> ActionabilityReport {
    let mut report = ActionabilityReport::default();
    let pointer_required = operation.uses_pointer() && !legacy_id;
    let state_required = !legacy_id;
    let source = |normal| {
        if legacy_id {
            ActionabilitySource::LegacyCache
        } else {
            normal
        }
    };

    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::Unique,
        if legacy_id {
            ActionabilityVerdict::Unproven
        } else {
            ActionabilityVerdict::Passed
        },
        !legacy_id,
        source(ActionabilitySource::SemanticResolution),
    ));
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::Enabled,
        covered_flag(coverage.enabled, target.states.enabled),
        state_required,
        source(ActionabilitySource::NormalizedState),
    ));
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::Visible,
        covered_flag(coverage.visible, target.states.visible),
        pointer_required,
        source(ActionabilitySource::NormalizedState),
    ));
    if operation.requires_focus_eligibility() {
        report.push(ActionabilityCheck::new(
            ActionabilityCheckName::FocusEligible,
            if operation == ActionabilityOperation::SetValue {
                set_value_eligibility(target, coverage)
            } else {
                targeted_type_eligibility(target, coverage)
            },
            state_required,
            source(ActionabilitySource::NormalizedState),
        ));
    }
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::Stable,
        if legacy_id {
            ActionabilityVerdict::Unproven
        } else {
            stable.map_or(ActionabilityVerdict::Unproven, |stable| {
                if stable {
                    ActionabilityVerdict::Passed
                } else {
                    ActionabilityVerdict::Failed
                }
            })
        },
        pointer_required,
        source(ActionabilitySource::GeometrySamples),
    ));
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::InWindow,
        if target
            .bounds
            .and_then(|bounds| bounds.clamped_center(window.0, window.1))
            .is_some()
        {
            ActionabilityVerdict::Passed
        } else {
            ActionabilityVerdict::Failed
        },
        pointer_required,
        source(ActionabilitySource::WindowGeometry),
    ));
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::NonOccluded,
        if legacy_id {
            ActionabilityVerdict::Unproven
        } else {
            match hit {
                PointerHit::Target | PointerHit::AcceptedAncestor => ActionabilityVerdict::Passed,
                PointerHit::Other => ActionabilityVerdict::Failed,
                PointerHit::Inconclusive => ActionabilityVerdict::Unproven,
            }
        },
        pointer_required,
        source(ActionabilitySource::BackendHitProbe),
    ));
    report.push(ActionabilityCheck::new(
        ActionabilityCheckName::BackendFingerprint,
        ActionabilityVerdict::Unproven,
        !legacy_id,
        source(ActionabilitySource::BackendRewalk),
    ));
    report
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{AxNodeId, AxRect, AxRole, AxStates, ElementInfo, SemanticState};

    fn target(role: AxRole, states: AxStates, bounds: Option<AxRect>) -> ElementInfo {
        ElementInfo {
            id: AxNodeId(1),
            role,
            name: Some("target".into()),
            description: Some("description".into()),
            value: None,
            bounds,
            states,
        }
    }

    fn check(report: &ActionabilityReport, name: ActionabilityCheckName) -> ActionabilityCheck {
        let matches = report
            .checks
            .iter()
            .copied()
            .filter(|check| check.name == name)
            .collect::<Vec<_>>();
        assert_eq!(matches.len(), 1, "expected one {name:?} record");
        matches[0]
    }

    #[test]
    fn selector_state_pairs_require_the_underlying_reader_facts() {
        let coverage = AxStateCoverage {
            enabled: true,
            visible: false,
            checkable: true,
            checked: true,
            selected: false,
            expanded: false,
            focused: true,
            focusable: true,
            editable: true,
        };
        assert!(coverage.covers_selector_state(SemanticState::Enabled));
        assert!(coverage.covers_selector_state(SemanticState::Disabled));
        assert!(coverage.covers_selector_state(SemanticState::Checked));
        assert!(coverage.covers_selector_state(SemanticState::Unchecked));
        assert!(!coverage.covers_selector_state(SemanticState::Visible));
        assert!(!coverage.covers_selector_state(SemanticState::Hidden));
        assert!(!coverage.covers_selector_state(SemanticState::Selected));
    }

    #[test]
    fn checked_selectors_require_both_checkability_and_checked_coverage() {
        for (checkable, checked, expected) in [
            (false, false, false),
            (false, true, false),
            (true, false, false),
            (true, true, true),
        ] {
            let coverage = AxStateCoverage {
                checkable,
                checked,
                ..AxStateCoverage::NONE
            };
            assert_eq!(
                coverage.covers_selector_state(SemanticState::Checked),
                expected,
                "checkable={checkable}, checked={checked}"
            );
            assert_eq!(
                coverage.covers_selector_state(SemanticState::Unchecked),
                expected,
                "checkable={checkable}, checked={checked}"
            );
        }
    }

    #[test]
    fn actionability_records_keep_verdict_required_and_source_separate() {
        let check = ActionabilityCheck {
            name: ActionabilityCheckName::NonOccluded,
            verdict: ActionabilityVerdict::Unproven,
            required: true,
            source: ActionabilitySource::BackendHitProbe,
        };
        assert_eq!(check.name.as_str(), "non_occluded");
        assert_eq!(check.verdict.as_str(), "unproven");
        assert_eq!(check.source.as_str(), "backend_hit_probe");
        assert!(check.required);
        assert!(!check.blocks_dispatch());
    }

    #[test]
    fn a_known_failed_required_check_blocks_but_optional_or_unproven_checks_do_not() {
        let required_failure = ActionabilityCheck::new(
            ActionabilityCheckName::Enabled,
            ActionabilityVerdict::Failed,
            true,
            ActionabilitySource::NormalizedState,
        );
        let optional_failure = ActionabilityCheck::new(
            ActionabilityCheckName::Visible,
            ActionabilityVerdict::Failed,
            false,
            ActionabilitySource::NormalizedState,
        );
        let required_unknown = ActionabilityCheck::new(
            ActionabilityCheckName::NonOccluded,
            ActionabilityVerdict::Unproven,
            true,
            ActionabilitySource::BackendHitProbe,
        );
        assert!(required_failure.blocks_dispatch());
        assert!(!optional_failure.blocks_dispatch());
        assert!(!required_unknown.blocks_dispatch());
    }

    #[test]
    fn native_click_requires_unique_and_known_enabled_but_not_visibility_or_occlusion() {
        let coverage = AxStateCoverage {
            enabled: true,
            visible: true,
            ..AxStateCoverage::NONE
        };
        let report = ActionabilityReport::evaluate_click(
            &target(
                AxRole::Button,
                AxStates {
                    enabled: false,
                    visible: false,
                    ..AxStates::default()
                },
                Some(AxRect {
                    x: 200,
                    y: 200,
                    width: 20,
                    height: 20,
                }),
            ),
            coverage,
            Some(false),
            (100, 100),
            PointerHit::Other,
            false,
            false,
        );

        assert_eq!(
            report
                .checks
                .iter()
                .map(|check| check.name)
                .collect::<Vec<_>>(),
            vec![
                ActionabilityCheckName::Unique,
                ActionabilityCheckName::Enabled,
                ActionabilityCheckName::Visible,
                ActionabilityCheckName::Stable,
                ActionabilityCheckName::InWindow,
                ActionabilityCheckName::NonOccluded,
                ActionabilityCheckName::BackendFingerprint,
            ]
        );
        assert_eq!(
            check(&report, ActionabilityCheckName::Unique),
            ActionabilityCheck::new(
                ActionabilityCheckName::Unique,
                ActionabilityVerdict::Passed,
                true,
                ActionabilitySource::SemanticResolution,
            )
        );
        assert_eq!(
            report.blocking(),
            Some(check(&report, ActionabilityCheckName::Enabled))
        );
        for name in [
            ActionabilityCheckName::Visible,
            ActionabilityCheckName::Stable,
            ActionabilityCheckName::InWindow,
            ActionabilityCheckName::NonOccluded,
        ] {
            let check = check(&report, name);
            assert_eq!(check.verdict, ActionabilityVerdict::Failed);
            assert!(!check.required);
        }
    }

    #[test]
    fn pointer_click_blocks_known_hidden_off_window_unstable_and_occluded_targets() {
        let coverage = AxStateCoverage {
            enabled: true,
            visible: true,
            ..AxStateCoverage::NONE
        };
        let report = ActionabilityReport::evaluate_click(
            &target(
                AxRole::Button,
                AxStates {
                    enabled: true,
                    visible: false,
                    ..AxStates::default()
                },
                Some(AxRect {
                    x: 100,
                    y: 100,
                    width: 20,
                    height: 20,
                }),
            ),
            coverage,
            Some(false),
            (100, 100),
            PointerHit::Other,
            false,
            true,
        );

        assert_eq!(
            report.blocking(),
            Some(check(&report, ActionabilityCheckName::Visible))
        );
        for name in [
            ActionabilityCheckName::Visible,
            ActionabilityCheckName::Stable,
            ActionabilityCheckName::InWindow,
            ActionabilityCheckName::NonOccluded,
        ] {
            let check = check(&report, name);
            assert_eq!(check.verdict, ActionabilityVerdict::Failed);
            assert!(check.required);
        }
    }

    #[test]
    fn pointer_click_allows_an_inconclusive_hit_probe_and_marks_it_unproven() {
        let coverage = AxStateCoverage {
            enabled: true,
            visible: true,
            ..AxStateCoverage::NONE
        };
        let report = ActionabilityReport::evaluate_click(
            &target(
                AxRole::Button,
                AxStates {
                    enabled: true,
                    visible: true,
                    ..AxStates::default()
                },
                Some(AxRect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 20,
                }),
            ),
            coverage,
            Some(true),
            (100, 100),
            PointerHit::Inconclusive,
            false,
            true,
        );

        assert_eq!(report.blocking(), None);
        assert_eq!(
            check(&report, ActionabilityCheckName::NonOccluded),
            ActionabilityCheck::new(
                ActionabilityCheckName::NonOccluded,
                ActionabilityVerdict::Unproven,
                true,
                ActionabilitySource::BackendHitProbe,
            )
        );
    }

    #[test]
    fn passing_the_backend_fingerprint_changes_only_that_disclosed_check() {
        let coverage = AxStateCoverage {
            enabled: true,
            visible: true,
            ..AxStateCoverage::NONE
        };
        let mut report = ActionabilityReport::evaluate_click(
            &target(
                AxRole::Button,
                AxStates {
                    enabled: true,
                    visible: true,
                    ..AxStates::default()
                },
                Some(AxRect {
                    x: 10,
                    y: 10,
                    width: 20,
                    height: 20,
                }),
            ),
            coverage,
            Some(true),
            (100, 100),
            PointerHit::Target,
            false,
            true,
        );
        let before = report.clone();

        report.pass_backend_fingerprint();

        assert_eq!(
            check(&report, ActionabilityCheckName::BackendFingerprint).verdict,
            ActionabilityVerdict::Passed
        );
        for original in before.checks {
            if original.name != ActionabilityCheckName::BackendFingerprint {
                assert_eq!(check(&report, original.name), original);
            }
        }
    }

    #[test]
    fn value_and_type_actionability_insert_one_exact_eligibility_check_before_stability() {
        let bounds = AxRect {
            x: 10,
            y: 10,
            width: 80,
            height: 20,
        };
        let editable = target(
            AxRole::Button,
            AxStates {
                enabled: true,
                visible: true,
                editable: true,
                ..AxStates::default()
            },
            Some(bounds),
        );
        let coverage = AxStateCoverage {
            enabled: true,
            visible: true,
            editable: true,
            checkable: true,
            ..AxStateCoverage::NONE
        };

        let semantic =
            ActionabilityReport::evaluate_set_value(&editable, coverage, (100, 100), false);
        let legacy = ActionabilityReport::evaluate_set_value(&editable, coverage, (100, 100), true);
        let targeted = ActionabilityReport::evaluate_targeted_type(
            &editable,
            coverage,
            Some(true),
            (100, 100),
            true,
        );
        for (report, required, source) in [
            (&semantic, true, ActionabilitySource::NormalizedState),
            (&legacy, false, ActionabilitySource::LegacyCache),
            (&targeted, true, ActionabilitySource::NormalizedState),
        ] {
            let positions = report
                .checks
                .iter()
                .enumerate()
                .filter(|(_, check)| check.name == ActionabilityCheckName::FocusEligible)
                .collect::<Vec<_>>();
            assert_eq!(positions.len(), 1);
            let (position, check) = positions[0];
            assert_eq!(
                report.checks[position + 1].name,
                ActionabilityCheckName::Stable
            );
            assert_eq!(check.verdict, ActionabilityVerdict::Passed);
            assert_eq!(check.required, required);
            assert_eq!(check.source, source);
        }
    }

    #[test]
    fn focus_confirmation_discloses_coverage_and_observation_independently() {
        for (covered, confirmed, expected) in [
            (false, false, ActionabilityVerdict::Unproven),
            (false, true, ActionabilityVerdict::Unproven),
            (true, false, ActionabilityVerdict::Failed),
            (true, true, ActionabilityVerdict::Passed),
        ] {
            let mut report = ActionabilityReport::default();
            report.record_focus_confirmation(
                AxStateCoverage {
                    focused: covered,
                    ..AxStateCoverage::NONE
                },
                confirmed,
            );
            assert_eq!(
                report.checks,
                vec![ActionabilityCheck::new(
                    ActionabilityCheckName::Focused,
                    expected,
                    true,
                    ActionabilitySource::ConfirmationPoll,
                )],
                "covered={covered}, confirmed={confirmed}"
            );
        }
    }

    #[test]
    fn set_value_accepts_editable_combo_numeric_and_checkable_roles() {
        let coverage = AxStateCoverage {
            enabled: true,
            focusable: true,
            editable: true,
            checkable: true,
            ..AxStateCoverage::NONE
        };
        let cases = [
            (
                AxRole::TextField,
                AxStates {
                    enabled: true,
                    editable: true,
                    ..AxStates::default()
                },
            ),
            (
                AxRole::ComboBox,
                AxStates {
                    enabled: true,
                    ..AxStates::default()
                },
            ),
            (
                AxRole::Slider,
                AxStates {
                    enabled: true,
                    ..AxStates::default()
                },
            ),
            (
                AxRole::SpinButton,
                AxStates {
                    enabled: true,
                    ..AxStates::default()
                },
            ),
            (
                AxRole::CheckBox,
                AxStates {
                    enabled: true,
                    checkable: true,
                    ..AxStates::default()
                },
            ),
        ];

        for (role, states) in cases {
            let report = ActionabilityReport::evaluate_set_value(
                &target(
                    role,
                    states,
                    Some(AxRect {
                        x: 10,
                        y: 10,
                        width: 20,
                        height: 20,
                    }),
                ),
                coverage,
                (100, 100),
                false,
            );
            assert_eq!(report.blocking(), None, "role {role:?} was rejected");
            assert_eq!(
                check(&report, ActionabilityCheckName::FocusEligible).verdict,
                ActionabilityVerdict::Passed,
                "role {role:?} was not marked eligible"
            );
        }
    }

    #[test]
    fn set_value_eligibility_requires_matching_state_evidence_or_complete_negative_coverage() {
        let cases = [
            (true, true, false, false, ActionabilityVerdict::Passed),
            (false, false, true, true, ActionabilityVerdict::Passed),
            (true, false, false, false, ActionabilityVerdict::Unproven),
            (false, false, true, false, ActionabilityVerdict::Unproven),
            (false, true, false, true, ActionabilityVerdict::Unproven),
            (true, false, true, false, ActionabilityVerdict::Failed),
        ];

        for (covers_editable, editable, covers_checkable, checkable, expected) in cases {
            let coverage = AxStateCoverage {
                editable: covers_editable,
                checkable: covers_checkable,
                ..AxStateCoverage::NONE
            };
            let element = target(
                AxRole::Button,
                AxStates {
                    editable,
                    checkable,
                    ..AxStates::default()
                },
                None,
            );
            let report =
                ActionabilityReport::evaluate_set_value(&element, coverage, (100, 100), false);
            assert_eq!(
                check(&report, ActionabilityCheckName::FocusEligible).verdict,
                expected,
                "coverage=({covers_editable},{covers_checkable}), state=({editable},{checkable})"
            );
            assert_eq!(
                report.eligible_for_resolution(),
                expected == ActionabilityVerdict::Passed
            );
        }
    }

    #[test]
    fn targeted_type_requires_text_role_or_covered_editability_in_both_modes() {
        let cases = [
            (
                AxRole::Button,
                true,
                false,
                true,
                ActionabilityVerdict::Failed,
            ),
            (
                AxRole::Button,
                true,
                false,
                false,
                ActionabilityVerdict::Unproven,
            ),
            (
                AxRole::Button,
                false,
                true,
                false,
                ActionabilityVerdict::Unproven,
            ),
            (
                AxRole::Button,
                false,
                true,
                true,
                ActionabilityVerdict::Passed,
            ),
            (
                AxRole::TextField,
                false,
                false,
                false,
                ActionabilityVerdict::Passed,
            ),
            (
                AxRole::TextArea,
                false,
                false,
                false,
                ActionabilityVerdict::Passed,
            ),
            (
                AxRole::TextField,
                false,
                false,
                true,
                ActionabilityVerdict::Passed,
            ),
            (
                AxRole::TextArea,
                false,
                false,
                true,
                ActionabilityVerdict::Passed,
            ),
        ];
        for (role, focusable, editable, covered, verdict) in cases {
            for pointer in [false, true] {
                let report = ActionabilityReport::evaluate_targeted_type(
                    &target(
                        role,
                        AxStates {
                            enabled: true,
                            visible: true,
                            focusable,
                            editable,
                            ..AxStates::default()
                        },
                        Some(AxRect {
                            x: 10,
                            y: 10,
                            width: 20,
                            height: 20,
                        }),
                    ),
                    AxStateCoverage {
                        enabled: true,
                        visible: true,
                        focusable: true,
                        editable: covered,
                        ..AxStateCoverage::NONE
                    },
                    Some(true),
                    (100, 100),
                    pointer,
                );
                assert_eq!(
                    check(&report, ActionabilityCheckName::FocusEligible),
                    ActionabilityCheck::new(
                        ActionabilityCheckName::FocusEligible,
                        verdict,
                        true,
                        ActionabilitySource::NormalizedState,
                    ),
                    "{role:?}, focusable={focusable}, editable={editable}, covered={covered}, pointer={pointer}"
                );
                assert_eq!(
                    report.eligible_for_resolution(),
                    verdict == ActionabilityVerdict::Passed
                );
                for name in [
                    ActionabilityCheckName::Visible,
                    ActionabilityCheckName::Stable,
                    ActionabilityCheckName::InWindow,
                    ActionabilityCheckName::NonOccluded,
                ] {
                    assert_eq!(check(&report, name).required, pointer);
                }
            }
        }
    }

    #[test]
    fn value_and_type_resolution_still_refuse_a_disabled_eligible_target() {
        let element = target(
            AxRole::TextField,
            AxStates {
                editable: true,
                ..AxStates::default()
            },
            None,
        );
        let coverage = AxStateCoverage {
            enabled: true,
            editable: true,
            ..AxStateCoverage::NONE
        };
        for report in [
            ActionabilityReport::evaluate_set_value(&element, coverage, (100, 100), false),
            ActionabilityReport::evaluate_targeted_type(
                &element,
                coverage,
                None,
                (100, 100),
                false,
            ),
        ] {
            assert_eq!(
                check(&report, ActionabilityCheckName::FocusEligible).verdict,
                ActionabilityVerdict::Passed
            );
            assert!(!report.eligible_for_resolution());
            assert_eq!(
                report.blocking(),
                Some(check(&report, ActionabilityCheckName::Enabled))
            );
        }
    }

    #[test]
    fn targeted_type_evidence_preserves_pointer_proofs_and_replaces_its_eligibility_check() {
        let element = target(
            AxRole::Button,
            AxStates {
                enabled: true,
                visible: true,
                editable: true,
                ..AxStates::default()
            },
            Some(AxRect {
                x: 10,
                y: 10,
                width: 20,
                height: 20,
            }),
        );
        let coverage = AxStateCoverage {
            enabled: true,
            visible: true,
            editable: true,
            ..AxStateCoverage::NONE
        };
        let mut pointer = ActionabilityReport::evaluate_click(
            &element,
            coverage,
            Some(true),
            (100, 100),
            PointerHit::Other,
            false,
            true,
        );
        pointer.pass_backend_fingerprint();
        for mut report in [pointer, ActionabilityReport::default()] {
            let original = report.clone();
            report.record_targeted_type_eligibility(&element, coverage);
            let once = report.clone();
            report.record_targeted_type_eligibility(&element, coverage);
            assert_eq!(report, once);
            assert_eq!(
                check(&report, ActionabilityCheckName::FocusEligible).verdict,
                ActionabilityVerdict::Passed
            );
            let remaining = report
                .checks
                .iter()
                .copied()
                .filter(|check| check.name != ActionabilityCheckName::FocusEligible)
                .collect::<Vec<_>>();
            assert_eq!(remaining, original.checks);
            if let Some(stable) = report
                .checks
                .iter()
                .position(|check| check.name == ActionabilityCheckName::Stable)
            {
                assert_eq!(
                    report.checks[stable - 1].name,
                    ActionabilityCheckName::FocusEligible
                );
            }
            let mut ineligible = element.clone();
            ineligible.states.editable = false;
            report.record_targeted_type_eligibility(&ineligible, coverage);
            assert_eq!(
                check(&report, ActionabilityCheckName::FocusEligible).verdict,
                ActionabilityVerdict::Failed
            );
        }
    }

    #[test]
    fn legacy_pointer_actionability_is_disclosed_without_making_semantic_checks_required() {
        let report = ActionabilityReport::evaluate_click(
            &target(
                AxRole::Button,
                AxStates::default(),
                Some(AxRect {
                    x: 100,
                    y: 100,
                    width: 20,
                    height: 20,
                }),
            ),
            AxStateCoverage {
                enabled: true,
                visible: true,
                ..AxStateCoverage::NONE
            },
            Some(false),
            (100, 100),
            PointerHit::Other,
            true,
            true,
        );

        assert!(report.checks.iter().all(|check| !check.required));
        assert!(
            report
                .checks
                .iter()
                .all(|check| check.source == ActionabilitySource::LegacyCache)
        );
        assert_eq!(report.blocking(), None);
        assert_eq!(
            check(&report, ActionabilityCheckName::Unique).verdict,
            ActionabilityVerdict::Unproven
        );
        assert_eq!(
            check(&report, ActionabilityCheckName::Enabled).verdict,
            ActionabilityVerdict::Failed
        );
        assert_eq!(
            check(&report, ActionabilityCheckName::Visible).verdict,
            ActionabilityVerdict::Failed
        );
    }
}
