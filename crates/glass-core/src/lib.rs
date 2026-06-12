#![feature(portable_simd)]
//! glass-core: platform-agnostic core for the glass UI-automation harness.
//!
//! No OS/windowing types appear here; every backend detail lives behind the
//! [`Platform`] trait, implemented in separate backend crates.

// Modules are added task-by-task.

pub mod error;
pub use error::{GlassError, Result};

pub mod toolpath;
pub use toolpath::tool_path;

pub mod frame;
pub use frame::{Frame, Region};

pub mod input;
pub use input::{drag_path, drag_schedule};

pub mod keys;
pub use keys::Modifier;

pub mod image_io;
pub use image_io::{frame_from_webp, frame_to_webp};

pub mod diff;
pub use diff::{diff, diff_perceptual, region_satisfied, BBox, DiffResult, RegionUntil};
pub mod doctor;
pub use doctor::{Check, CheckStatus, Diagnosis, Section};

pub mod stability;
pub use stability::StabilityTracker;

pub mod poll;
pub use poll::{poll_until, PollOutcome};

pub mod baseline;
pub use baseline::BaselineStore;

pub mod logbuf;
pub use logbuf::{LogBuffer, LogLine, Stream};

pub mod platform;
pub use platform::{
    AppSpec, KeyEvent, MouseButton, Platform, PointerEvent, SandboxLevel, WindowGeometry,
    WindowHint, WindowId, WindowInfo, WindowOp,
};

pub mod accessibility;
pub use accessibility::{
    element_match, Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget,
    AxTree, ElementCondition, ElementInfo, ElementMatch,
};

pub mod marks;
pub use marks::Mark;

pub mod session;
pub use session::{
    Backend, Glass, PlatformFactory, WaitElementOutcome, WaitElementParams, WaitLogOutcome,
    WaitLogParams, WaitRegionOutcome, WaitRegionParams, WaitStableOutcome, WaitStableParams,
};
