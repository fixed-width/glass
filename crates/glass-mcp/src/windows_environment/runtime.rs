use super::files::Root;
use crate::backend_worker::{self, Operation, Properties, Request, Response, Value};
use anyhow::{Context, Result, bail, ensure};
use glass_core::{Deadline, DesktopInterference, GlassError, InputRoute};
use glass_windows::desktop::{LocalPipe, ProcessIdentity};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use std::os::windows::process::CommandExt;
use std::path::PathBuf;
use std::process::{Child, Command, Stdio};
use std::sync::{Arc, Mutex};
use std::time::Duration;

mod payload {
    include!(concat!(env!("OUT_DIR"), "/windows_desktop_payload.rs"));
}

#[cfg(test)]
pub(super) use payload::INTEROP as TEST_INTEROP;

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Launch {
    pub version: u32,
    pub instance: String,
    pub pipe: String,
    pub parent: ProcessIdentity,
    pub helper: Option<ProcessIdentity>,
    pub worker_exe: PathBuf,
}

#[derive(Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Journal {
    pub version: u32,
    pub child: u32,
    pub instance: String,
    pub phase: String,
    pub error: Option<String>,
    pub task: String,
    pub worker_arguments: Option<String>,
    pub pipe: String,
    pub stop_event: String,
    pub show_event: String,
    pub hide_event: String,
    pub parent: ProcessIdentity,
    pub owner: ProcessIdentity,
    pub worker: Option<ProcessIdentity>,
    pub child_logon: i64,
    #[serde(default)]
    pub child_pending: bool,
    #[serde(default)]
    pub child_gone: bool,
    pub child_enabled_before: bool,
    pub changed_child_setting: bool,
    pub clean: bool,
    pub viewer_visible: bool,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct Hello {
    pub version: u32,
    pub instance: String,
    pub process: ProcessIdentity,
    pub context: Option<ExecutionContext>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct ExecutionContext {
    pub cwd: PathBuf,
    pub path: Option<String>,
    pub providers: glass_windows::containment::ProviderSettings,
}

pub(super) fn require_payload() -> Result<()> {
    ensure!(
        payload::HOST.starts_with(b"MZ") && payload::INTEROP.starts_with(b"MZ"),
        "this build has no packaged Windows desktop helper; use a native Windows build or supply GLASS_DESKTOP_HELPER_DIR at build time"
    );
    Ok(())
}

pub(super) fn helper(base: &Root) -> Result<PathBuf> {
    require_payload()?;
    let mut hash = Sha256::new();
    hash.update(payload::HOST);
    hash.update(payload::INTEROP);
    let hash = hash
        .finalize()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    let assets = base
        .child(&format!("assets-{hash}"), true)?
        .context("private desktop helper directory")?;
    for (name, expected) in [
        ("GlassDesktopHost.exe", payload::HOST),
        ("GlassDesktopRdp.dll", payload::INTEROP),
    ] {
        match assets.bytes(name, 32 * 1024 * 1024)? {
            Some(bytes) => ensure!(
                bytes == expected,
                "packaged Windows desktop helper changed on disk"
            ),
            None => assets.write_bytes(name, expected)?,
        }
    }
    Ok(assets.path.join("GlassDesktopHost.exe"))
}

pub(super) fn validate_journal(journal: &Journal, current: &ProcessIdentity) -> Result<()> {
    ensure!(
        journal.version == 1
            && journal.instance.len() == 32
            && journal
                .instance
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit()),
        "unsupported Windows desktop ownership record"
    );
    ensure!(
        journal.parent.sid == current.sid
            && journal.owner.sid == current.sid
            && journal.parent.session != 0
            && journal.owner.session == journal.parent.session
            && journal.owner.logon == journal.parent.logon,
        "Windows desktop record belongs to another user or logon"
    );
    ensure!(
        journal.pipe == format!("glass-desktop-{}", journal.instance)
            && journal.task == format!("Glass.Desktop.{}", journal.instance),
        "desktop endpoint or task does not match its launch"
    );
    ensure!(
        journal.stop_event == format!("Global\\Glass.Desktop.Stop.{}", journal.instance)
            && journal.show_event == format!("Local\\Glass.Desktop.Show.{}", journal.instance)
            && journal.hide_event == format!("Local\\Glass.Desktop.Hide.{}", journal.instance),
        "desktop control event does not match its launch"
    );
    ensure!(
        journal
            .owner
            .image
            .file_name()
            .is_some_and(|name| name == "GlassDesktopHost.exe"),
        "desktop owner executable does not match its role"
    );
    if journal.child != u32::MAX {
        ensure!(
            journal.child != 0
                && journal.child != journal.parent.session
                && journal.child_logon > 0,
            "invalid child-session generation"
        );
    }
    ensure!(
        !journal.child_pending || (!journal.child_gone && !journal.clean),
        "unresolved child identity cannot confirm cleanup"
    );
    if let Some(worker) = &journal.worker {
        ensure!(
            worker.sid == current.sid
                && worker.session == journal.child
                && worker.image == journal.parent.image,
            "worker record is outside the owned desktop"
        );
    }
    Ok(())
}

pub(super) fn journals(base: &Root) -> Result<Vec<(Root, Journal)>> {
    let mut result = Vec::new();
    for name in base
        .names()?
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().starts_with("environment-"))
    {
        ensure!(
            result.len() < 64,
            "too many retained Windows desktop records; inspect cleanup before starting"
        );
        let root = base
            .child(&name, false)?
            .context("desktop ownership directory disappeared")?;
        if let Some(journal) = root.read::<Journal>("owner.json")? {
            ensure!(
                name.eq_ignore_ascii_case(&format!("environment-{}", journal.instance)),
                "desktop journal directory does not match its instance"
            );
            result.push((root, journal));
        }
    }
    Ok(result)
}

pub(super) fn pending_starts(
    base: &Root,
    current: &ProcessIdentity,
) -> Result<Vec<serde_json::Value>> {
    let mut result = Vec::new();
    for name in base
        .names()?
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().starts_with("environment-"))
    {
        let root = base
            .child(&name, false)?
            .context("desktop startup disappeared")?;
        if root.read::<Journal>("owner.json")?.is_some() {
            continue;
        }
        let intent = root
            .read::<Launch>("intent.json")?
            .context("desktop startup metadata is missing; the directory was retained")?;
        ensure!(
            intent.version == 1
                && intent.parent.sid == current.sid
                && name.eq_ignore_ascii_case(&format!("environment-{}", intent.instance)),
            "pending startup has an invalid origin"
        );
        let parent_alive = intent.parent.is_current()?;
        let authorization = root.read::<Launch>("launch.json")?;
        let helper_alive = match authorization {
            Some(launch) => {
                ensure!(
                    launch.parent == intent.parent && launch.instance == intent.instance,
                    "pending startup authorization changed"
                );
                launch
                    .helper
                    .map(|helper| helper.is_current())
                    .transpose()?
                    .unwrap_or(false)
            }
            None => false,
        };
        result.push(serde_json::json!({
            "phase": if parent_alive || helper_alive { "pending_startup" } else { "recovery_required" },
            "parent_session": intent.parent.session,
            "parent_alive": parent_alive,
            "owner_alive": helper_alive,
            "clean": false,
            "error": "Startup has not published an owner; close its MCP server and run desktop recover."
        }));
    }
    Ok(result)
}

pub(super) fn archive_finished(base: &Root, current: &ProcessIdentity) -> Result<()> {
    for name in base
        .names()?
        .into_iter()
        .filter(|name| name.to_ascii_lowercase().starts_with("environment-"))
    {
        let root = base
            .child(&name, false)?
            .context("desktop state disappeared")?;
        if let Some(journal) = root.read::<Journal>("owner.json")? {
            validate_journal(&journal, current)?;
            if journal.clean
                && journal.phase == "stopped"
                && !journal.owner.is_current()?
                && !journal.parent.is_current()?
            {
                ensure!(
                    !journal
                        .worker
                        .as_ref()
                        .map(|worker| worker.is_current())
                        .transpose()?
                        .unwrap_or(false),
                    "clean desktop still has a live worker"
                );
                root.validate()?;
                base.validate()?;
                std::fs::rename(
                    &root.path,
                    base.path.join(format!("history-{}", journal.instance)),
                )?;
            }
        } else if let Some(intent) = root.read::<Launch>("intent.json")? {
            ensure!(
                intent.version == 1
                    && intent.parent.sid == current.sid
                    && name.eq_ignore_ascii_case(&format!("environment-{}", intent.instance)),
                "pending startup has an invalid origin"
            );
            if !intent.parent.is_current()? {
                let authorization = root.read::<Launch>("launch.json")?;
                if let Some(helper) = authorization
                    .as_ref()
                    .and_then(|record| record.helper.as_ref())
                {
                    ensure!(
                        !helper.is_current()?,
                        "pending desktop host is still running; finish its stop before recovery"
                    );
                }
                // No host may mutate a session before publishing owner.json. Keep abandoned
                // launch metadata for inspection instead of deleting it or guessing an owner.
                root.validate()?;
                base.validate()?;
                std::fs::rename(
                    &root.path,
                    base.path
                        .join(format!("history-unstarted-{}", intent.instance)),
                )?;
            } else {
                bail!("another Windows desktop startup is pending in this account");
            }
        } else {
            bail!("Windows desktop startup metadata is missing; the directory was retained");
        }
    }
    Ok(())
}

fn read_journal(root: &Root, current: &ProcessIdentity) -> Result<Journal> {
    let journal = root
        .read::<Journal>("owner.json")?
        .context("desktop owner has not published its state")?;
    validate_journal(&journal, current)?;
    Ok(journal)
}

pub(super) fn control(root: &Root, journal: &Journal, show: bool) -> Result<()> {
    root.validate()?;
    glass_windows::desktop::signal_control(if show {
        &journal.show_event
    } else {
        &journal.hide_event
    })?;
    Ok(())
}

pub(super) fn stop_external(root: &Root, journal: &Journal, deadline: Deadline) -> Result<Journal> {
    let current = ProcessIdentity::current()?;
    validate_journal(journal, &current)?;
    ensure!(
        journal.owner.is_current()?,
        "desktop owner exited; use desktop recover"
    );
    root.write(
        "stop.json",
        &serde_json::json!({"version":1,"instance":journal.instance}),
    )?;
    if let Err(error) = glass_windows::desktop::signal_stop(&journal.stop_event) {
        let state = read_journal(root, &current)?;
        ensure!(
            state.owner == journal.owner && native_cleanup_finished(&state)?,
            "{error}"
        );
    }
    let finalizing_grace = std::time::Instant::now() + Duration::from_secs(2);
    let forced = wait_for_owner(
        deadline,
        finalizing_grace,
        || journal.owner.is_current().map_err(Into::into),
        || {
            let state = read_journal(root, &current)?;
            ensure!(
                state.owner == journal.owner,
                "desktop owner changed during stop"
            );
            if native_cleanup_finished(&state)? {
                // The host publishes finalizing only after child/task/application cleanup. Its
                // remaining resource is its own control process, whose COM disposal can stall.
                state.owner.terminate(deadline)?;
                return Ok(true);
            }
            Ok(false)
        },
    )?;
    let mut final_state = read_journal(root, &current)?;
    ensure!(
        final_state.owner == journal.owner && final_state.instance == journal.instance,
        "Windows desktop owner changed during stop"
    );
    if forced && final_state.phase == "finalizing" {
        final_state.phase = "stopped".into();
        final_state.clean = true;
        final_state.viewer_visible = false;
        root.write("owner.json", &final_state)?;
    }
    ensure!(
        final_state.clean && final_state.phase == "stopped",
        "Windows desktop cleanup incomplete: {}",
        final_state
            .error
            .as_deref()
            .unwrap_or("owner exited without confirming cleanup")
    );
    Ok(final_state)
}

fn wait_for_owner(
    deadline: Deadline,
    finalizing_grace: std::time::Instant,
    mut is_current: impl FnMut() -> Result<bool>,
    mut finish: impl FnMut() -> Result<bool>,
) -> Result<bool> {
    while is_current()? {
        ensure!(
            !deadline.has_passed(),
            "Windows desktop teardown is still pending; its ownership record was retained"
        );
        if std::time::Instant::now() >= finalizing_grace && finish()? {
            return Ok(true);
        }
        std::thread::sleep(Duration::from_millis(25));
    }
    Ok(false)
}

fn native_cleanup_finished(state: &Journal) -> Result<bool> {
    if state.child_pending
        || !state.child_gone
        || !(state.phase == "finalizing" || (state.phase == "stopped" && state.clean))
    {
        return Ok(false);
    }
    Ok(!state
        .worker
        .as_ref()
        .map(ProcessIdentity::is_current)
        .transpose()?
        .unwrap_or(false))
}

pub(super) fn recover(
    base: &Root,
    root: &Root,
    journal: &Journal,
    deadline: Deadline,
) -> Result<Journal> {
    let current = ProcessIdentity::current()?;
    validate_journal(journal, &current)?;
    ensure!(
        !journal.owner.is_current()?,
        "cannot recover a live Windows desktop owner"
    );
    let mut process = Command::new(helper(base)?)
        .arg(&root.path)
        .arg("recover")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("start owned desktop recovery")?;
    let first_attempt = wait(&mut process, deadline);
    let intermediate = read_journal(root, &current)?;
    ensure!(
        intermediate.child_gone
            && !intermediate
                .worker
                .as_ref()
                .map(|worker| worker.is_current())
                .transpose()?
                .unwrap_or(false),
        "recovery did not establish child and worker teardown: {}",
        first_attempt
            .as_ref()
            .err()
            .map(ToString::to_string)
            .unwrap_or_default()
    );
    match super::work::Work::open(root, &journal.instance, false)? {
        Some(work) => work.recover(deadline)?,
        None => ensure!(
            journal.worker.is_none(),
            "container ownership journal is missing; cleanup cannot be confirmed"
        ),
    }
    root.write("receipt.json", &serde_json::json!({"version":1,"instance":journal.instance,"clean":true,"error":null,"recovered":true}))?;
    let mut finish = Command::new(helper(base)?)
        .arg(&root.path)
        .arg("recover")
        .creation_flags(0x0800_0000)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .spawn()
        .context("finish owned desktop recovery")?;
    wait(&mut finish, deadline)?;
    let final_state = read_journal(root, &current)?;
    ensure!(
        final_state.clean && final_state.phase == "stopped",
        "Windows desktop recovery remains incomplete: {}",
        final_state
            .error
            .as_deref()
            .unwrap_or("inspect the retained owner record")
    );
    Ok(final_state)
}

fn wait(process: &mut Child, deadline: Deadline) -> Result<()> {
    loop {
        if let Some(status) = process.try_wait()? {
            ensure!(status.success(), "desktop helper exited with {status}");
            return Ok(());
        }
        ensure!(
            !deadline.has_passed(),
            "desktop helper is still finishing cleanup; its ownership record was retained"
        );
        std::thread::sleep(Duration::from_millis(25));
    }
}

struct State {
    helper: Child,
    journal: Journal,
    pipe: Option<LocalPipe>,
    next_id: u64,
    leased: bool,
    app_stopped: bool,
    stopped: bool,
}

pub(super) struct Runtime {
    pub root: Root,
    revision: String,
    state: Mutex<State>,
}

impl Runtime {
    #[cfg(feature = "network")]
    pub fn start(
        base: Root,
        revision: String,
        cancel: &std::sync::atomic::AtomicBool,
    ) -> Result<Self> {
        let parent = ProcessIdentity::current()?;
        ensure!(
            parent.session != 0,
            "start separate desktop development from your signed-in Windows desktop, not Session 0"
        );
        archive_finished(&base, &parent)?;
        for (root, journal) in journals(&base)? {
            validate_journal(&journal, &parent)?;
            ensure!(
                !journal.owner.is_current()?,
                "another Windows desktop owner is active; stop that environment before starting another server"
            );
            if !journal.clean {
                recover(&base, &root, &journal, Deadline::from_millis(30_000))?;
            }
        }
        let executable = helper(&base)?;
        let instance = format!("{:032x}", rand::random::<u128>());
        let root = base
            .child(&format!("environment-{instance}"), true)?
            .context("new desktop ownership directory")?;
        let mut launch = Launch {
            version: 1,
            instance: instance.clone(),
            pipe: format!("glass-desktop-{instance}"),
            parent: parent.clone(),
            helper: None,
            worker_exe: parent.image.clone(),
        };
        root.write("intent.json", &launch)?;
        glass_windows::desktop::protect_stdio_inheritance()?;
        let mut actor = Command::new(executable)
            .arg(&root.path)
            .creation_flags(0x0800_0000)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn()
            .context("start packaged Windows desktop owner")?;
        launch.helper = Some(ProcessIdentity::read(actor.id())?);
        root.write("launch.json", &launch)?;
        let deadline = Deadline::from_millis(125_000);
        let result = (|| {
            loop {
                ensure!(
                    !cancel.load(std::sync::atomic::Ordering::Acquire),
                    "Desktop preparation cancelled"
                );
                if let Some(journal) = root.read::<Journal>("owner.json")? {
                    validate_journal(&journal, &parent)?;
                    ensure!(
                        Some(&journal.owner) == launch.helper.as_ref()
                            && journal.parent == parent
                            && journal.instance == instance,
                        "desktop owner does not match the process launched by this server"
                    );
                    if journal.phase == "ready" {
                        let worker = journal
                            .worker
                            .as_ref()
                            .context("ready desktop has no worker")?;
                        ensure!(worker.is_current()?, "ready desktop worker has exited");
                        let mut pipe =
                            LocalPipe::connect(&launch.pipe, Deadline::from_millis(10_000))?;
                        ensure!(
                            pipe.server_identity()? == *worker,
                            "desktop pipe belongs to a different process generation"
                        );
                        let hello = Hello {
                            version: backend_worker::VERSION,
                            instance: instance.clone(),
                            process: parent.clone(),
                            context: Some(ExecutionContext {
                                cwd: std::env::current_dir()?,
                                path: std::env::var("PATH").ok(),
                                providers: glass_windows::containment::ProviderSettings::capture()?,
                            }),
                        };
                        pipe.write_frame(
                            &serde_json::to_vec(&hello)?,
                            Deadline::from_millis(5_000),
                        )?;
                        let answer: Hello = serde_json::from_slice(
                            &pipe.read_frame(Deadline::from_millis(10_000))?,
                        )?;
                        ensure!(
                            answer.version == backend_worker::VERSION
                                && answer.instance == instance
                                && answer.process == *worker,
                            "desktop worker greeting does not match its authenticated peer"
                        );
                        return Ok((journal, pipe));
                    }
                    ensure!(
                        journal.phase != "cleanup_failed" && journal.phase != "stopped",
                        "desktop startup failed: {}",
                        journal.error.as_deref().unwrap_or("startup was cancelled")
                    );
                }
                if let Some(status) = actor.try_wait()? {
                    let error = root
                        .bytes("host-error.txt", 64 * 1024)?
                        .map(|bytes| String::from_utf8_lossy(&bytes).into_owned())
                        .unwrap_or_else(|| format!("owner exited with {status}"));
                    bail!("desktop startup failed: {error}");
                }
                ensure!(
                    !deadline.has_passed(),
                    "Windows desktop startup timed out; complete any native Windows sign-in locally and inspect desktop status"
                );
                std::thread::sleep(Duration::from_millis(25));
            }
        })();
        match result {
            Ok((journal, pipe)) => Ok(Self {
                root,
                revision,
                state: Mutex::new(State {
                    helper: actor,
                    journal,
                    pipe: Some(pipe),
                    next_id: 0,
                    leased: false,
                    app_stopped: true,
                    stopped: false,
                }),
            }),
            Err(primary) => {
                if let Some(journal) = root.read::<Journal>("owner.json")?
                    && journal.owner.is_current()?
                    && let Err(cleanup) =
                        stop_external(&root, &journal, Deadline::from_millis(30_000))
                {
                    bail!("{primary:#}; cleanup also failed: {cleanup:#}");
                }
                Err(primary)
            }
        }
    }

    pub fn acquire(self: &Arc<Self>) -> Result<Connection> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Windows desktop transport is poisoned"))?;
        self.guard(&state, false)?;
        ensure!(
            !state.leased,
            "another application owns the Windows desktop worker"
        );
        state.leased = true;
        Ok(Connection {
            owner: Arc::clone(self),
            closed: false,
        })
    }

    #[cfg(feature = "network")]
    pub fn ready(&self) -> Result<bool> {
        match self.state.try_lock() {
            Ok(state) => {
                self.guard(&state, false)?;
                Ok(state.pipe.as_ref().is_some_and(LocalPipe::is_healthy))
            }
            Err(std::sync::TryLockError::WouldBlock) => Ok(true),
            Err(_) => bail!("Windows desktop transport is poisoned"),
        }
    }

    #[cfg(feature = "network")]
    pub fn viewer_visible(&self) -> Result<Option<bool>> {
        let state = match self.state.try_lock() {
            Ok(state) => state,
            Err(std::sync::TryLockError::WouldBlock) => return Ok(None),
            Err(_) => bail!("Windows desktop transport is poisoned"),
        };
        self.guard(&state, false)?;
        let journal = read_journal(&self.root, &state.journal.parent)?;
        ensure!(
            journal.instance == state.journal.instance && journal.owner == state.journal.owner,
            "Windows desktop viewer identity changed"
        );
        Ok((journal.phase == "ready").then_some(journal.viewer_visible))
    }

    #[cfg(feature = "network")]
    pub fn viewer(&self, show: bool) -> Result<()> {
        let state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Windows desktop transport is poisoned"))?;
        self.guard(&state, false)?;
        control(&self.root, &state.journal, show)
    }

    fn guard(&self, state: &State, stopping: bool) -> Result<()> {
        ensure!(
            !state.stopped,
            "Windows desktop was stopped; restart this MCP server before creating another environment"
        );
        ensure!(
            state.journal.owner.is_current()?
                && state
                    .journal
                    .worker
                    .as_ref()
                    .map(|worker| worker.is_current())
                    .transpose()?
                    .unwrap_or(false),
            "Windows desktop owner or worker exited; no request was dispatched"
        );
        if !stopping {
            ensure!(
                !glass_windows::desktop::stop_requested(&state.journal.instance)?,
                "Windows desktop stop was requested; no request was dispatched"
            );
            ensure!(
                self.root.read::<serde_json::Value>("stop.json")?.is_none(),
                "Windows desktop is stopping; no request was dispatched"
            );
            let configuration = super::native::configuration()?;
            ensure!(
                configuration.revision == self.revision && !configuration.setup_pending,
                "Windows desktop configuration changed; restart this MCP server"
            );
        }
        Ok(())
    }

    fn call_locked(
        &self,
        state: &mut State,
        operation: Operation,
        deadline: Deadline,
    ) -> glass_core::Result<Response> {
        let stopping = matches!(&operation, Operation::Stop);
        if stopping
            && !state
                .journal
                .owner
                .is_current()
                .map_err(|error| GlassError::Backend(error.to_string()))?
        {
            let final_state = read_journal(&self.root, &state.journal.parent)
                .map_err(|error| GlassError::Backend(error.to_string()))?;
            if final_state.clean && final_state.child_gone && final_state.phase == "stopped" {
                state.stopped = true;
                state.app_stopped = true;
                state.leased = false;
                state.pipe = None;
                return Ok(Response {
                    version: backend_worker::VERSION,
                    id: state.next_id,
                    properties: Properties {
                        recording_supported: false,
                        buffer_supported: false,
                        recording_scope: glass_core::RecordingScope::OwnedDisplay,
                        input_route: InputRoute::Isolated,
                        input: glass_core::InputCapabilities::unknown(),
                        app_pid: None,
                        window: None,
                        window_handle: None,
                        scope_basis: None,
                        coverage: Default::default(),
                    },
                    result: Ok(Value::Unit),
                });
            }
        }
        self.guard(state, stopping)
            .map_err(|error| GlassError::Backend(error.to_string()).before_dispatch())?;
        if matches!(&operation, Operation::Start(_)) {
            state.app_stopped = false;
        }
        let bound = request_deadline(&operation, deadline);
        state.next_id = state
            .next_id
            .checked_add(1)
            .ok_or_else(|| GlassError::Backend("desktop request sequence exhausted".into()))?;
        let id = state.next_id;
        let request = Request {
            version: backend_worker::VERSION,
            id,
            expires_ns: backend_worker::expires(bound)?,
            operation,
        };
        let bytes =
            serde_json::to_vec(&request).map_err(|error| GlassError::Backend(error.to_string()))?;
        let pipe = state.pipe.as_mut().ok_or_else(|| {
            GlassError::Backend("Windows desktop transport was closed".into()).before_dispatch()
        })?;
        pipe.write_frame(&bytes, bound)?;
        let result = pipe.read_frame(bound).map_err(|error| {
            if bound.has_passed() {
                GlassError::caller_deadline_elapsed("Windows desktop worker response")
            } else {
                error
            }
        })?;
        let mut response: Response = serde_json::from_slice(&result).map_err(|error| {
            GlassError::Backend(format!("invalid desktop worker response: {error}"))
        })?;
        if response.version != backend_worker::VERSION || response.id != id {
            state.pipe = None;
            return Err(GlassError::Backend(
                "desktop response sequence or protocol changed".into(),
            ));
        }
        if stopping && response.result.is_ok() {
            state.app_stopped = true;
        }
        response.properties.input_route = InputRoute::Isolated;
        for capability in [
            &mut response.properties.input.click,
            &mut response.properties.input.scroll,
            &mut response.properties.input.text,
        ] {
            capability.desktop_interference = DesktopInterference::None;
        }
        Ok(response)
    }

    pub fn shutdown(&self, deadline: Deadline) -> Result<()> {
        let mut state = self
            .state
            .lock()
            .map_err(|_| anyhow::anyhow!("Windows desktop transport is poisoned"))?;
        if state.stopped {
            return Ok(());
        }
        let primary = if state
            .journal
            .worker
            .as_ref()
            .map(|worker| worker.is_current())
            .transpose()?
            .unwrap_or(false)
        {
            self.call_locked(&mut state, Operation::Stop, deadline)
                .and_then(|response| response.result.map(|_| ()))
                .err()
        } else {
            None
        };
        self.root.write(
            "stop.json",
            &serde_json::json!({"version":1,"instance":state.journal.instance}),
        )?;
        let cleanup = if state.journal.owner.is_current()? {
            stop_external(&self.root, &state.journal, deadline).map(|_| ())
        } else {
            let journal = read_journal(&self.root, &ProcessIdentity::current()?)?;
            ensure!(
                journal.clean,
                "desktop owner exited with unfinished cleanup"
            );
            Ok(())
        };
        state.pipe = None;
        if cleanup.is_ok() {
            state.stopped = true;
            state.leased = false;
        }
        let _ = state.helper.try_wait();
        match (primary, cleanup) {
            (None, result) => result,
            (Some(error), Ok(())) => Err(error.into()),
            (Some(error), Err(cleanup)) => {
                bail!("{error}; environment cleanup also failed: {cleanup:#}")
            }
        }
    }
}

fn request_deadline(operation: &Operation, deadline: Deadline) -> Deadline {
    // Start includes the build, which has no implicit time limit.
    if matches!(operation, Operation::Start(_)) {
        deadline
    } else {
        Deadline::at(
            deadline
                .resolve(std::time::Instant::now() + Duration::from_secs(120))
                .0,
        )
    }
}

impl Drop for Runtime {
    fn drop(&mut self) {
        if let Err(error) = self.shutdown(Deadline::from_millis(5_000)) {
            eprintln!("glass: Windows desktop cleanup incomplete: {error:#}");
        }
    }
}

pub(super) struct Connection {
    owner: Arc<Runtime>,
    closed: bool,
}

impl crate::remote_backend::Connection for Connection {
    fn call(&mut self, operation: Operation, deadline: Deadline) -> glass_core::Result<Response> {
        if self.closed {
            return Err(GlassError::NoActiveSession.before_dispatch());
        }
        let mut state = self
            .owner
            .state
            .lock()
            .map_err(|_| GlassError::Backend("Windows desktop transport is poisoned".into()))?;
        self.owner.call_locked(&mut state, operation, deadline)
    }

    fn close(&mut self, _deadline: Deadline) -> glass_core::Result<()> {
        if self.closed {
            return Ok(());
        }
        let mut state = self
            .owner
            .state
            .lock()
            .map_err(|_| GlassError::Backend("Windows desktop transport is poisoned".into()))?;
        if !state.app_stopped && !state.stopped {
            return Err(GlassError::Backend(
                "application cleanup is unconfirmed; the desktop lease was retained".into(),
            ));
        }
        state.leased = false;
        self.closed = true;
        Ok(())
    }

    fn cleanup_complete(&self) -> bool {
        self.owner.state.lock().is_ok_and(|state| state.stopped)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn start_operation() -> Operation {
        Operation::Start(glass_core::AppSpec {
            build: Some("cargo build".into()),
            run: vec!["app.exe".into()],
            cwd: None,
            env: vec![],
            window_hint: None,
            timeout_ms: 300_000,
            sandbox: glass_core::SandboxLevel::Default,
            a11y: false,
        })
    }

    #[test]
    fn start_preserves_unbounded_build_and_long_caller_deadlines() {
        let operation = start_operation();
        assert_eq!(
            request_deadline(&operation, Deadline::UNBOUNDED),
            Deadline::UNBOUNDED
        );
        let caller = Deadline::from_millis(600_000);
        assert_eq!(request_deadline(&operation, caller), caller);
        let expired = Deadline::at(std::time::Instant::now() - Duration::from_secs(1));
        assert_eq!(request_deadline(&operation, expired), expired);
    }

    #[test]
    fn ordinary_worker_requests_keep_a_finite_transport_bound() {
        let bound = request_deadline(&Operation::Stop, Deadline::UNBOUNDED);
        assert!(
            bound
                .remaining()
                .is_some_and(|left| left <= Duration::from_secs(120))
        );
        let caller = Deadline::from_millis(1_000);
        assert!(request_deadline(&Operation::Stop, caller).instant() <= caller.instant());
    }

    #[test]
    fn finalized_owner_is_not_queried_again() {
        let finalized = std::cell::Cell::new(false);
        let result = wait_for_owner(
            Deadline::from_millis(1_000),
            std::time::Instant::now(),
            || {
                ensure!(!finalized.get(), "process executable: Access is denied");
                Ok(true)
            },
            || {
                finalized.set(true);
                Ok(true)
            },
        );
        assert!(result.expect("confirmed exit must not re-query its PID"));
    }

    #[test]
    fn identity_failure_before_finalization_is_not_ignored() {
        let result = wait_for_owner(
            Deadline::from_millis(1_000),
            std::time::Instant::now(),
            || bail!("unverified owner"),
            || panic!("an unverified owner must not be terminated"),
        );
        assert_eq!(result.unwrap_err().to_string(), "unverified owner");
    }

    #[test]
    fn finalization_failure_is_not_reported_as_owner_exit() {
        let result = wait_for_owner(
            Deadline::from_millis(1_000),
            std::time::Instant::now(),
            || Ok(true),
            || bail!("process did not exit before deadline"),
        );
        assert_eq!(
            result.unwrap_err().to_string(),
            "process did not exit before deadline"
        );
    }

    #[test]
    fn expired_stop_deadline_does_not_terminate_an_owner() {
        let result = wait_for_owner(
            Deadline::at(std::time::Instant::now() - Duration::from_secs(1)),
            std::time::Instant::now(),
            || Ok(true),
            || panic!("expired stop must not begin termination"),
        );
        assert!(result.is_err());
    }

    #[test]
    fn naturally_exited_owner_needs_no_forced_finalization() {
        let result = wait_for_owner(
            Deadline::from_millis(1_000),
            std::time::Instant::now(),
            || Ok(false),
            || panic!("exited owner must not be terminated"),
        );
        assert!(!result.expect("natural exit"));
    }

    #[test]
    #[ignore = "subprocess fixture; inert without its private environment flag"]
    fn finalizing_owner_child() {
        if std::env::var("GLASS_FINALIZING_OWNER_CHILD").as_deref() == Ok("1") {
            loop {
                std::thread::park();
            }
        }
    }

    #[test]
    #[ignore = "requires the signed-in standard Windows session"]
    fn finalizing_owner_exits_and_commits_cleanup_on_the_first_stop() {
        struct ChildGuard(Child);
        impl Drop for ChildGuard {
            fn drop(&mut self) {
                let _ = self.0.kill();
                let _ = self.0.wait();
            }
        }

        let parent = ProcessIdentity::current().expect("test parent");
        assert_ne!(parent.session, 0, "run through the interactive task bridge");
        let scratch = tempfile::tempdir().expect("test scratch");
        let root = Root::open(&scratch.path().join("state"), true)
            .expect("private test state")
            .expect("state exists");
        let executable = scratch.path().join("GlassDesktopHost.exe");
        std::fs::copy(
            std::env::current_exe().expect("test executable"),
            &executable,
        )
        .expect("copy owned fixture");
        let iterations = std::env::var("GLASS_TEARDOWN_ITERATIONS")
            .ok()
            .map(|value| value.parse::<usize>().expect("iteration count"))
            .unwrap_or(1);
        assert!((1..=20).contains(&iterations));
        for iteration in 0..iterations {
            let instance = format!("{:032x}", rand::random::<u128>());
            let stop_event = format!("Global\\Glass.Desktop.Stop.{instance}");
            let mut child = ChildGuard(
                Command::new(&executable)
                    .args([
                        "--exact",
                        "windows_environment::runtime::tests::finalizing_owner_child",
                        "--ignored",
                    ])
                    .env("GLASS_FINALIZING_OWNER_CHILD", "1")
                    .creation_flags(0x0800_0000)
                    .stdin(Stdio::null())
                    .stdout(Stdio::null())
                    .stderr(Stdio::null())
                    .spawn()
                    .expect("owned idle fixture"),
            );
            let owner = ProcessIdentity::read(child.0.id()).expect("fixture identity");
            let journal = Journal {
                version: 1,
                child: u32::MAX,
                instance: instance.clone(),
                phase: "finalizing".into(),
                error: None,
                task: format!("Glass.Desktop.{instance}"),
                worker_arguments: None,
                pipe: format!("glass-desktop-{instance}"),
                stop_event,
                show_event: format!("Local\\Glass.Desktop.Show.{instance}"),
                hide_event: format!("Local\\Glass.Desktop.Hide.{instance}"),
                parent: parent.clone(),
                owner,
                worker: None,
                child_logon: 0,
                child_pending: false,
                child_gone: true,
                child_enabled_before: false,
                changed_child_setting: false,
                clean: false,
                viewer_visible: false,
            };
            root.write("owner.json", &journal)
                .expect("publish finalizing state");
            let final_state = stop_external(&root, &journal, Deadline::from_millis(10_000))
                .expect("first stop confirms helper exit and cleanup");
            assert!(child.0.try_wait().expect("fixture exit state").is_some());
            assert!(final_state.clean && final_state.child_gone && final_state.phase == "stopped");
            assert!(root.read::<Journal>("owner.json").unwrap().unwrap().clean);
            eprintln!(
                "finalizing owner iteration {} completed on first stop",
                iteration + 1
            );
        }
    }

    fn cleanup_record() -> Journal {
        let process = ProcessIdentity::current().expect("test process");
        Journal {
            version: 1,
            child: u32::MAX,
            instance: "1234567890abcdef1234567890abcdef".into(),
            phase: "finalizing".into(),
            error: None,
            task: String::new(),
            worker_arguments: None,
            pipe: String::new(),
            stop_event: String::new(),
            show_event: String::new(),
            hide_event: String::new(),
            parent: process.clone(),
            owner: process,
            worker: None,
            child_logon: 0,
            child_pending: false,
            child_gone: true,
            child_enabled_before: true,
            changed_child_setting: false,
            clean: false,
            viewer_visible: false,
        }
    }

    #[test]
    fn disposed_stop_event_is_tolerated_only_after_confirmed_native_cleanup() {
        let mut state = cleanup_record();
        assert!(native_cleanup_finished(&state).expect("cleanup fact"));
        state.child_gone = false;
        assert!(!native_cleanup_finished(&state).expect("child still present"));
        state.child_gone = true;
        state.phase = "cleanup_failed".into();
        assert!(!native_cleanup_finished(&state).expect("failed cleanup"));
    }

    #[test]
    fn unresolved_child_identity_prevents_finalizing_cleanup() {
        let mut state = cleanup_record();
        state.child_pending = true;
        assert!(!native_cleanup_finished(&state).unwrap());
    }

    #[test]
    fn a_live_worker_prevents_finalizing_the_host() {
        let mut state = cleanup_record();
        state.worker = Some(ProcessIdentity::current().expect("live worker stand-in"));
        assert!(!native_cleanup_finished(&state).expect("worker fact"));
    }

    #[test]
    fn pending_start_is_visible_readonly_and_cannot_be_archived_while_parent_lives() {
        let scratch = tempfile::tempdir().expect("scratch");
        let base = Root::open(&scratch.path().join("state"), true)
            .unwrap()
            .unwrap();
        let parent = ProcessIdentity::current().unwrap();
        let instance = "1234567890abcdef1234567890abcdef";
        let root = base
            .child(&format!("environment-{instance}"), true)
            .unwrap()
            .unwrap();
        root.write(
            "intent.json",
            &Launch {
                version: 1,
                instance: instance.into(),
                pipe: format!("glass-desktop-{instance}"),
                worker_exe: parent.image.clone(),
                parent: parent.clone(),
                helper: None,
            },
        )
        .unwrap();
        let before = root.names().unwrap();
        let pending = pending_starts(&base, &parent).unwrap();
        assert_eq!(pending.len(), 1);
        assert_eq!(pending[0]["phase"], "pending_startup");
        assert_eq!(root.names().unwrap(), before);
        assert!(archive_finished(&base, &parent).is_err());
        assert!(root.path.exists());
    }

    #[test]
    fn pending_authorization_cannot_replace_the_recorded_parent_generation() {
        let scratch = tempfile::tempdir().expect("scratch");
        let base = Root::open(&scratch.path().join("state"), true)
            .unwrap()
            .unwrap();
        let parent = ProcessIdentity::current().unwrap();
        let instance = "1234567890abcdef1234567890abcdef";
        let root = base
            .child(&format!("environment-{instance}"), true)
            .unwrap()
            .unwrap();
        let mut launch = Launch {
            version: 1,
            instance: instance.into(),
            pipe: format!("glass-desktop-{instance}"),
            worker_exe: parent.image.clone(),
            parent: parent.clone(),
            helper: None,
        };
        root.write("intent.json", &launch).unwrap();
        launch.parent.created += 1;
        root.write("launch.json", &launch).unwrap();
        assert!(pending_starts(&base, &parent).is_err());
    }
}
