//! one adapter at the Platform/Accessibility seam for either MCP transport.

use std::sync::{Arc, Mutex};

use glass_core::*;

use crate::backend_worker::{Context, Operation, Properties, Response, Value};

pub(crate) trait Connection: Send {
    fn call(&mut self, operation: Operation, deadline: Deadline) -> Result<Response>;
    fn close(&mut self, deadline: Deadline) -> Result<()>;
    fn cleanup_complete(&self) -> bool {
        false
    }
}

struct Shared<C> {
    connection: C,
    properties: Properties,
    closed: bool,
    healthy: bool,
    log_failure: Option<GlassError>,
    stopped: bool,
}

pub(crate) fn backend<C: Connection + 'static>(mut connection: C) -> Result<Backend> {
    let initial = connection.call(Operation::Info, Deadline::UNBOUNDED)?;
    unit(initial.result?)?;
    let shared = Arc::new(Mutex::new(Shared {
        connection,
        properties: initial.properties,
        closed: false,
        healthy: true,
        log_failure: None,
        stopped: false,
    }));
    Ok(Backend {
        platform: Box::new(RemotePlatform(Arc::clone(&shared))),
        accessibility: Some(Box::new(RemoteAccessibility(shared))),
    })
}

struct RemotePlatform<C: Connection>(Arc<Mutex<Shared<C>>>);
struct RemoteAccessibility<C>(Arc<Mutex<Shared<C>>>);

impl<C: Connection> Shared<C> {
    fn call(&mut self, operation: Operation, deadline: Deadline) -> Result<Value> {
        if self.closed {
            return Err(GlassError::NoActiveSession.before_dispatch());
        }
        if let Some(error) = &self.log_failure {
            return Err(GlassError::Backend(format!(
                "an earlier worker log read failed ({error}); this request was not sent"
            ))
            .before_dispatch());
        }
        let response = self.connection.call(operation, deadline).inspect_err(|_| {
            self.healthy = false;
        })?;
        self.properties = response.properties;
        response.result
    }

    fn close(&mut self, deadline: Deadline) -> Result<()> {
        if self.closed {
            return Ok(());
        }
        let stopped = if self.stopped {
            Ok(())
        } else {
            self.connection
                .call(Operation::Stop, deadline)
                .and_then(|response| {
                    self.properties = response.properties;
                    response.result.and_then(unit)
                })
        };
        self.stopped |= stopped.is_ok();
        let released = self.connection.close(deadline);
        self.closed = released.is_ok() || self.connection.cleanup_complete();
        match (stopped, released) {
            (Ok(()), result) => result,
            (Err(error), Ok(())) => Err(error),
            (Err(primary), Err(cleanup)) => Err(GlassError::cleanup_failed(
                "closing private backend",
                primary,
                cleanup,
            )),
        }
    }
}

fn lock<C>(shared: &Mutex<Shared<C>>) -> Result<std::sync::MutexGuard<'_, Shared<C>>> {
    shared
        .lock()
        .map_err(|_| GlassError::Backend("backend worker state is poisoned".into()))
}

fn wrong_response() -> GlassError {
    GlassError::Backend("backend worker returned the wrong response type".into())
}

fn unit(value: Value) -> Result<()> {
    if matches!(value, Value::Unit) {
        Ok(())
    } else {
        Err(wrong_response())
    }
}
fn geometry(value: Value) -> Result<WindowGeometry> {
    if let Value::Geometry(value) = value {
        Ok(value)
    } else {
        Err(wrong_response())
    }
}
fn node(value: Value) -> Result<Option<AxNodeId>> {
    if let Value::Node(value) = value {
        Ok(value)
    } else {
        Err(wrong_response())
    }
}

impl<C: Connection + 'static> Platform for RemotePlatform<C> {
    fn input_route(&self) -> InputRoute {
        lock(&self.0)
            .map(|state| state.properties.input_route)
            .unwrap_or(InputRoute::SharedDesktop)
    }
    fn input_capabilities(&self) -> InputCapabilities {
        let Ok(mut state) = lock(&self.0) else {
            return InputCapabilities::unknown();
        };
        if !state.healthy || state.closed {
            return InputCapabilities::unknown();
        }
        match state
            .connection
            .call(Operation::Info, Deadline::from_millis(1000))
        {
            Ok(response) if matches!(response.result, Ok(Value::Unit) | Err(_)) => {
                state.properties = response.properties;
                state.properties.input.clone()
            }
            _ => {
                state.healthy = false;
                InputCapabilities::unknown()
            }
        }
    }

    fn configure_protected_host_paths(
        &mut self,
        paths: &[ProtectedHostPath],
    ) -> Result<HostPathProtectionMode> {
        match lock(&self.0)?.call(Operation::Configure(paths.to_vec()), Deadline::UNBOUNDED)? {
            Value::Protection(value) => Ok(value),
            _ => Err(wrong_response()),
        }
    }
    fn start_app(&mut self, spec: &AppSpec) -> Result<WindowGeometry> {
        let mut state = lock(&self.0)?;
        let started = state
            .call(Operation::Start(spec.clone()), Deadline::UNBOUNDED)
            .and_then(geometry);
        match started {
            Ok(geometry) => Ok(geometry),
            Err(primary) => match state.close(Deadline::from_millis(1500)) {
                Ok(()) => Err(primary),
                Err(cleanup) => Err(GlassError::cleanup_failed(
                    "closing failed private launch",
                    primary,
                    cleanup,
                )),
            },
        }
    }
    fn stop_app_by(&mut self, deadline: Deadline) -> Result<()> {
        lock(&self.0)?.close(deadline)
    }
    fn capture_frame_by(&mut self, region: Option<&Region>, deadline: Deadline) -> Result<Frame> {
        match lock(&self.0)?.call(Operation::Capture(region.copied()), deadline)? {
            Value::Image(value) => crate::backend_worker::frame(&value),
            _ => Err(wrong_response()),
        }
    }
    fn capture_window_by(
        &mut self,
        id: WindowId,
        region: Option<&Region>,
        deadline: Deadline,
    ) -> Result<Frame> {
        match lock(&self.0)?.call(Operation::CaptureWindow(id, region.copied()), deadline)? {
            Value::Image(value) => crate::backend_worker::frame(&value),
            _ => Err(wrong_response()),
        }
    }
    fn send_pointer_by(&mut self, event: &PointerEvent, deadline: Deadline) -> Result<()> {
        lock(&self.0)?
            .call(Operation::Pointer(event.clone()), deadline)
            .and_then(unit)
    }
    fn send_key_by(&mut self, event: &KeyEvent, deadline: Deadline) -> Result<()> {
        lock(&self.0)?
            .call(Operation::Key(event.clone()), deadline)
            .and_then(unit)
    }
    fn get_clipboard(&mut self) -> Result<String> {
        match lock(&self.0)?.call(Operation::GetClipboard, Deadline::UNBOUNDED)? {
            Value::Text(value) => Ok(value),
            _ => Err(wrong_response()),
        }
    }
    fn set_clipboard(&mut self, text: &str) -> Result<()> {
        lock(&self.0)?
            .call(
                Operation::SetClipboard(text.to_owned()),
                Deadline::UNBOUNDED,
            )
            .and_then(unit)
    }
    fn window_by(&mut self, op: &WindowOp, deadline: Deadline) -> Result<WindowGeometry> {
        lock(&self.0)?
            .call(Operation::Window(op.clone()), deadline)
            .and_then(geometry)
    }
    fn list_windows_by(&mut self, deadline: Deadline) -> Result<Vec<WindowInfo>> {
        match lock(&self.0)?.call(Operation::ListWindows, deadline)? {
            Value::Windows(value) => Ok(value),
            _ => Err(wrong_response()),
        }
    }
    fn select_window_by(&mut self, id: WindowId, deadline: Deadline) -> Result<WindowGeometry> {
        lock(&self.0)?
            .call(Operation::SelectWindow(id), deadline)
            .and_then(geometry)
    }
    fn drain_logs(&mut self) -> Vec<(Stream, String)> {
        let Ok(mut state) = lock(&self.0) else {
            return Vec::new();
        };
        if state.log_failure.is_some() {
            return Vec::new();
        }
        match state.call(Operation::Logs, Deadline::from_millis(100)) {
            Ok(Value::Logs(value)) => value,
            Ok(_) => {
                let error = wrong_response();
                let diagnostic = format!("glass: worker log read failed: {error}");
                state.log_failure = Some(error);
                vec![(Stream::Stderr, diagnostic)]
            }
            Err(error) => {
                let diagnostic = format!("glass: worker log read failed: {error}");
                state.log_failure = Some(error);
                vec![(Stream::Stderr, diagnostic)]
            }
        }
    }
    fn app_pid(&self) -> Option<u32> {
        lock(&self.0)
            .ok()
            .and_then(|state| state.properties.app_pid)
    }
    fn observation_window_id(&self) -> Option<WindowId> {
        lock(&self.0).ok().and_then(|state| state.properties.window)
    }
    fn app_pids_by(&self, deadline: Deadline) -> Result<Vec<u32>> {
        match lock(&self.0)?.call(Operation::Pids, deadline)? {
            Value::Pids(value) => Ok(value),
            _ => Err(wrong_response()),
        }
    }
    fn pointer_window_occluded_by(
        &mut self,
        point: (i32, i32),
        deadline: Deadline,
    ) -> Result<bool> {
        match lock(&self.0)?.call(Operation::Occluded(point), deadline)? {
            Value::Bool(value) => Ok(value),
            _ => Err(wrong_response()),
        }
    }
}

impl<C: Connection> Drop for RemotePlatform<C> {
    fn drop(&mut self) {
        if let Ok(mut state) = lock(&self.0)
            && let Err(error) = state.close(Deadline::from_millis(1500))
        {
            eprintln!("glass: owned backend cleanup incomplete: {error}");
        }
    }
}

impl<C: Connection + 'static> Accessibility for RemoteAccessibility<C> {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        match lock(&self.0)?.call(Operation::Snapshot(Context::from_core(ctx)), ctx.deadline)? {
            Value::Tree(tree) => Ok(*tree),
            _ => Err(wrong_response()),
        }
    }
    fn observation_scope(&self) -> Option<AxObservationScope> {
        lock(&self.0)
            .ok()
            .and_then(|state| state.properties.scope_basis)
            .map(|coordinate_basis| AxObservationScope {
                provider: "macos-ax-v1",
                coordinate_basis,
            })
    }
    fn state_coverage(&self) -> AxStateCoverage {
        lock(&self.0)
            .map(|state| state.properties.coverage)
            .unwrap_or_default()
    }
    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        lock(&self.0)?
            .call(
                Operation::SetValue(Context::from_core(ctx), target.clone(), text.to_owned()),
                ctx.deadline,
            )
            .and_then(unit)
    }
    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        lock(&self.0)?
            .call(
                Operation::Invoke(Context::from_core(ctx), target.clone()),
                ctx.deadline,
            )
            .and_then(node)
    }
    fn focus(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        lock(&self.0)?
            .call(
                Operation::Focus(Context::from_core(ctx), target.clone()),
                ctx.deadline,
            )
            .and_then(node)
    }
    fn pointer_target_at(
        &mut self,
        ctx: &AxContext,
        target: &AxTarget,
        point: (i32, i32),
    ) -> Result<PointerHit> {
        match lock(&self.0)?.call(
            Operation::PointerTarget(Context::from_core(ctx), target.clone(), point),
            ctx.deadline,
        )? {
            Value::Hit(value) => Ok(value),
            _ => Err(wrong_response()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::backend_worker::{Dispatcher, Request, VERSION};
    use crate::tools::testutil::FakePlatform;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Reader;
    impl Accessibility for Reader {
        fn snapshot(&mut self, _ctx: &AxContext) -> Result<AxTree> {
            Ok(AxTree::new(AxNode {
                id: AxNodeId(0),
                role: AxRole::TextField,
                raw_role: "field".into(),
                name: Some("Note".into()),
                description: None,
                value: Some("Unicode Ω".into()),
                states: AxStates::default(),
                bounds: Some(AxRect {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                }),
                children: Vec::new(),
            }))
        }
    }

    struct Local {
        dispatcher: Dispatcher<FakePlatform, Reader>,
        closes: Arc<AtomicUsize>,
    }
    impl Connection for Local {
        fn call(&mut self, operation: Operation, deadline: Deadline) -> Result<Response> {
            let request = Request {
                version: VERSION,
                id: 1,
                expires_ns: crate::backend_worker::expires(deadline)?,
                operation,
            };
            let request: Request =
                serde_json::from_slice(&serde_json::to_vec(&request).unwrap()).unwrap();
            let response = self.dispatcher.handle(request);
            Ok(serde_json::from_slice(&serde_json::to_vec(&response).unwrap()).unwrap())
        }
        fn close(&mut self, deadline: Deadline) -> Result<()> {
            let _ = self.dispatcher.current_properties();
            self.dispatcher.stop(deadline)?;
            self.closes.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    fn local() -> Local {
        Local {
            dispatcher: Dispatcher::new(FakePlatform::new(4, 4), Reader),
            closes: Arc::new(AtomicUsize::new(0)),
        }
    }

    struct Changing {
        inner: Local,
        lost: Arc<std::sync::atomic::AtomicBool>,
    }
    impl Connection for Changing {
        fn call(&mut self, operation: Operation, deadline: Deadline) -> Result<Response> {
            let info = matches!(operation, Operation::Info);
            let mut response = self.inner.call(operation, deadline)?;
            if info {
                let lost = self.lost.load(Ordering::SeqCst);
                response.properties.input = InputCapabilities::uniform(
                    if lost {
                        CapabilityStatus::requires_setup("private attachment unavailable")
                    } else {
                        CapabilityStatus::supported()
                    },
                    if lost {
                        DesktopInterference::Unknown
                    } else {
                        DesktopInterference::None
                    },
                );
                if lost {
                    response.result =
                        Err(GlassError::Backend("private attachment unavailable".into())
                            .before_dispatch());
                }
            }
            Ok(response)
        }
        fn close(&mut self, deadline: Deadline) -> Result<()> {
            self.inner.close(deadline)
        }
    }

    #[test]
    fn capability_query_refreshes_private_attachment_without_dispatching_input() {
        let lost = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let backend = backend(Changing {
            inner: local(),
            lost: Arc::clone(&lost),
        })
        .unwrap();
        assert_eq!(
            backend
                .platform
                .input_capabilities()
                .click
                .desktop_interference,
            DesktopInterference::None
        );
        lost.store(true, Ordering::SeqCst);
        let report = backend.platform.input_capabilities();
        assert_eq!(report.click.support.status, Support::RequiresSetup);
        assert_eq!(
            report.click.desktop_interference,
            DesktopInterference::Unknown
        );
    }

    struct RetryClose {
        inner: Local,
        attempts: Arc<AtomicUsize>,
    }
    impl Connection for RetryClose {
        fn call(&mut self, operation: Operation, deadline: Deadline) -> Result<Response> {
            self.inner.call(operation, deadline)
        }
        fn close(&mut self, deadline: Deadline) -> Result<()> {
            if self.attempts.fetch_add(1, Ordering::SeqCst) == 0 {
                Err(GlassError::Backend("cleanup still pending".into()))
            } else {
                self.inner.close(deadline)
            }
        }
    }

    #[test]
    fn failed_backend_close_remains_retryable() {
        let attempts = Arc::new(AtomicUsize::new(0));
        let mut backend = backend(RetryClose {
            inner: local(),
            attempts: Arc::clone(&attempts),
        })
        .unwrap();
        assert!(backend.platform.stop_app().is_err());
        backend.platform.stop_app().unwrap();
        backend.platform.stop_app().unwrap();
        assert_eq!(attempts.load(Ordering::SeqCst), 2);
    }

    struct LostLogs(Local);
    impl Connection for LostLogs {
        fn call(&mut self, operation: Operation, deadline: Deadline) -> Result<Response> {
            if matches!(operation, Operation::Logs) {
                return Err(GlassError::Backend("log transport lost".into()).after_dispatch());
            }
            self.0.call(operation, deadline)
        }
        fn close(&mut self, deadline: Deadline) -> Result<()> {
            self.0.close(deadline)
        }
    }

    #[test]
    fn a_prior_log_failure_does_not_mark_a_later_unsent_input_as_dispatched() {
        let mut backend = backend(LostLogs(local())).unwrap();
        let logs = backend.platform.drain_logs();
        assert!(
            logs.iter()
                .any(|(_, text)| text.contains("worker log read failed"))
        );
        let error = backend
            .platform
            .send_key_by(&KeyEvent::Text("not sent".into()), Deadline::UNBOUNDED)
            .unwrap_err();
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert!(backend.platform.drain_logs().is_empty());
    }

    fn connected() -> (Backend, Arc<Mutex<Vec<AppSpec>>>, Arc<AtomicUsize>) {
        let specs = Arc::new(Mutex::new(Vec::new()));
        let closes = Arc::new(AtomicUsize::new(0));
        let mut platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [12, 34, 56, 255])]);
        platform.specs = Arc::clone(&specs);
        platform.protected_paths = Some(Arc::new(Mutex::new(Vec::new())));
        let connection = Local {
            dispatcher: Dispatcher::new(platform, Reader),
            closes: Arc::clone(&closes),
        };
        (backend(connection).unwrap(), specs, closes)
    }

    fn server() -> crate::server::GlassServer {
        let store = tempfile::tempdir().unwrap().keep();
        let glass = Glass::new(
            Box::new(|_| Ok(connected().0)),
            "macos".into(),
            BaselineStore::new(store),
            100,
        );
        crate::server::GlassServer::new(glass, crate::audit::report_from_config(None, |_| None))
    }

    async fn tool_loop<C>(client: &rmcp::service::RunningService<rmcp::RoleClient, C>)
    where
        C: rmcp::ClientHandler,
    {
        use rmcp::model::CallToolRequestParams;
        let call = |name: &str, arguments: serde_json::Value| {
            CallToolRequestParams::new(name.to_owned())
                .with_arguments(arguments.as_object().unwrap().clone())
        };
        let started = client.call_tool(call("glass_start",serde_json::json!({"run":["./app"],"backend":"macos","cwd":"../other","env":{"CUSTOM":"Ω"}}))).await.unwrap();
        assert_ne!(started.is_error, Some(true));
        let image = client
            .call_tool(call("glass_screenshot", serde_json::json!({})))
            .await
            .unwrap();
        assert_ne!(image.is_error, Some(true));
        assert!(image.content.iter().any(|item| item.as_image().is_some()));
        let stopped = client
            .call_tool(call("glass_stop", serde_json::json!({})))
            .await
            .unwrap();
        assert_ne!(stopped.is_error, Some(true));
    }

    #[tokio::test]
    async fn stdio_server_drives_the_backend_adapter_through_public_tools() {
        use rmcp::ServiceExt;
        let (client_io, server_io) = tokio::io::duplex(1024 * 1024);
        let server = tokio::spawn(async {
            server()
                .serve(server_io)
                .await
                .unwrap()
                .waiting()
                .await
                .unwrap()
        });
        let client = ().serve(client_io).await.unwrap();
        tool_loop(&client).await;
        client.cancel().await.unwrap();
        server.await.unwrap();
    }

    #[cfg(feature = "network")]
    #[tokio::test]
    async fn http_server_drives_the_same_backend_adapter_through_public_tools() {
        use rmcp::ServiceExt;
        use rmcp::transport::StreamableHttpClientTransport;
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let address = listener.local_addr().unwrap();
        let (finish, stopped) = tokio::sync::oneshot::channel();
        let cfg = crate::serve::config::ServeConfig {
            addr: address,
            token: None,
            tool_profile: crate::tool_profile::ToolProfile::Full,
            trace: None,
        };
        let task = tokio::spawn(crate::serve::run_server_on_until(
            listener,
            cfg,
            server(),
            async {
                let _ = stopped.await;
            },
        ));
        let client = ()
            .serve(StreamableHttpClientTransport::from_uri(format!(
                "http://{address}/mcp"
            )))
            .await
            .unwrap();
        tool_loop(&client).await;
        client.cancel().await.unwrap();
        finish.send(()).unwrap();
        task.await.unwrap().unwrap();
    }

    #[test]
    fn platform_adapter_preserves_spec_pixels_and_one_connection_lifetime() {
        let (mut backend, specs, closes) = connected();
        let spec = AppSpec {
            build: Some("build --flag".into()),
            run: vec!["./new/app".into(), "Ω".into()],
            cwd: Some("../changed".into()),
            env: vec![("CUSTOM".into(), "unchanged".into())],
            window_hint: Some(WindowHint {
                title: Some("new title".into()),
                class: None,
            }),
            timeout_ms: 1734,
            sandbox: SandboxLevel::Strict,
            a11y: true,
        };
        backend.platform.start_app(&spec).unwrap();
        assert_eq!(specs.lock().unwrap().as_slice(), [spec]);
        assert_eq!(
            backend.platform.capture_frame(None).unwrap(),
            Frame::solid(4, 4, [12, 34, 56, 255])
        );
        backend.platform.stop_app().unwrap();
        drop(backend);
        assert_eq!(closes.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn accessibility_context_cannot_address_an_unowned_process() {
        let (mut backend, _, _) = connected();
        let mut ctx = AxContext {
            pids: vec![4242],
            window: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
            deadline: Deadline::UNBOUNDED,
        };
        let reader = backend.accessibility.as_mut().unwrap();
        assert_eq!(
            reader.snapshot(&ctx).unwrap().root.value.as_deref(),
            Some("Unicode Ω")
        );
        ctx.pids = vec![9];
        let error = reader.snapshot(&ctx).unwrap_err();
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
    }

    #[test]
    fn deadline_transmission_does_not_reset_a_spent_budget() {
        let (mut backend, _, _) = connected();
        let error = backend
            .platform
            .capture_frame_by(None, Deadline::from_millis(0))
            .unwrap_err();
        assert_eq!(error.bound_dispatch(), Some(BoundDispatch::NotDispatched));
        assert_eq!(error.bound_owner(), Some(Whose::Caller));
    }
}
