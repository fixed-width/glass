use super::SemanticState;

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
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum PointerHit {
    Target,
    AcceptedAncestor,
    Other,
    Inconclusive,
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::SemanticState;

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
}
