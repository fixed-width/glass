//! iOS Simulator backend for glass: drives native apps over `xcrun simctl`.
//!
//! macOS-only in practice (the tools are Apple's), but the code links nothing
//! platform-specific — it shells out. The Simulator is the isolation boundary,
//! so there is no sandbox machinery here. The backend drives input (tap, type,
//! swipe, scroll) and reads the accessibility tree via `idb_companion`;
//! multi-touch gestures are not yet supported.
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
mod simctl;
mod target;

pub use a11y::IosA11y;
pub use platform::IosPlatform;
pub use simctl::Simctl;
pub use target::{SimTarget, SimulatorRegistry};
