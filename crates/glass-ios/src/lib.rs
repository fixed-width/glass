//! iOS Simulator backend for glass: drives native apps over `xcrun simctl`.
//!
//! macOS-only in practice (the tools are Apple's), but the code links nothing
//! platform-specific — it shells out. The Simulator is the isolation boundary,
//! so there is no sandbox machinery here. Input and the accessibility tree are
//! not implemented yet.
#![forbid(unsafe_code)]

mod capture;
mod device;
pub mod doctor;
mod idb;
mod injector;
mod logs;
mod platform;
mod simctl;
mod target;

pub use platform::IosPlatform;
pub use simctl::Simctl;
pub use target::{SimTarget, SimulatorRegistry};
