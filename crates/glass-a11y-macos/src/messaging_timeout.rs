//! Apple propagates the system-wide AX timeout process-wide, so one owner serializes setters and
//! refreshes the remaining budget before each AX message.

use glass_core::{Deadline, GlassError, Result};
use std::sync::{Condvar, Mutex};
use std::thread::{self, ThreadId};
use std::time::Duration;

const DOCTOR_TIMEOUT: Duration = Duration::from_secs(2);
const CONTAMINATION_PREFIX: &str = "AX messaging timeout owner is contaminated";
const REENTRY_MESSAGE: &str = "macOS AX timeout owner rejected same-thread reentry";

/// The only production owner of the process-global AX messaging timeout.
static AX_MESSAGING_TIMEOUT_OWNER: TimeoutOwner = TimeoutOwner::new();

/// Platform-specific access to the system-wide AX timeout setter.
pub trait AxMessaging {
    type Element;

    fn system_wide_element(&self) -> Self::Element;
    fn set_messaging_timeout(&self, element: &Self::Element, seconds: f32) -> Result<()>;
}

#[derive(Default)]
struct OwnerState {
    owner_thread: Option<ThreadId>,
    contamination: Option<String>,
}

/// Serializes every process-global AX timeout setter and remembers an unsafe failed restoration.
pub struct TimeoutOwner {
    state: Mutex<OwnerState>,
    available: Condvar,
}

impl Default for TimeoutOwner {
    fn default() -> Self {
        Self::new()
    }
}

impl TimeoutOwner {
    pub const fn new() -> Self {
        Self {
            state: Mutex::new(OwnerState {
                owner_thread: None,
                contamination: None,
            }),
            available: Condvar::new(),
        }
    }

    /// Run a window/backend operation whose per-message timeout is the remaining caller budget.
    /// `already_dispatched` carries compound-operation provenance from work done before this owner.
    pub fn with_deadline_by<A, T>(
        &self,
        ax: &A,
        deadline: Deadline,
        already_dispatched: bool,
        operation: impl FnOnce(&mut MessageScope<'_, '_, A>) -> Result<T>,
    ) -> Result<T>
    where
        A: AxMessaging,
    {
        self.with_mode(ax, deadline, deadline, already_dispatched, operation)
    }

    /// Run the doctor probe under its fixed two-second total budget and this same owner.
    pub fn with_doctor_timeout_by<A, T>(
        &self,
        ax: &A,
        operation: impl FnOnce(&mut MessageScope<'_, '_, A>) -> Result<T>,
    ) -> Result<T>
    where
        A: AxMessaging,
    {
        self.with_timeout_by(ax, DOCTOR_TIMEOUT, operation)
    }

    /// Run a fixed-total-budget AX operation. Gate wait and messages share the same deadline.
    pub fn with_timeout_by<A, T>(
        &self,
        ax: &A,
        timeout: Duration,
        operation: impl FnOnce(&mut MessageScope<'_, '_, A>) -> Result<T>,
    ) -> Result<T>
    where
        A: AxMessaging,
    {
        let instant = std::time::Instant::now()
            .checked_add(timeout)
            .ok_or_else(|| GlassError::Backend("AX messaging timeout budget overflowed".into()))?;
        let deadline = Deadline::at(instant);
        self.with_mode(ax, deadline, deadline, false, operation)
    }

    fn with_mode<A, T>(
        &self,
        ax: &A,
        deadline: Deadline,
        gate_deadline: Deadline,
        already_dispatched: bool,
        operation: impl FnOnce(&mut MessageScope<'_, '_, A>) -> Result<T>,
    ) -> Result<T>
    where
        A: AxMessaging,
    {
        let permit = self.acquire(gate_deadline, already_dispatched)?;
        let element = ax.system_wide_element();
        let mut scope = MessageScope {
            ax,
            element,
            deadline,
            dispatched: already_dispatched,
            installed: false,
            permit,
        };
        let result = operation(&mut scope);
        scope.complete(result)
    }

    fn acquire(&self, deadline: Deadline, already_dispatched: bool) -> Result<TimeoutPermit<'_>> {
        let current_thread = thread::current().id();
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        loop {
            if let Some(contamination) = &state.contamination {
                return Err(contamination_error(contamination));
            }
            if state.owner_thread == Some(current_thread) {
                return Err(reentry_error(already_dispatched));
            }
            if deadline.has_passed() {
                return Err(deadline_error("macOS AX timeout owner", already_dispatched));
            }
            if state.owner_thread.is_none() {
                state.owner_thread = Some(current_thread);
                return Ok(TimeoutPermit { owner: self });
            }

            state = match deadline.remaining() {
                None => self
                    .available
                    .wait(state)
                    .unwrap_or_else(|poisoned| poisoned.into_inner()),
                Some(remaining) => {
                    let (next, _) = self
                        .available
                        .wait_timeout(state, remaining)
                        .unwrap_or_else(|poisoned| poisoned.into_inner());
                    next
                }
            };
        }
    }

    fn contaminate(&self, failure: &GlassError) {
        let message = format!(
            "AX messaging timeout restoration failed ({failure}); the process-global timeout is \
             contaminated and this process must restart before another AX operation"
        );
        let mut state = self
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.contamination = Some(message.clone());
        drop(state);
        eprintln!("glass-a11y-macos: {message}");
    }
}

struct TimeoutPermit<'a> {
    owner: &'a TimeoutOwner,
}

impl Drop for TimeoutPermit<'_> {
    fn drop(&mut self) {
        let mut state = self
            .owner
            .state
            .lock()
            .unwrap_or_else(|poisoned| poisoned.into_inner());
        state.owner_thread = None;
        self.owner.available.notify_all();
    }
}

/// A serialized AX operation. Every actual AX IPC must run through [`Self::message`].
pub struct MessageScope<'owner, 'ax, A: AxMessaging> {
    ax: &'ax A,
    element: A::Element,
    deadline: Deadline,
    dispatched: bool,
    installed: bool,
    permit: TimeoutPermit<'owner>,
}

impl<A: AxMessaging> MessageScope<'_, '_, A> {
    /// Refresh the process-global timeout, then immediately dispatch one AX message.
    pub fn message<T>(
        &mut self,
        operation: &str,
        message: impl FnOnce() -> Result<T>,
    ) -> Result<T> {
        let Some(timeout) = self.deadline.remaining() else {
            return self.dispatch(message);
        };
        if timeout.is_zero() {
            return Err(deadline_error(operation, self.dispatched));
        }

        self.ax
            .set_messaging_timeout(&self.element, timeout.as_secs_f32())
            .map_err(|error| {
                if self.dispatched {
                    error.after_dispatch()
                } else {
                    error
                }
            })?;
        self.installed = true;
        let result = self.dispatch(message);
        if self.deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed(operation));
        }
        result
    }

    fn dispatch<T>(&mut self, message: impl FnOnce() -> Result<T>) -> Result<T> {
        self.dispatched = true;
        message().map_err(GlassError::after_dispatch)
    }

    fn complete<T>(mut self, result: Result<T>) -> Result<T> {
        match self.restore() {
            Ok(()) => result,
            Err(restore) => {
                if let Err(primary) = result {
                    eprintln!(
                        "glass-a11y-macos: AX operation also failed before timeout restoration: \
                         {primary}"
                    );
                }
                Err(restore)
            }
        }
    }

    fn restore(&mut self) -> Result<()> {
        if !self.installed {
            return Ok(());
        }
        self.installed = false;
        match self.ax.set_messaging_timeout(&self.element, 0.0) {
            Ok(()) => Ok(()),
            Err(failure) => {
                self.permit.owner.contaminate(&failure);
                let contamination = contamination_error(&failure.to_string());
                if self.dispatched {
                    Err(contamination.after_dispatch())
                } else {
                    Err(contamination)
                }
            }
        }
    }
}

impl<A: AxMessaging> Drop for MessageScope<'_, '_, A> {
    fn drop(&mut self) {
        if let Err(failure) = self.restore() {
            eprintln!("glass-a11y-macos: {failure}");
        }
    }
}

fn deadline_error(operation: &str, dispatched: bool) -> GlassError {
    if dispatched {
        GlassError::caller_deadline_elapsed(operation)
    } else {
        GlassError::deadline_not_started(operation)
    }
}

fn contamination_error(detail: &str) -> GlassError {
    GlassError::Backend(format!(
        "{CONTAMINATION_PREFIX}; restart glass before another AX operation ({detail})"
    ))
}

fn reentry_error(already_dispatched: bool) -> GlassError {
    let error = GlassError::Backend(REENTRY_MESSAGE.into());
    if already_dispatched {
        error.after_dispatch()
    } else {
        error.before_dispatch()
    }
}

/// Whether an error reports that a failed reset left the process-global AX timeout unsafe.
pub fn is_contamination_error(error: &GlassError) -> bool {
    matches!(
        error.cause(),
        GlassError::Backend(message) if message.starts_with(CONTAMINATION_PREFIX)
    )
}

/// Production window entry point. The ScreenCaptureKit query runs before owner acquisition, so a
/// later gate expiry retains proof that external work may already have dispatched.
pub fn with_window_query_by<A, Q, T>(
    ax: &A,
    deadline: Deadline,
    query: impl FnOnce() -> Result<Q>,
    operation: impl FnOnce(Q, &mut MessageScope<'_, '_, A>) -> Result<T>,
) -> Result<T>
where
    A: AxMessaging,
{
    let query_result = query()?;
    AX_MESSAGING_TIMEOUT_OWNER
        .with_deadline_by(ax, deadline, true, |scope| operation(query_result, scope))
}

/// Production fixed-timeout entry point used by `glass_doctor`.
pub fn with_doctor_timeout_by<A, T>(
    ax: &A,
    operation: impl FnOnce(&mut MessageScope<'_, '_, A>) -> Result<T>,
) -> Result<T>
where
    A: AxMessaging,
{
    AX_MESSAGING_TIMEOUT_OWNER.with_doctor_timeout_by(ax, operation)
}
