//! iOS Simulator backend for glass: drives native apps over `xcrun simctl`.
//!
//! macOS-only in practice (the tools are Apple's), but the code links nothing
//! platform-specific — it shells out. The Simulator is the isolation boundary,
//! so there is no sandbox machinery here. Input and the accessibility tree are
//! not implemented yet; a planned follow-up will add them via an on-simulator
//! driver.
#![forbid(unsafe_code)]

mod capture;
mod device;
mod logs;
mod simctl;
mod target;

pub use simctl::Simctl;
pub use target::{SimTarget, SimulatorRegistry};
