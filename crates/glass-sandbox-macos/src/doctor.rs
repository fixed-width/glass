//! Doctor probe for the macOS Seatbelt sandbox. `sandbox_init` ships on every supported
//! macOS (it underpins App Sandbox and is used by Chromium), so availability is effectively
//! always `Ok`; the check exists for parity with `glass-sandbox-linux` and to surface the
//! "deprecated but shipping" note.
use glass_core::{Check, CheckStatus};

/// Whether Seatbelt containment is usable here.
pub enum Availability {
    Ok,
    Unavailable(String),
}

/// On macOS, Seatbelt is always present. Off macOS, this crate's containment can't run.
pub fn availability() -> Availability {
    #[cfg(target_os = "macos")]
    {
        Availability::Ok
    }
    #[cfg(not(target_os = "macos"))]
    {
        Availability::Unavailable("Seatbelt is macOS-only".into())
    }
}

/// Doctor check(s) for the macOS sandbox.
pub fn checks() -> Vec<Check> {
    match availability() {
        Availability::Ok => vec![Check::new(
            "sandbox (seatbelt)",
            CheckStatus::Ok,
            "sandbox_init present (Seatbelt; deprecated but shipping)",
        )],
        Availability::Unavailable(why) => {
            vec![Check::new("sandbox (seatbelt)", CheckStatus::Fail, why)]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[cfg(target_os = "macos")]
    fn macos_reports_ok() {
        assert!(matches!(availability(), Availability::Ok));
        assert_eq!(checks().len(), 1);
    }

    #[test]
    #[cfg(not(target_os = "macos"))]
    fn non_macos_reports_unavailable() {
        assert!(matches!(availability(), Availability::Unavailable(_)));
        let checks = checks();
        assert_eq!(checks.len(), 1);
        assert_eq!(checks[0].status, CheckStatus::Fail);
    }
}
