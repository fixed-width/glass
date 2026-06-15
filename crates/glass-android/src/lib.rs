//! Android (AVD emulator) backend for glass: drives native apps over `adb`.
//!
//! Host-OS-agnostic — this crate links nothing platform-specific; it shells out
//! to `adb`. The emulator's VM is the isolation boundary, so there is no
//! sandbox machinery here.

mod adb;
mod build;
mod cmd;
mod logs;
mod parse;
mod platform;
mod screencap;
mod target;

pub use platform::AndroidPlatform;
pub use target::{AdbTarget, AttachedDevice};
