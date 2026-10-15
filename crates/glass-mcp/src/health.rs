//! The `/healthz` payload: whether the *running* server (on macOS, the LaunchAgent — its own
//! responsible process) holds the two TCC grants. Onboarding and `glass-mcp setup`, run from a
//! different responsible process, cannot read the agent's grant from themselves, so they poll this.
use serde::{Deserialize, Serialize};

#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub struct HealthStatus {
    /// The server is up and answering. Always true from a live handler.
    pub ok: bool,
    /// This process's Screen Recording grant (macOS). `None` on platforms without the concept.
    #[serde(default)]
    pub screen_recording: Option<bool>,
    /// This process's Accessibility grant (macOS). `None` off macOS.
    #[serde(default)]
    pub accessibility: Option<bool>,
    /// PID of this responding server, used to verify its managed LaunchAgent.
    #[serde(default)]
    pub process_id: Option<u32>,
    /// Whether this process routes macOS sessions through an enrolled account.
    #[serde(default)]
    pub macos_account: bool,
}

impl HealthStatus {
    /// True once both macOS grants read granted — the signal setup/onboarding poll to complete.
    /// Its only callers are the macOS grant-verification flow (`setup`/`onboarding`) and this
    /// crate's tests, so it's gated to macOS+test like they are: now that `health` is
    /// `pub(crate)` (no longer public API), a plain non-test Linux/Windows build — where
    /// `/healthz` still serializes a [`HealthStatus`] but nothing reads this predicate — would
    /// otherwise flag it dead.
    #[cfg(any(target_os = "macos", test))]
    pub fn grants_ready(&self) -> bool {
        self.screen_recording == Some(true) && self.accessibility == Some(true)
    }

    #[cfg(any(target_os = "macos", test))]
    pub fn private_account_ready(&self, managed_pid: u32) -> bool {
        self.ok && managed_pid != 0 && self.process_id == Some(managed_pid) && self.macos_account
    }
}

/// Snapshot the running process's grant state.
#[cfg(feature = "network")]
pub fn current_health() -> HealthStatus {
    #[cfg(target_os = "macos")]
    {
        let configured = glass_macos::private_session::configured();
        let shared = matches!(configured, Ok(false));
        HealthStatus {
            ok: true,
            screen_recording: shared.then(glass_macos::screen_recording_granted),
            accessibility: shared.then(glass_macos::accessibility_granted),
            process_id: Some(std::process::id()),
            macos_account: matches!(configured, Ok(true)),
        }
    }
    #[cfg(not(target_os = "macos"))]
    {
        HealthStatus {
            ok: true,
            screen_recording: None,
            accessibility: None,
            process_id: Some(std::process::id()),
            macos_account: false,
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
            ..Default::default()
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
        assert!(
            HealthStatus {
                ok: true,
                screen_recording: Some(true),
                accessibility: Some(true),
                ..Default::default()
            }
            .grants_ready()
        );
        assert!(
            !HealthStatus {
                ok: true,
                screen_recording: Some(true),
                accessibility: Some(false),
                ..Default::default()
            }
            .grants_ready()
        );
        assert!(
            !HealthStatus {
                ok: true,
                screen_recording: None,
                accessibility: None,
                ..Default::default()
            }
            .grants_ready()
        );
    }
    #[test]
    fn private_readiness_requires_the_managed_process_and_account_route() {
        let ready = HealthStatus {
            ok: true,
            process_id: Some(42),
            macos_account: true,
            ..Default::default()
        };
        assert!(ready.private_account_ready(42));
        assert!(!ready.private_account_ready(43));
        assert!(!ready.private_account_ready(0));
        assert!(!HealthStatus { ok: false, ..ready }.private_account_ready(42));
        assert!(
            !HealthStatus {
                macos_account: false,
                ..ready
            }
            .private_account_ready(42)
        );
        assert!(
            !HealthStatus {
                process_id: None,
                ..ready
            }
            .private_account_ready(42)
        );
        let legacy: HealthStatus =
            serde_json::from_str(r#"{"ok":true,"screen_recording":true,"accessibility":true}"#)
                .unwrap();
        assert!(legacy.grants_ready());
        assert!(!legacy.private_account_ready(42));
    }
}
