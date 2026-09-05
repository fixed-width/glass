use std::collections::BTreeSet;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU64, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, mpsc};
use std::time::{Duration, Instant};

use serde::Serialize;
use serde_json::json;

use super::TraceConfig;
use super::config::*;
use super::format::{Event, Evidence, Limits, Manifest, SCHEMA};
use super::{fs, store::Store};

const RECORDING: u8 = 0;
const LIMITED: u8 = 1;
const FAILED: u8 = 2;
const CLOSED: u8 = 3;

struct Shared {
    state: AtomicU8,
    closing: AtomicBool,
    admission: Mutex<()>,
    timed_out: AtomicBool,
    pending_bytes: AtomicUsize,
    calls: AtomicU64,
    events: AtomicU64,
    omissions: AtomicU64,
    errors: AtomicU64,
    stored_bytes: AtomicU64,
    max_bytes: u64,
    start: Instant,
    #[cfg(test)]
    fail_next_write: AtomicBool,
    #[cfg(test)]
    writer_gate: Mutex<Option<(mpsc::SyncSender<()>, mpsc::Receiver<()>)>>,
}

impl Shared {
    fn stop(&self, state: u8, reason: &str) {
        if self
            .state
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |current| {
                (current < state && current != CLOSED).then_some(state)
            })
            .is_ok()
        {
            eprintln!("glass: session trace incomplete ({reason}); tool execution is unchanged");
        }
    }

    fn reserve(&self, count: usize) -> bool {
        if self
            .pending_bytes
            .fetch_update(Ordering::AcqRel, Ordering::Acquire, |used| {
                used.checked_add(count)
                    .filter(|next| *next <= MAX_PENDING_BYTES)
            })
            .is_ok()
        {
            true
        } else {
            self.stop(LIMITED, "pending byte limit");
            false
        }
    }
}

struct Pending {
    event: Event,
    blobs: Vec<(usize, Vec<u8>)>,
    reserved: usize,
    shared: Arc<Shared>,
}

impl Drop for Pending {
    fn drop(&mut self) {
        self.shared
            .pending_bytes
            .fetch_sub(self.reserved, Ordering::AcqRel);
    }
}

pub(super) struct Capture<'a> {
    pending: &'a mut Pending,
}

impl Capture<'_> {
    pub fn entries(&self) -> usize {
        self.pending.event.evidence.len()
    }

    pub fn read_bytes(
        &mut self,
        mut evidence: Evidence,
        length: usize,
        read: impl FnOnce() -> Option<Vec<u8>>,
    ) {
        evidence.original_bytes = Some(length as u64);
        if length > MAX_PAYLOAD_BYTES {
            self.omission(evidence, "payload_limit");
        } else if !self.pending.shared.reserve(length) {
            self.omission(evidence, "pending_byte_limit");
        } else {
            self.pending.reserved += length;
            match read() {
                Some(bytes) if bytes.len() == length => {
                    self.pending
                        .blobs
                        .push((self.pending.event.evidence.len(), bytes));
                    self.pending.event.evidence.push(evidence);
                }
                _ => self.omission(evidence, "artifact_unavailable"),
            }
        }
    }

    pub fn bytes(&mut self, mut evidence: Evidence, bytes: &[u8]) {
        evidence.original_bytes = Some(bytes.len() as u64);
        let index = self.pending.event.evidence.len();
        if bytes.len() > MAX_PAYLOAD_BYTES {
            evidence.omitted = Some("payload_limit".into());
            self.pending
                .shared
                .omissions
                .fetch_add(1, Ordering::Relaxed);
        } else if self.pending.shared.reserve(bytes.len()) {
            self.pending.reserved += bytes.len();
            self.pending.blobs.push((index, bytes.to_vec()));
        } else {
            evidence.omitted = Some("pending_byte_limit".into());
            self.pending
                .shared
                .omissions
                .fetch_add(1, Ordering::Relaxed);
        }
        self.pending.event.evidence.push(evidence);
    }

    pub fn json(&mut self, mut evidence: Evidence, value: &impl Serialize) {
        let mut counter = Counter(0);
        if serde_json::to_writer(&mut counter, value).is_err() {
            evidence.omitted = Some("serialization_failed".into());
        } else {
            evidence.original_bytes = Some(counter.0 as u64);
            if counter.0 > MAX_PAYLOAD_BYTES {
                evidence.omitted = Some("payload_limit".into());
            } else if self.pending.shared.reserve(counter.0) {
                self.pending.reserved += counter.0;
                match serde_json::to_vec(value) {
                    Ok(bytes) if bytes.len() == counter.0 => {
                        self.pending
                            .blobs
                            .push((self.pending.event.evidence.len(), bytes));
                    }
                    _ => evidence.omitted = Some("serialization_failed".into()),
                }
            } else {
                evidence.omitted = Some("pending_byte_limit".into());
            }
        }
        if evidence.omitted.is_some() {
            self.pending
                .shared
                .omissions
                .fetch_add(1, Ordering::Relaxed);
        }
        self.pending.event.evidence.push(evidence);
    }

    pub fn omission(&mut self, mut evidence: Evidence, reason: &str) {
        evidence.omitted = Some(reason.into());
        self.pending.event.evidence.push(evidence);
        self.pending
            .shared
            .omissions
            .fetch_add(1, Ordering::Relaxed);
    }
}

pub(super) fn evidence(label: &str, mime: &str, trust: &str, block: Option<usize>) -> Evidence {
    Evidence {
        label: label.into(),
        mime_type: mime.into(),
        trust: trust.into(),
        block,
        source_uri: None,
        payload: None,
        omitted: None,
        original_bytes: None,
    }
}

struct Inner {
    root: PathBuf,
    path: PathBuf,
    sender: mpsc::SyncSender<Pending>,
    shared: Arc<Shared>,
    done: Mutex<Option<tokio::sync::oneshot::Receiver<()>>>,
    next_client: AtomicU64,
}

impl Drop for Inner {
    fn drop(&mut self) {
        self.shared.closing.store(true, Ordering::Release);
    }
}

#[derive(Clone)]
pub(crate) struct TraceRecorder(Arc<Inner>);

impl TraceRecorder {
    pub fn start(
        config: &TraceConfig,
        profile: crate::tool_profile::ToolProfile,
        transport: &str,
    ) -> anyhow::Result<Self> {
        TraceConfig::new(config.directory.clone(), Some(config.max_bytes))?;
        let root = fs::open_directory(&config.directory)?;
        fs::check_owner(&root)?;
        let root_path = config.directory.canonicalize()?;
        anyhow::ensure!(
            fs::same_directory(&root, &fs::open_directory(&root_path)?)?,
            "trace root changed while opening it"
        );
        let id = crate::artifacts::new_server_id();
        let name = format!("trace-{id}");
        let directory = fs::create_directory(&root, &name)?;
        let manifest = Manifest {
            schema: SCHEMA.into(),
            id,
            started_at: chrono::Utc::now().to_rfc3339(),
            server_version: crate::VERSION.into(),
            source_revision: None,
            os: std::env::consts::OS.into(),
            architecture: std::env::consts::ARCH.into(),
            transport: transport.into(),
            profile: match profile {
                crate::tool_profile::ToolProfile::Full => "full",
                crate::tool_profile::ToolProfile::Lean => "lean",
            }
            .into(),
            exclusions: [
                "transport_credentials",
                "http_headers",
                "protocol_session_tokens",
                "request_meta",
                "host_environment",
                "glass_start.env_names_and_values",
                "malformed_arguments",
                "secure_accessibility_values",
            ]
            .map(String::from)
            .into(),
            limits: Limits::new(config.max_bytes),
            state: "recording".into(),
            complete: false,
            events: 0,
            calls: 0,
            omissions: 0,
            errors: 0,
            stored_bytes: 0,
            journal_bytes: 0,
            journal_sha256: None,
            finalization: None,
        };
        let store = Store::new(directory, manifest)?;
        let shared = Arc::new(Shared {
            state: AtomicU8::new(RECORDING),
            closing: AtomicBool::new(false),
            admission: Mutex::new(()),
            timed_out: AtomicBool::new(false),
            pending_bytes: AtomicUsize::new(0),
            calls: AtomicU64::new(0),
            events: AtomicU64::new(0),
            omissions: AtomicU64::new(0),
            errors: AtomicU64::new(0),
            stored_bytes: AtomicU64::new(store.manifest.stored_bytes),
            max_bytes: config.max_bytes,
            start: Instant::now(),
            #[cfg(test)]
            fail_next_write: AtomicBool::new(false),
            #[cfg(test)]
            writer_gate: Mutex::new(None),
        });
        let (sender, receiver) = mpsc::sync_channel(MAX_PENDING_EVENTS);
        let (done_tx, done_rx) = tokio::sync::oneshot::channel();
        let worker_shared = shared.clone();
        std::thread::Builder::new()
            .name("glass-trace".into())
            .spawn(move || {
                writer(store, receiver, &worker_shared);
                let _ = done_tx.send(());
            })?;
        let recorder = Self(Arc::new(Inner {
            root: root_path.clone(),
            path: root_path.join(name),
            sender,
            shared,
            done: Mutex::new(Some(done_rx)),
            next_client: AtomicU64::new(1),
        }));
        recorder.record("inventory", None, 0, json!({}), |capture| {
            capture.json(evidence("tools_and_instructions", "application/json", "glass", None), &json!({
                "tools": crate::server::tool_inventory(profile), "instructions": profile.instructions(),
            }));
        });
        eprintln!(
            "glass: session trace at {:?}; retains supplied inputs and requested app evidence, which may contain sensitive data",
            recorder.path()
        );
        Ok(recorder)
    }

    pub fn root(&self) -> &Path {
        &self.0.root
    }
    pub fn path(&self) -> &Path {
        &self.0.path
    }
    pub fn new_client(&self) -> u64 {
        let client = self.0.next_client.fetch_add(1, Ordering::Relaxed);
        self.record("client_created", None, client, json!({}), |_| {});
        client
    }

    pub fn shutdown_event(&self, status: &str) {
        self.record(
            "shutdown",
            None,
            0,
            json!({"status": status, "host_hook_confirmation": "not_reported"}),
            |_| {},
        );
        if status != "completed" {
            self.0.shared.errors.fetch_add(1, Ordering::Relaxed);
        }
    }

    pub fn status(&self) -> serde_json::Value {
        let shared = &self.0.shared;
        json!({
            "enabled": true, "state": state_name(shared.state.load(Ordering::Acquire)),
            "stored_bytes": shared.stored_bytes.load(Ordering::Relaxed), "calls": shared.calls.load(Ordering::Relaxed).min(MAX_CALLS),
            "limits": Limits::new(shared.max_bytes), "omissions": shared.omissions.load(Ordering::Relaxed), "errors": shared.errors.load(Ordering::Relaxed),
        })
    }

    pub fn begin_call(&self, tool: &str, client: u64) -> Option<CallTrace> {
        if !self.accepting() {
            return None;
        }
        let call = self.0.shared.calls.fetch_add(1, Ordering::Relaxed) + 1;
        if call > MAX_CALLS {
            self.0.shared.omissions.fetch_add(1, Ordering::Relaxed);
            self.0.shared.stop(LIMITED, "call limit");
            return None;
        }
        self.record(
            "call_received",
            Some(call),
            client,
            json!({"tool": tool.chars().take(128).collect::<String>()}),
            |_| {},
        );
        Some(CallTrace {
            recorder: self.clone(),
            id: call,
            client,
            valid_arguments: Arc::new(AtomicBool::new(false)),
        })
    }

    fn accepting(&self) -> bool {
        !self.0.shared.closing.load(Ordering::Acquire)
            && self.0.shared.state.load(Ordering::Acquire) == RECORDING
    }

    pub(super) fn record(
        &self,
        kind: &str,
        call: Option<u64>,
        client: u64,
        data: serde_json::Value,
        capture: impl FnOnce(&mut Capture<'_>),
    ) {
        let admission = self
            .0
            .shared
            .admission
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        if !self.accepting() {
            return;
        }
        if self.0.shared.events.fetch_add(1, Ordering::Relaxed) >= MAX_EVENTS - 1 {
            self.0.shared.omissions.fetch_add(1, Ordering::Relaxed);
            self.0.shared.stop(LIMITED, "event limit");
            return;
        }
        let mut pending = Pending {
            event: Event {
                seq: 0,
                elapsed_us: elapsed(&self.0.shared),
                kind: kind.into(),
                call,
                client,
                data,
                evidence: vec![],
            },
            blobs: vec![],
            reserved: MAX_EVENT_BYTES,
            shared: self.0.shared.clone(),
        };
        if !self.0.shared.reserve(MAX_EVENT_BYTES) {
            pending.reserved = 0;
            self.0.shared.omissions.fetch_add(1, Ordering::Relaxed);
            return;
        }
        drop(admission);
        capture(&mut Capture {
            pending: &mut pending,
        });
        if self.0.sender.try_send(pending).is_err() {
            self.0.shared.omissions.fetch_add(1, Ordering::Relaxed);
            self.0.shared.stop(LIMITED, "writer queue unavailable");
        }
    }

    pub async fn close(&self) {
        {
            let _admission = self
                .0
                .shared
                .admission
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            self.0.shared.closing.store(true, Ordering::Release);
        }
        let done = self.0.done.lock().unwrap_or_else(|e| e.into_inner()).take();
        if let Some(done) = done
            && !matches!(
                tokio::time::timeout(Duration::from_secs(2), done).await,
                Ok(Ok(()))
            )
        {
            self.0.shared.timed_out.store(true, Ordering::Release);
            self.0.shared.errors.fetch_add(1, Ordering::Relaxed);
            self.0.shared.stop(FAILED, "writer shutdown timeout");
        }
    }

    #[cfg(test)]
    pub(super) async fn idle(&self) {
        tokio::time::timeout(Duration::from_secs(5), async {
            while self.0.shared.pending_bytes.load(Ordering::Acquire) != 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("trace writer drained");
    }

    #[cfg(test)]
    pub(super) fn fail_next_write(&self) {
        self.0.shared.fail_next_write.store(true, Ordering::Release);
    }

    #[cfg(test)]
    pub(super) fn pause_next_write(&self) -> (mpsc::Receiver<()>, mpsc::SyncSender<()>) {
        let (entered_tx, entered_rx) = mpsc::sync_channel(1);
        let (release_tx, release_rx) = mpsc::sync_channel(1);
        *self.0.shared.writer_gate.lock().unwrap() = Some((entered_tx, release_rx));
        (entered_rx, release_tx)
    }

    #[cfg(test)]
    pub(super) fn set_calls_at_limit(&self) {
        self.0.shared.calls.store(MAX_CALLS, Ordering::Relaxed);
    }
}

fn elapsed(shared: &Shared) -> u64 {
    u64::try_from(shared.start.elapsed().as_micros()).unwrap_or(u64::MAX)
}

fn state_name(state: u8) -> &'static str {
    match state {
        RECORDING => "recording",
        LIMITED => "limited",
        FAILED => "failed",
        _ => "closed",
    }
}

fn writer(mut store: Store, receiver: mpsc::Receiver<Pending>, shared: &Shared) {
    let mut unfinished = BTreeSet::new();
    let mut journal_has_capacity = true;
    loop {
        let pending = match receiver.recv_timeout(Duration::from_millis(20)) {
            Ok(pending) => pending,
            Err(mpsc::RecvTimeoutError::Timeout)
                if !shared.closing.load(Ordering::Acquire)
                    || shared.pending_bytes.load(Ordering::Acquire) != 0 =>
            {
                continue;
            }
            Err(_) => break,
        };
        if shared.state.load(Ordering::Acquire) == FAILED || !journal_has_capacity {
            continue;
        }
        match write_pending(&mut store, &pending, shared, &mut unfinished) {
            Ok(has_capacity) => journal_has_capacity = has_capacity,
            Err(_) => {
                shared.errors.fetch_add(1, Ordering::Relaxed);
                shared.stop(FAILED, "evidence write failed");
            }
        }
        shared
            .stored_bytes
            .store(store.manifest.stored_bytes, Ordering::Relaxed);
    }
    if shared.state.load(Ordering::Acquire) == FAILED || shared.timed_out.load(Ordering::Acquire) {
        store.manifest.state = "failed".into();
        store.manifest.errors = shared.errors.load(Ordering::Relaxed);
        store.manifest.omissions = shared.omissions.load(Ordering::Relaxed);
        let _ = store.write_manifest();
        return;
    }
    let terminal = Event {
        seq: 0,
        elapsed_us: elapsed(shared),
        kind: "trace_closed".into(),
        call: None,
        client: 0,
        data: json!({"unfinished_calls": unfinished, "writer_timed_out": shared.timed_out.load(Ordering::Acquire)}),
        evidence: vec![],
    };
    if store.event(terminal, true).is_err() {
        shared.errors.fetch_add(1, Ordering::Relaxed);
        shared.stop(FAILED, "terminal record write failed");
        return;
    }
    let state = shared.state.load(Ordering::Acquire);
    store.manifest.state = if state == RECORDING {
        "closed".into()
    } else {
        state_name(state).into()
    };
    store.manifest.omissions = shared.omissions.load(Ordering::Relaxed);
    store.manifest.errors = shared.errors.load(Ordering::Relaxed);
    store.manifest.complete = state == RECORDING
        && store.manifest.omissions == 0
        && store.manifest.errors == 0
        && unfinished.is_empty()
        && !shared.timed_out.load(Ordering::Acquire);
    store.manifest.finalization = Some("writer_closed".into());
    if store.finish().is_err() {
        shared.errors.fetch_add(1, Ordering::Relaxed);
        shared.stop(FAILED, "trace finalization failed");
    } else if state == RECORDING {
        let _ =
            shared
                .state
                .compare_exchange(RECORDING, CLOSED, Ordering::AcqRel, Ordering::Acquire);
    }
    shared
        .stored_bytes
        .store(store.manifest.stored_bytes, Ordering::Relaxed);
}

fn write_pending(
    store: &mut Store,
    pending: &Pending,
    shared: &Shared,
    unfinished: &mut BTreeSet<u64>,
) -> anyhow::Result<bool> {
    #[cfg(test)]
    if let Some((entered, release)) = shared.writer_gate.lock().unwrap().take() {
        let _ = entered.send(());
        let _ = release.recv();
    }
    #[cfg(test)]
    if shared.fail_next_write.swap(false, Ordering::AcqRel) {
        anyhow::bail!("injected trace write failure");
    }
    let mut event = pending.event.clone();
    for (index, bytes) in &pending.blobs {
        match store.payload(bytes)? {
            Some(payload) => event.evidence[*index].payload = Some(payload),
            None => {
                event.evidence[*index].omitted = Some("total_byte_limit".into());
                shared.omissions.fetch_add(1, Ordering::Relaxed);
                shared.stop(LIMITED, "total byte limit");
            }
        }
    }
    if store.event(event.clone(), false)? {
        if let Some(call) = event.call {
            match event.kind.as_str() {
                "call_received" => {
                    unfinished.insert(call);
                    store.manifest.calls += 1;
                }
                "logical_outcome" | "router_rejection" | "worker_unavailable" => {
                    unfinished.remove(&call);
                }
                _ => {}
            }
        }
    } else {
        shared.omissions.fetch_add(1, Ordering::Relaxed);
        shared.stop(LIMITED, "total byte limit");
        return Ok(false);
    }
    Ok(true)
}

#[derive(Clone)]
pub(crate) struct CallTrace {
    recorder: TraceRecorder,
    pub id: u64,
    pub client: u64,
    valid_arguments: Arc<AtomicBool>,
}

impl CallTrace {
    pub fn record(&self, kind: &str, data: serde_json::Value) {
        self.recorder
            .record(kind, Some(self.id), self.client, data, |_| {});
    }

    pub fn arguments(&self, value: &impl Serialize) {
        self.valid_arguments.store(true, Ordering::Release);
        self.capture("arguments", json!({}), |capture| {
            capture.json(
                evidence("arguments", "application/json", "caller", None),
                value,
            )
        });
    }

    pub fn has_valid_arguments(&self) -> bool {
        self.valid_arguments.load(Ordering::Acquire)
    }

    pub fn arguments_unavailable(&self) {
        self.valid_arguments.store(true, Ordering::Release);
        self.capture("arguments", json!({}), |capture| {
            capture.omission(
                evidence("arguments", "application/json", "caller", None),
                "argument_capture_unavailable",
            );
        });
    }

    pub(super) fn capture(
        &self,
        kind: &str,
        data: serde_json::Value,
        capture: impl FnOnce(&mut Capture<'_>),
    ) {
        self.recorder
            .record(kind, Some(self.id), self.client, data, capture);
    }
}

struct Counter(usize);

pub(crate) fn argument_bytes(value: &impl Serialize) -> Option<u64> {
    let mut counter = Counter(0);
    serde_json::to_writer(&mut counter, value).ok()?;
    u64::try_from(counter.0).ok()
}
impl Write for Counter {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0 = self
            .0
            .checked_add(bytes.len())
            .ok_or_else(|| std::io::Error::other("serialized size overflow"))?;
        Ok(bytes.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}
