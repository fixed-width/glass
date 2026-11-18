//! iOS Simulator backend for glass: drives native apps over `xcrun simctl`.
//!
//! macOS-only in practice — `xcrun simctl` and `idb_companion` are both Apple
//! tools, and the accessibility/input transport is a Unix-domain-socket gRPC
//! client to `idb_companion`, so the crate is `cfg(unix)`-gated and compiles
//! to nothing off unix. Gated to `unix` rather than macOS so the crate's pure
//! logic still compiles and unit-tests on the free Linux runner.
//! The Simulator is the isolation boundary, so there is no sandbox machinery
//! here. The backend drives input (tap, type, swipe, scroll) and reads the
//! accessibility tree via `idb_companion` — and, on a companion that
//! implements the event, a two-finger pinch. Other multi-touch gestures are
//! unsupported: the vendored proto carries one multi-contact event, a canned
//! pinch, and the raw-touch primitive is single-contact, so there is nothing
//! to drive an N-finger gesture with.
#![cfg(unix)]
#![forbid(unsafe_code)]

mod a11y;
mod axmap;
mod capture;
mod device;
pub mod doctor;
mod idb;
mod injector;
mod logs;
mod platform;
mod pointer;
mod recording;
mod simctl;
mod target;

pub use a11y::IosA11y;
pub use platform::IosPlatform;
pub use simctl::Simctl;
pub use target::{SimTarget, SimulatorRegistry};

use glass_core::capability::{CapabilityMap, CapabilityStatus, Support};

/// This backend's canonical name (matches the `glass_capabilities` / `GLASS_BACKEND` value).
pub const BACKEND: &str = "ios";

const ACCESSIBILITY_BACKEND_ENV: &str = "GLASS_IOS_ACCESSIBILITY_BACKEND";
const AXBRIDGE_SETUP: &str = "axbridge needs idb_companion with the bundled SimulatorFrameworkBridge helper (verified with v1.7.8)";

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum AccessibilityBackend {
    Host,
    AxBridge,
}

impl AccessibilityBackend {
    fn resolve(
        requested: Option<&str>,
        default: impl FnOnce() -> glass_core::Result<Self>,
    ) -> glass_core::Result<Self> {
        requested.map_or_else(default, Self::parse)
    }

    fn from_env() -> glass_core::Result<Self> {
        match std::env::var(ACCESSIBILITY_BACKEND_ENV) {
            Ok(value) => Self::parse(&value),
            Err(std::env::VarError::NotPresent) => Ok(Self::Host),
            Err(error) => Err(glass_core::GlassError::Unsupported(format!(
                "{ACCESSIBILITY_BACKEND_ENV}: {error}"
            ))),
        }
    }

    fn parse(value: &str) -> glass_core::Result<Self> {
        match value {
            "ax" => Ok(Self::Host),
            "axbridge" => Ok(Self::AxBridge),
            _ => Err(glass_core::GlassError::Unsupported(format!(
                "{ACCESSIBILITY_BACKEND_ENV} must be ax or axbridge"
            ))),
        }
    }
}

/// This backend's live capability map. `input`/`accessibility` need `idb_companion` — gated on
/// `doctor::companion_runnable`, which is the launch path's own verdict, so a companion that is
/// installed but cannot be executed is unavailable here exactly as it would be at spawn time.
pub fn capabilities() -> CapabilityMap {
    capabilities_with(
        crate::doctor::companion_runnable(),
        AccessibilityBackend::from_env(),
    )
}

fn capabilities_with(
    companion: bool,
    reader: glass_core::Result<AccessibilityBackend>,
) -> CapabilityMap {
    let accessibility = match (companion, reader) {
        (_, Err(_)) => CapabilityStatus::requires_setup(
            "GLASS_IOS_ACCESSIBILITY_BACKEND must be ax or axbridge",
        ),
        (false, _) => CapabilityStatus::requires_setup(
            "needs a runnable idb_companion. Run glass doctor. For WKWebView content, configure \
             accessibility_reader=\"axbridge\" in glass_start",
        ),
        (true, Ok(AccessibilityBackend::Host)) => CapabilityStatus::new(
            Support::Supported,
            Some(
                "ax reader configured by default. For WKWebView content, pass accessibility_reader=\"axbridge\" \
                 to glass_start. Use idb_companion with its \
                 bundled SimulatorFrameworkBridge helper. Guest reads leave the simulator in automation mode",
            ),
        ),
        (true, Ok(AccessibilityBackend::AxBridge)) => CapabilityStatus::degraded(
            "axbridge configured by default for WKWebView snapshots and element lookup. To use the \
             host reader for a session, pass accessibility_reader=\"ax\" to glass_start. The first guest read \
             checks companion support. Guest reads leave the simulator in automation mode. \
             Use coordinate input for web actions and coordinate focus plus typing for writes",
        ),
    };
    CapabilityMap {
        input: if companion {
            CapabilityStatus::degraded("US-ASCII input only; non-ASCII characters are unsupported")
        } else {
            CapabilityStatus::requires_setup("needs idb_companion (observe-only without it)")
        },
        multi_touch: if companion {
            CapabilityStatus::degraded(
                "two-finger pinch only, and only with an idb_companion that implements it. \
                 The pinch is delivered symmetrically about the midpoint of the two start \
                 points, on idb's own axis: the scale factor is preserved, the individual \
                 finger paths and the gesture's axis are not. Rotation, two-finger pan, \
                 3-or-more fingers, fingers starting at one point, and fingers starting \
                 closer than 8pt from centre are all refused by name",
            )
        } else {
            // RequiresSetup, not Unsupported: installing a companion moves this cell to
            // Degraded, so the backend can do it in principle and only a setup step is
            // missing — the same condition `input` and `accessibility` report that way.
            CapabilityStatus::requires_setup("needs idb_companion (observe-only without it)")
        },
        clipboard: CapabilityStatus::new(
            Support::Supported,
            Some("paste needs on-screen consent (Allow Paste)"),
        ),
        accessibility,
        window_move_resize: CapabilityStatus::unsupported(Some("apps are full-screen")),
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use glass_core::capability::Support;

    #[test]
    fn input_is_degraded_with_companion_requires_setup_without() {
        let c = capabilities_with(true, Ok(AccessibilityBackend::Host));
        assert_eq!(c.input.status, Support::Degraded);
        assert!(c.input.note.unwrap().contains("ASCII"));
        assert_eq!(
            capabilities_with(false, Ok(AccessibilityBackend::Host))
                .input
                .status,
            Support::RequiresSetup
        );
    }

    #[test]
    fn accessibility_gates_on_companion() {
        assert_eq!(
            capabilities_with(true, Ok(AccessibilityBackend::Host))
                .accessibility
                .status,
            Support::Supported
        );
        assert_eq!(
            capabilities_with(false, Ok(AccessibilityBackend::Host))
                .accessibility
                .status,
            Support::RequiresSetup
        );
    }

    #[test]
    fn accessibility_note_reports_how_to_enable_axbridge_and_when_it_is_configured() {
        let host = capabilities_with(true, AccessibilityBackend::parse("ax")).accessibility;
        assert!(
            host.note
                .unwrap()
                .contains("accessibility_reader=\"axbridge\"")
        );

        let guest = capabilities_with(true, AccessibilityBackend::parse("axbridge")).accessibility;
        assert_eq!(guest.status, Support::Degraded);
        assert!(guest.note.unwrap().contains("axbridge configured"));

        let invalid = capabilities_with(true, AccessibilityBackend::parse("guest")).accessibility;
        assert_eq!(invalid.status, Support::RequiresSetup);
    }

    #[test]
    fn explicit_reader_skips_an_invalid_environment_default() {
        let invalid_default = || AccessibilityBackend::parse("bad-default");
        assert_eq!(
            AccessibilityBackend::resolve(Some("axbridge"), invalid_default).unwrap(),
            AccessibilityBackend::AxBridge
        );
        assert!(AccessibilityBackend::resolve(None, invalid_default).is_err());
    }

    #[test]
    fn multi_touch_is_degraded_with_a_companion_and_needs_setup_without() {
        let with = capabilities_with(true, Ok(AccessibilityBackend::Host));
        assert_eq!(with.multi_touch.status, Support::Degraded);
        let note = with
            .multi_touch
            .note
            .expect("a degraded cell explains itself");
        assert!(note.contains("pinch"), "{note}");
        assert!(
            note.contains("Rotation") && note.contains("two-finger pan"),
            "the refused gestures are named: {note}"
        );
        assert!(
            note.contains("finger paths"),
            "what is accepted is approximated, and the note must say so: {note}"
        );
        assert_eq!(
            capabilities_with(false, Ok(AccessibilityBackend::Host))
                .multi_touch
                .status,
            Support::RequiresSetup,
            "a missing companion is a setup step, not a permanent incapacity"
        );
    }

    #[test]
    fn constant_cells_are_fixed() {
        let c = capabilities_with(true, Ok(AccessibilityBackend::Host));
        assert_eq!(c.clipboard.status, Support::Supported);
        assert_eq!(
            c.clipboard.note.as_deref(),
            Some("paste needs on-screen consent (Allow Paste)")
        );
        assert_eq!(c.window_move_resize.status, Support::Unsupported);
    }
}

#[cfg(test)]
mod unsupported_message_tests {
    #[test]
    fn multi_touch_unsupported_message_names_the_ios_backend_with_its_reason() {
        // The injector supplies the refused gesture as the note rather than a blanket
        // capability string, so this pins the shape that reaches the caller.
        let msg = glass_core::GlassError::unsupported(
            "multi_touch",
            crate::BACKEND,
            Some("a rotation, which is a different gesture from a pinch"),
        )
        .to_string();
        assert!(msg.contains("ios backend"), "{msg}");
        assert!(msg.contains("rotation"), "{msg}");
        assert!(msg.contains("glass_capabilities"), "{msg}");
    }

    #[test]
    fn window_move_resize_unsupported_message_names_the_ios_backend() {
        let msg = glass_core::GlassError::unsupported(
            "window_move_resize",
            crate::BACKEND,
            crate::capabilities().window_move_resize.note.as_deref(),
        )
        .to_string();
        assert!(msg.contains("ios backend"), "{msg}");
        assert!(msg.contains("full-screen"), "{msg}");
    }
}
