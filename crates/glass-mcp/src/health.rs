//! The `/healthz` payload: whether the *running* server (on macOS, the LaunchAgent — its own
//! responsible process) holds the two TCC grants. Onboarding and `glass-mcp setup`, run from a
//! different responsible process, cannot read the agent's grant from themselves, so they poll this.
use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    /// The server is up and answering. Always true from a live handler.
    pub ok: bool,
    /// This process's Screen Recording grant (macOS). `None` on platforms without the concept.
    #[serde(default)]
    pub screen_recording: Option<bool>,
    /// This process's Accessibility grant (macOS). `None` off macOS.
    #[serde(default)]
    pub accessibility: Option<bool>,
}

impl HealthStatus {
    /// True once both macOS grants read granted — the signal setup/onboarding poll to complete.
    pub fn grants_ready(&self) -> bool {
        self.screen_recording == Some(true) && self.accessibility == Some(true)
    }
}

/// Snapshot the running process's grant state.
pub fn current_health() -> HealthStatus {
    #[cfg(target_os = "macos")]
    {
        HealthStatus {
            ok: true,
            screen_recording: Some(glass_macos::screen_recording_granted()),
            accessibility: Some(glass_macos::accessibility_granted()),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        HealthStatus {
            ok: true,
            screen_recording: None,
            accessibility: None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn health_json_round_trips_and_shapes() {
        let h = HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(false),
        };
        let s = serde_json::to_string(&h).unwrap();
        assert!(s.contains("\"screen_recording\":true"));
        assert!(s.contains("\"accessibility\":false"));
        let back: HealthStatus = serde_json::from_str(&s).unwrap();
        assert_eq!(back.screen_recording, Some(true));
        assert_eq!(back.accessibility, Some(false));
    }

    #[test]
    fn health_both_granted_predicate() {
        assert!(HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(true)
        }
        .grants_ready());
        assert!(!HealthStatus {
            ok: true,
            screen_recording: Some(true),
            accessibility: Some(false)
        }
        .grants_ready());
        assert!(!HealthStatus {
            ok: true,
            screen_recording: None,
            accessibility: None
        }
        .grants_ready());
    }
}
