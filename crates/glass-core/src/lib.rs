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

pub mod bounded;
pub use bounded::{TIMED_OUT, run_bounded, run_bounded_with_stdin};

pub mod set_value;
pub use set_value::{read_back_confirms, typed_clear_landed, typed_text_landed};

pub mod frame;
pub use frame::{Frame, Region};

pub mod pixels;
pub use pixels::{SourceOrder, to_opaque_rgba, to_opaque_rgba_in_place};

pub mod drag;
pub use drag::{DragGesture, DragSink, run_drag};

pub mod chord;
pub use chord::{CHORD_DWELL, ChordSink, run_chord};

pub mod coords;

pub mod scroll;
pub use scroll::{SCROLL_DWELL, ScrollSink, run_scroll};

pub mod typing;
pub use typing::{TYPE_DWELL, TypeSink, run_type};

pub mod keys;
pub use keys::Modifier;

pub mod image_io;
pub use image_io::{frame_from_webp, frame_to_webp};

pub mod diff;
pub use diff::{
    BBox, DiffResult, IgnoreMask, RegionUntil, diff, diff_perceptual, diff_perceptual_with_mask,
    diff_with_mask, region_satisfied,
};
pub mod doctor;
pub use doctor::{Check, CheckStatus, Diagnosis, Section};

pub mod capability;
pub use capability::{CapabilityMap, CapabilityStatus, Support};

pub mod stability;
pub use stability::StabilityTracker;

pub mod poll;
pub use poll::{PollOutcome, poll_until};

pub mod baseline;
pub use baseline::BaselineStore;

pub mod logbuf;
pub use logbuf::{LogBuffer, LogLine, Stream};

pub mod platform;
pub use platform::{
    A11yBind, AppSpec, KeyEvent, MAX_GESTURE_POINTERS, MouseButton, Platform, PointerEvent,
    SandboxLevel, Segment, TEARDOWN_BUDGET, WindowGeometry, WindowHint, WindowId, WindowInfo,
    WindowOp,
};

pub mod accessibility;
pub use accessibility::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree,
    ChangeSignal, ChangeWait, ClickMethod, ElementCondition, ElementInfo, ElementMatch, MAX_DEPTH,
    MAX_NODES, MAX_SIBLINGS, Truncation, TruncationLimit, WalkBudget, WalkLimits, element_match,
    normalize_description,
};

pub mod marks;
pub use marks::{Mark, MarkLabel};

pub mod outline;

pub mod role_support;

pub mod role_histogram;
pub use role_histogram::{RoleTally, role_histogram};

pub mod description_census;
pub use description_census::{
    DescribedSample, DescriptionCensus, DescriptionSourcing, description_census,
    description_census_report,
};

pub mod audit;
pub use audit::{Actuation, ActuationContext, AuditOutcome, AuditSink, ElementRef, WindowRef};

pub mod session;
pub use session::{
    Backend, Glass, PlatformFactory, SCROLL_TO_DEFAULT_STEP, SCROLL_TO_DEFAULT_TIMEOUT_MS,
    ScrollDirection, ScrollToElementOutcome, ScrollToElementParams, WaitElementOutcome,
    WaitElementParams, WaitLogOutcome, WaitLogParams, WaitRegionOutcome, WaitRegionParams,
    WaitStableOutcome, WaitStableParams,
};
