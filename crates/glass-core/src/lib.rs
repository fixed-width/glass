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

pub mod drag;
pub use drag::{run_drag, DragGesture, DragSink};

pub mod chord;
pub use chord::{run_chord, ChordSink, CHORD_DWELL};

pub mod scroll;
pub use scroll::{run_scroll, ScrollSink, SCROLL_DWELL};

pub mod typing;
pub use typing::{run_type, TypeSink, TYPE_DWELL};

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
    A11yBind, AppSpec, KeyEvent, MouseButton, Platform, PointerEvent, SandboxLevel, Segment,
    WindowGeometry, WindowHint, WindowId, WindowInfo, WindowOp, MAX_GESTURE_POINTERS,
};

pub mod accessibility;
pub use accessibility::{
    element_match, Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget,
    AxTree, ElementCondition, ElementInfo, ElementMatch,
};

pub mod marks;
pub use marks::Mark;

pub mod audit;
pub use audit::{Actuation, ActuationContext, AuditOutcome, AuditSink, ElementRef, WindowRef};

pub mod session;
pub use session::{
    Backend, Glass, PlatformFactory, WaitElementOutcome, WaitElementParams, WaitLogOutcome,
    WaitLogParams, WaitRegionOutcome, WaitRegionParams, WaitStableOutcome, WaitStableParams,
};
