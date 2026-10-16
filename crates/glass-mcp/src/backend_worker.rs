//! backend operations shared by the normal server's stdio and HTTP paths.

use base64::Engine;
use glass_core::*;
use serde::{Deserialize, Serialize};

pub(crate) const VERSION: u32 = 1;

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub version: u32,
    pub id: u64,
    pub expires_ns: Option<u64>,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Operation {
    Info,
    Configure(Vec<ProtectedHostPath>),
    Start(AppSpec),
    Stop,
    Capture(Option<Region>),
    CaptureWindow(WindowId, Option<Region>),
    Pointer(PointerEvent),
    Key(KeyEvent),
    GetClipboard,
    SetClipboard(String),
    Window(WindowOp),
    ListWindows,
    SelectWindow(WindowId),
    Logs,
    Pids,
    Occluded((i32, i32)),
    Snapshot(Context),
    SetValue(Context, AxTarget, String),
    Invoke(Context, AxTarget),
    Focus(Context, AxTarget),
    PointerTarget(Context, AxTarget, (i32, i32)),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Context {
    pub pids: Vec<u32>,
    pub window: WindowGeometry,
    pub window_handle: Option<i64>,
    pub a11y_bus_addr: Option<String>,
    pub limits: WalkLimits,
}

impl Context {
    pub(crate) fn from_core(ctx: &AxContext) -> Self {
        Self {
            pids: ctx.pids.clone(),
            window: ctx.window.clone(),
            window_handle: ctx.window_handle,
            a11y_bus_addr: ctx.a11y_bus_addr.clone(),
            limits: ctx.limits,
        }
    }

    fn into_core(self, deadline: Deadline) -> AxContext {
        AxContext {
            pids: self.pids,
            window: self.window,
            window_handle: self.window_handle,
            a11y_bus_addr: self.a11y_bus_addr,
            limits: self.limits,
            deadline,
        }
    }
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Properties {
    pub input_route: InputRoute,
    pub input: InputCapabilities,
    pub app_pid: Option<u32>,
    pub window: Option<WindowId>,
    pub scope_basis: Option<u64>,
    pub coverage: AxStateCoverage,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Value {
    Unit,
    Geometry(WindowGeometry),
    Protection(HostPathProtectionMode),
    Image(String),
    Text(String),
    Windows(Vec<WindowInfo>),
    Logs(Vec<(Stream, String)>),
    Pids(Vec<u32>),
    Bool(bool),
    Tree(Box<AxTree>),
    Node(Option<AxNodeId>),
    Hit(PointerHit),
}

#[derive(Serialize, Deserialize)]
pub(crate) struct Response {
    pub version: u32,
    pub id: u64,
    pub properties: Properties,
    pub result: Result<Value>,
}

pub(crate) fn now_ns() -> Result<u64> {
    let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
    u64::try_from(time.tv_sec)
        .ok()
        .and_then(|seconds| seconds.checked_mul(1_000_000_000))
        .and_then(|seconds| seconds.checked_add(time.tv_nsec as u64))
        .ok_or_else(|| GlassError::Backend("monotonic clock overflow".into()))
}

pub(crate) fn expires(deadline: Deadline) -> Result<Option<u64>> {
    deadline
        .remaining()
        .map(|remaining| {
            now_ns()?
                .checked_add(
                    u64::try_from(remaining.as_nanos()).map_err(|_| {
                        GlassError::Backend("deadline cannot be represented".into())
                    })?,
                )
                .ok_or_else(|| GlassError::Backend("deadline overflow".into()))
        })
        .transpose()
}

fn deadline(expires_ns: Option<u64>) -> Result<Deadline> {
    match expires_ns {
        None => Ok(Deadline::UNBOUNDED),
        Some(expires) => Ok(Deadline::at(
            std::time::Instant::now()
                + std::time::Duration::from_nanos(expires.saturating_sub(now_ns()?)),
        )),
    }
}

pub(crate) struct Dispatcher<P, A> {
    platform: P,
    accessibility: A,
}

impl<P: Platform, A: Accessibility> Dispatcher<P, A> {
    pub(crate) fn new(platform: P, accessibility: A) -> Self {
        Self {
            platform,
            accessibility,
        }
    }

    fn properties(&self) -> Properties {
        Properties {
            input_route: self.platform.input_route(),
            input: self.platform.input_capabilities(),
            app_pid: self.platform.app_pid(),
            window: self.platform.observation_window_id(),
            scope_basis: self
                .accessibility
                .observation_scope()
                .map(|scope| scope.coordinate_basis),
            coverage: self.accessibility.state_coverage(),
        }
    }

    pub(crate) fn current_properties(&self) -> Properties {
        self.properties()
    }

    fn context(&self, ctx: Context, deadline: Deadline) -> Result<AxContext> {
        let owned = self.platform.app_pids_by(deadline)?;
        if ctx.pids.is_empty() || ctx.pids.iter().any(|pid| !owned.contains(pid)) {
            return Err(GlassError::Backend(
                "accessibility context is outside the owned application".into(),
            )
            .before_dispatch());
        }
        Ok(ctx.into_core(deadline))
    }

    pub(crate) fn handle(&mut self, request: Request) -> Response {
        let result = if request.version != VERSION {
            Err(GlassError::Backend("backend worker protocol mismatch".into()).before_dispatch())
        } else {
            deadline(request.expires_ns)
                .and_then(|deadline| self.apply(request.operation, deadline))
        };
        Response {
            version: VERSION,
            id: request.id,
            properties: self.properties(),
            result,
        }
    }

    fn apply(&mut self, operation: Operation, deadline: Deadline) -> Result<Value> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("backend worker operation"));
        }
        match operation {
            Operation::Info => Ok(Value::Unit),
            Operation::Configure(paths) => self
                .platform
                .configure_protected_host_paths(&paths)
                .map(Value::Protection),
            Operation::Start(spec) => self.platform.start_app(&spec).map(Value::Geometry),
            Operation::Stop => self.platform.stop_app_by(deadline).map(|()| Value::Unit),
            Operation::Capture(region) => self
                .platform
                .capture_frame_by(region.as_ref(), deadline)
                .and_then(image),
            Operation::CaptureWindow(id, region) => self
                .platform
                .capture_window_by(id, region.as_ref(), deadline)
                .and_then(image),
            Operation::Pointer(event) => self
                .platform
                .send_pointer_by(&event, deadline)
                .map(|()| Value::Unit),
            Operation::Key(event) => self
                .platform
                .send_key_by(&event, deadline)
                .map(|()| Value::Unit),
            Operation::GetClipboard => self.platform.get_clipboard().map(Value::Text),
            Operation::SetClipboard(text) => {
                self.platform.set_clipboard(&text).map(|()| Value::Unit)
            }
            Operation::Window(op) => self.platform.window_by(&op, deadline).map(Value::Geometry),
            Operation::ListWindows => self.platform.list_windows_by(deadline).map(Value::Windows),
            Operation::SelectWindow(id) => self
                .platform
                .select_window_by(id, deadline)
                .map(Value::Geometry),
            Operation::Logs => Ok(Value::Logs(self.platform.drain_logs())),
            Operation::Pids => self.platform.app_pids_by(deadline).map(Value::Pids),
            Operation::Occluded(point) => self
                .platform
                .pointer_window_occluded_by(point, deadline)
                .map(Value::Bool),
            Operation::Snapshot(ctx) => {
                let ctx = self.context(ctx, deadline)?;
                self.accessibility
                    .snapshot(&ctx)
                    .map(|tree| Value::Tree(Box::new(tree)))
            }
            Operation::SetValue(ctx, target, text) => {
                let ctx = self.context(ctx, deadline)?;
                self.accessibility
                    .set_value(&ctx, &target, &text)
                    .map(|()| Value::Unit)
            }
            Operation::Invoke(ctx, target) => {
                let ctx = self.context(ctx, deadline)?;
                self.accessibility.invoke(&ctx, &target).map(Value::Node)
            }
            Operation::Focus(ctx, target) => {
                let ctx = self.context(ctx, deadline)?;
                self.accessibility.focus(&ctx, &target).map(Value::Node)
            }
            Operation::PointerTarget(ctx, target, point) => {
                let ctx = self.context(ctx, deadline)?;
                self.accessibility
                    .pointer_target_at(&ctx, &target, point)
                    .map(Value::Hit)
            }
        }
    }

    pub(crate) fn stop(&mut self, deadline: Deadline) -> Result<()> {
        self.platform.stop_app_by(deadline)
    }
}

fn image(frame: Frame) -> Result<Value> {
    glass_core::image_io::frame_to_webp(&frame)
        .map(|bytes| Value::Image(base64::engine::general_purpose::STANDARD.encode(bytes)))
}

pub(crate) fn frame(encoded: &str) -> Result<Frame> {
    let bytes = base64::engine::general_purpose::STANDARD
        .decode(encoded)
        .map_err(|error| GlassError::CaptureFailed(error.to_string()))?;
    glass_core::image_io::frame_from_webp(&bytes)
}
