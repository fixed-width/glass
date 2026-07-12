//! Backend capability descriptors: which operations an agent can perform right now.
//!
//! A [`CapabilityMap`] is produced per backend (each backend crate's `capabilities()`)
//! and surfaced by the `glass_capabilities` MCP tool. The map's named fields make
//! capability completeness a compile-time guarantee: add a [`Capability`] variant and
//! every `CapabilityMap` literal fails to compile until it fills the new field.

use serde::{Deserialize, Serialize};

/// The operations whose availability varies by backend or environment.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Capability {
    MultiTouch,
    Clipboard,
    Accessibility,
    WindowMoveResize,
}

impl Capability {
    /// Every capability, for consumers/tests that iterate the set. The named fields of
    /// [`CapabilityMap`] remain the completeness authority.
    pub const ALL: &'static [Capability] = &[
        Capability::MultiTouch,
        Capability::Clipboard,
        Capability::Accessibility,
        Capability::WindowMoveResize,
    ];
}

/// Whether an operation can be performed right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Works right now.
    Supported,
    /// Supported by this backend in principle, but a setup step is missing right now.
    RequiresSetup,
    /// This backend can never do it (a code-constant fact).
    Unsupported,
}

/// One capability's status plus an optional human note (what's missing / why).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CapabilityStatus {
    pub status: Support,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub note: Option<&'static str>,
}

impl CapabilityStatus {
    pub const fn new(status: Support, note: Option<&'static str>) -> Self {
        Self { status, note }
    }
    pub const fn supported() -> Self {
        Self::new(Support::Supported, None)
    }
    pub const fn unsupported(note: Option<&'static str>) -> Self {
        Self::new(Support::Unsupported, note)
    }
    pub const fn requires_setup(note: &'static str) -> Self {
        Self::new(Support::RequiresSetup, Some(note))
    }
}

/// One status per capability. Serializes to a JSON object keyed by field name.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
pub struct CapabilityMap {
    pub multi_touch: CapabilityStatus,
    pub clipboard: CapabilityStatus,
    pub accessibility: CapabilityStatus,
    pub window_move_resize: CapabilityStatus,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_serializes_to_keyed_object_snake_case_notes_omitted_when_none() {
        let m = CapabilityMap {
            multi_touch: CapabilityStatus::requires_setup("need agent"),
            clipboard: CapabilityStatus::supported(),
            accessibility: CapabilityStatus::supported(),
            window_move_resize: CapabilityStatus::unsupported(Some("full-screen")),
        };
        let v = serde_json::to_value(m).unwrap();
        assert_eq!(v["multi_touch"]["status"], "requires_setup");
        assert_eq!(v["multi_touch"]["note"], "need agent");
        assert_eq!(v["clipboard"]["status"], "supported");
        assert!(
            v["clipboard"].get("note").is_none(),
            "note omitted when None"
        );
        assert_eq!(v["window_move_resize"]["status"], "unsupported");
        assert_eq!(v["window_move_resize"]["note"], "full-screen");
    }

    #[test]
    fn capability_all_lists_every_variant_snake_case() {
        assert_eq!(Capability::ALL.len(), 4);
        assert_eq!(
            serde_json::to_value(Capability::MultiTouch).unwrap(),
            "multi_touch"
        );
        assert_eq!(
            serde_json::to_value(Capability::WindowMoveResize).unwrap(),
            "window_move_resize"
        );
    }
}
