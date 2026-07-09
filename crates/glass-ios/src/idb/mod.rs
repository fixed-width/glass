//! Generated idb gRPC client, compiled from the vendored `proto/idb.proto`.
//!
//! The `idb_companion` tool exposes UI automation (HID input, accessibility) over
//! gRPC; `build.rs` turns the vendored proto into a tonic client with protox, and
//! this module includes that generated code as [`proto`].
//!
//! prost/tonic rename the proto's `HID*`-cased identifiers. The paths downstream
//! code should use (confirmed against the generated code) are:
//! - client:      `proto::companion_service_client::CompanionServiceClient<Channel>`
//! - `Point`:     `proto::Point` (fields `x`, `y`: `f64`)
//! - `HIDEvent`:  `proto::HidEvent`; its `oneof event` -> enum `proto::hid_event::Event`
//! - nested types live under `mod hid_event`:
//!   - `proto::hid_event::HidPress`   (fields `action`, `direction: i32`)
//!   - `proto::hid_event::HidPressAction`; its `oneof action` -> `hid_event::hid_press_action::Action`
//!   - `proto::hid_event::HidTouch`   (field `point`)
//!   - `proto::hid_event::HidButton`  (field `button: i32`) / `HidButtonType` enum
//!   - `proto::hid_event::HidKey`     (field `keycode: u64`)
//!   - `proto::hid_event::HidSwipe`   (fields `start`, `end`, `delta`, `duration`)
//!   - `proto::hid_event::HidDelay`, `proto::hid_event::HidPinch`
//!   - `HIDDirection` -> `proto::hid_event::HidDirection` (enum: `Down` = 0, `Up` = 1)
//! - `HIDResponse` -> `proto::HidResponse` (empty message)
//! - `proto::AccessibilityInfoRequest` (fields `point`, `format: i32`); its nested
//!   `Format` enum -> `proto::accessibility_info_request::Format` (`Legacy` = 0, `Nested` = 1)
//! - `proto::AccessibilityInfoResponse` (field `json: String`)
// prost/tonic output is machine-generated and isn't held to our lint gate: it trips
// `dead_code` for messages no rpc references (e.g. `OpenUrlResponse`, since upstream's
// `open_url` returns `OpenUrlRequest`) and clippy's stylistic lints don't apply here.
#[allow(clippy::all, clippy::pedantic, dead_code)]
pub mod proto {
    // The vendored proto declares `package idb;`, so the generated file is `idb.rs`.
    tonic::include_proto!("idb");
}

/// The top-level generated types downstream code uses, flattened onto `crate::idb`.
///
/// Nested types stay reachable at their generated module paths, e.g.
/// `proto::hid_event::{HidPress, HidTouch, HidKey, HidSwipe, HidDirection}`,
/// `proto::hid_event::hid_press_action::Action`, and
/// `proto::accessibility_info_request::Format` — this re-export deliberately does
/// not flatten the whole tree.
// These are a stable surface for the sync client / input / accessibility code that
// consumes this module; `allow(unused_imports)` because nothing references them yet.
#[allow(unused_imports)]
pub use proto::{
    companion_service_client::CompanionServiceClient, AccessibilityInfoRequest,
    AccessibilityInfoResponse, HidEvent, HidResponse, Point,
};

#[cfg(test)]
mod tests {
    use super::proto;

    #[test]
    fn generated_types_are_nameable() {
        // A pure compile/reference check that codegen produced the client + messages.
        // Constructs a Point and names the client + key message types; no network.
        let p = proto::Point { x: 1.0, y: 2.0 };
        assert_eq!((p.x, p.y), (1.0, 2.0));

        // Naming these types anchors the paths downstream code depends on: if a
        // regenerated proto renames them, this stops compiling.
        let _client: Option<
            proto::companion_service_client::CompanionServiceClient<tonic::transport::Channel>,
        > = None;
        let _hid: Option<proto::HidEvent> = None;
        let _req: Option<proto::AccessibilityInfoRequest> = None;
        let _resp: Option<proto::AccessibilityInfoResponse> = None;
    }
}
