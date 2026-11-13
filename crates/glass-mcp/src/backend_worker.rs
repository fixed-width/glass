//! backend operations shared by the normal server's stdio and HTTP paths.

use base64::Engine;
use glass_core::*;
use serde::{Deserialize, Serialize};

pub(crate) const VERSION: u32 = 4;

#[derive(Serialize, Deserialize)]
pub(crate) struct Request {
    pub version: u32,
    pub id: u64,
    pub expires_ns: Option<u64>,
    pub operation: Operation,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Operation {
    #[cfg(target_os = "macos")]
    OutputAuthority(Option<glass_macos::workspace_access::outputs::OutputScope>),
    Info,
    Configure(Vec<ProtectedHostPath>),
    Start(AppSpec),
    Stop,
    RecordStart(RecordingOptions),
    RecordProgress(String),
    BufferStart(RollingOptions),
    BufferProgress(String),
    BufferMark {
        id: String,
        clip_id: String,
        tail_ms: u64,
    },
    BufferSave(String),
    BufferStop {
        id: String,
        keep: bool,
    },
    RecordFinish {
        id: String,
        keep: bool,
    },
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
    pub recording_supported: bool,
    pub buffer_supported: bool,
    pub recording_scope: RecordingScope,
    pub input_route: InputRoute,
    pub input: InputCapabilities,
    pub app_pid: Option<u32>,
    pub window: Option<WindowId>,
    pub window_handle: Option<i64>,
    pub scope_basis: Option<u64>,
    pub coverage: AxStateCoverage,
}

#[derive(Serialize, Deserialize)]
pub(crate) enum Value {
    BufferProgress(RollingProgress),
    BufferWindow(RollingWindow),
    BufferedVideo {
        window: RollingWindow,
        format: RecordingFormat,
        progress: RecordingProgress,
        data: String,
    },
    RecordingProgress(RecordingProgress),
    Video {
        format: RecordingFormat,
        progress: RecordingProgress,
        data: String,
    },
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
    #[cfg(windows)]
    return glass_windows::desktop::monotonic_ns();
    #[cfg(unix)]
    {
        let time = rustix::time::clock_gettime(rustix::time::ClockId::Monotonic);
        u64::try_from(time.tv_sec)
            .ok()
            .and_then(|seconds| seconds.checked_mul(1_000_000_000))
            .and_then(|seconds| seconds.checked_add(time.tv_nsec as u64))
            .ok_or_else(|| GlassError::Backend("monotonic clock overflow".into()))
    }
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

pub(crate) fn deadline(expires_ns: Option<u64>) -> Result<Deadline> {
    match expires_ns {
        None => Ok(Deadline::UNBOUNDED),
        Some(expires) => Ok(Deadline::at(
            std::time::Instant::now()
                + std::time::Duration::from_nanos(expires.saturating_sub(now_ns()?)),
        )),
    }
}

pub(crate) struct Dispatcher<P, A> {
    buffer: Option<(String, Box<dyn RollingBuffer>)>,
    recording: Option<(String, Box<dyn Recording>)>,
    platform: P,
    accessibility: A,
}

impl<P: Platform, A: Accessibility> Dispatcher<P, A> {
    pub(crate) fn new(platform: P, accessibility: A) -> Self {
        Self {
            buffer: None,
            recording: None,
            platform,
            accessibility,
        }
    }

    fn properties(&self) -> Properties {
        Properties {
            recording_supported: self.platform.recording_supported(),
            buffer_supported: self.platform.rolling_buffer_supported(),
            recording_scope: self.platform.recording_scope(),
            input_route: self.platform.input_route(),
            input: self.platform.input_capabilities(),
            app_pid: self.platform.app_pid(),
            window: self.platform.observation_window_id(),
            window_handle: self.platform.active_window_handle(),
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
            #[cfg(target_os = "macos")]
            Operation::OutputAuthority(_) => Err(GlassError::Backend(
                "output authority requires the private native worker".into(),
            )
            .before_dispatch()),
            Operation::Info => Ok(Value::Unit),
            Operation::Configure(paths) => self
                .platform
                .configure_protected_host_paths(&paths)
                .map(Value::Protection),
            Operation::Start(spec) => {
                self.discard_buffer(deadline)?;
                self.discard_recording(deadline)?;
                self.platform.start_app(&spec).map(Value::Geometry)
            }
            Operation::Stop => self.stop(deadline).map(|()| Value::Unit),
            Operation::RecordStart(options) => {
                if self.recording.is_some() || self.buffer.is_some() {
                    return Err(GlassError::Backend(
                        "a private recording already awaits disposition".into(),
                    ));
                }
                options.validate()?;
                let id = options.id.clone();
                let handle = self.platform.start_recording(options, deadline)?;
                let progress = handle.progress();
                self.recording = Some((id, handle));
                Ok(Value::RecordingProgress(progress))
            }
            Operation::BufferStart(options) => {
                if self.recording.is_some() || self.buffer.is_some() {
                    return Err(GlassError::Backend(
                        "a private capture is already active".into(),
                    ));
                }
                options.validate()?;
                let id = options.id.clone();
                let handle = self.platform.start_rolling_buffer(options, deadline)?;
                let progress = handle.progress();
                self.buffer = Some((id, handle));
                Ok(Value::BufferProgress(progress))
            }
            Operation::BufferProgress(id) => {
                Ok(Value::BufferProgress(self.buffer_handle(&id)?.progress()))
            }
            Operation::BufferMark {
                id,
                clip_id,
                tail_ms,
            } => self
                .buffer_handle(&id)?
                .mark(&clip_id, tail_ms, deadline)
                .map(Value::BufferWindow),
            Operation::BufferSave(id) => {
                let captured = self.buffer_handle(&id)?.save(deadline)?;
                buffered_video(captured)
            }
            Operation::BufferStop { id, keep } => {
                self.buffer_handle(&id)?;
                let (_, mut handle) = self
                    .buffer
                    .take()
                    .ok_or_else(|| GlassError::Backend("private buffer unavailable".into()))?;
                match handle.stop(keep, deadline)? {
                    Some(video) => buffered_video(video),
                    None => Ok(Value::Unit),
                }
            }
            Operation::RecordProgress(id) => {
                let (active, handle) = self.recording.as_ref().ok_or_else(|| {
                    GlassError::Backend("private recording is unavailable".into())
                })?;
                if *active != id {
                    return Err(GlassError::Backend(
                        "private recording identity mismatch".into(),
                    ));
                }
                Ok(Value::RecordingProgress(handle.progress()))
            }
            Operation::RecordFinish { id, keep } => {
                if !self
                    .recording
                    .as_ref()
                    .is_some_and(|(active, _)| *active == id)
                {
                    return Err(GlassError::Backend(
                        "private recording identity mismatch".into(),
                    ));
                }
                let (_, mut handle) = self.recording.take().ok_or_else(|| {
                    GlassError::Backend("private recording is unavailable".into())
                })?;
                let video = handle.finish(keep, deadline)?;
                if video.data.len() as u64 > recording::RECORDING_MAX_BYTES {
                    return Err(GlassError::Backend(
                        "private recording exceeds the byte limit".into(),
                    ));
                }
                Ok(Value::Video {
                    format: video.format,
                    progress: video.progress,
                    data: base64::engine::general_purpose::STANDARD.encode(video.data),
                })
            }
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
        let buffer = self.discard_buffer(deadline);
        let recording = self.discard_recording(deadline).and(buffer);
        let stopped = self.platform.stop_app_by(deadline);
        match (recording, stopped) {
            (Ok(()), result) | (result, Ok(())) => result,
            (Err(primary), Err(cleanup)) => Err(GlassError::cleanup_failed(
                "stopping private recorder and app",
                primary,
                cleanup,
            )),
        }
    }

    fn buffer_handle(&mut self, id: &str) -> Result<&mut Box<dyn RollingBuffer>> {
        let (active, handle) = self
            .buffer
            .as_mut()
            .ok_or_else(|| GlassError::Backend("private buffer unavailable".into()))?;
        if active != id {
            return Err(GlassError::Backend(
                "private buffer identity mismatch".into(),
            ));
        }
        Ok(handle)
    }

    fn discard_buffer(&mut self, deadline: Deadline) -> Result<()> {
        if let Some((_, mut handle)) = self.buffer.take() {
            handle.stop(false, deadline)?;
        }
        Ok(())
    }

    fn discard_recording(&mut self, deadline: Deadline) -> Result<()> {
        if let Some((_, mut handle)) = self.recording.take() {
            handle.finish(false, deadline)?;
        }
        Ok(())
    }
}

fn buffered_video(captured: BufferedVideo) -> Result<Value> {
    if captured.video.data.len() as u64 > recording::RECORDING_MAX_BYTES {
        return Err(GlassError::Backend(
            "private buffer clip exceeds byte limit".into(),
        ));
    }
    Ok(Value::BufferedVideo {
        window: captured.window,
        format: captured.video.format,
        progress: captured.video.progress,
        data: base64::engine::general_purpose::STANDARD.encode(captured.video.data),
    })
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
