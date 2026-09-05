//! Optional session evidence recording and offline inspection.

mod capture;
mod config;
mod format;
mod fs;
mod inspect;
mod recorder;
mod store;

pub(crate) use capture::{ACTIVE_CALL, RequestGuard, arguments, current_call, start_arguments};
pub use config::TraceConfig;
pub use inspect::{export, inspect, print_inspection};
pub(crate) use recorder::{CallTrace, TraceRecorder, argument_bytes};

#[cfg(test)]
mod tests;
#[cfg(test)]
mod transport_tests;
