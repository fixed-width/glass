//! Backend capability descriptors: which operations an agent can perform right now.
//!
//! A [`CapabilityMap`] is produced per backend (each backend crate's `capabilities()`)
//! and surfaced by the `glass_capabilities` MCP tool. `CapabilityMap`'s named fields are
//! the completeness authority: a capability is added by adding a field, and every
//! backend's `capabilities()` literal then fails to compile until it supplies that field,
//! so no backend can silently omit a capability.

use serde::Serialize;

/// Session background support has no degraded state: a route is admitted or refused.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BackgroundSupport {
    Supported,
    RequiresSetup,
    Unsupported,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundOperation {
    pub status: BackgroundSupport,
    pub reasons: Vec<String>,
}

impl BackgroundOperation {
    pub fn unsupported(reason: &str) -> Self {
        Self {
            status: BackgroundSupport::Unsupported,
            reasons: vec![reason.to_owned()],
        }
    }
}

/// Identity of the qualification authorizing a background route.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundProfile {
    pub id: String,
    pub qualification_reference: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct BackgroundInputCapabilities {
    pub click: BackgroundOperation,
    pub scroll: BackgroundOperation,
    pub text: BackgroundOperation,
    pub profile: Option<BackgroundProfile>,
}

impl Default for BackgroundInputCapabilities {
    fn default() -> Self {
        Self {
            click: BackgroundOperation::unsupported("no qualified background click route"),
            scroll: BackgroundOperation::unsupported("no qualified background scroll route"),
            text: BackgroundOperation::unsupported("background text input is unsupported"),
            profile: None,
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct SessionCapabilities {
    pub backend: String,
    pub input_mode: crate::InputMode,
    pub background_input: BackgroundInputCapabilities,
}

/// Whether an operation can be performed right now.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Support {
    /// Works right now.
    Supported,
    /// Works now but at reduced fidelity/coverage (note says what's lost + how to restore).
    Degraded,
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
    pub const fn degraded(note: &'static str) -> Self {
        Self::new(Support::Degraded, Some(note))
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
    /// Pointer + keyboard injection (glass_type/click/key/drag/scroll/move/do).
    pub input: CapabilityStatus,
    pub multi_touch: CapabilityStatus,
    pub clipboard: CapabilityStatus,
    pub accessibility: CapabilityStatus,
    pub window_move_resize: CapabilityStatus,
}

impl CapabilityMap {
    /// Every (operation-name, status) pair, in report order. The exhaustive destructure (no
    /// `..`) extends the named-field completeness guarantee into rendering: adding a field to
    /// `CapabilityMap` fails THIS to compile until it is listed here too.
    pub fn entries(&self) -> [(&'static str, CapabilityStatus); 5] {
        let CapabilityMap {
            input,
            multi_touch,
            clipboard,
            accessibility,
            window_move_resize,
        } = *self;
        [
            ("input", input),
            ("multi_touch", multi_touch),
            ("clipboard", clipboard),
            ("accessibility", accessibility),
            ("window_move_resize", window_move_resize),
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn map_serializes_to_keyed_object_snake_case_notes_omitted_when_none() {
        let m = CapabilityMap {
            input: CapabilityStatus::degraded("adb only"),
            multi_touch: CapabilityStatus::requires_setup("need agent"),
            clipboard: CapabilityStatus::supported(),
            accessibility: CapabilityStatus::supported(),
            window_move_resize: CapabilityStatus::unsupported(Some("full-screen")),
        };
        let v = serde_json::to_value(m).unwrap();
        assert_eq!(v["input"]["status"], "degraded");
        assert_eq!(v["input"]["note"], "adb only");
        assert_eq!(v["multi_touch"]["status"], "requires_setup");
        assert_eq!(v["clipboard"]["status"], "supported");
        assert!(v["clipboard"].get("note").is_none());
        assert_eq!(v["window_move_resize"]["status"], "unsupported");
    }
}
