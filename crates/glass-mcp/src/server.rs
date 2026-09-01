//! rmcp handler exposing the tool-logic layer over MCP. Thin wrappers only.

use std::sync::Arc;

use base64::Engine;
use glass_core::{Glass, HostPathAccess, ProtectedHostPath};
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{
    CallToolResult, ContentBlock, ErrorCode, ListResourceTemplatesResult, ListResourcesResult,
    Meta, ReadResourceRequestParams, ReadResourceResult, ResourceContents, ServerCapabilities,
    ServerInfo,
};
use rmcp::{ErrorData as McpError, ServerHandler, tool, tool_handler, tool_router};
use tokio::sync::Mutex;

use crate::artifacts::{ArtifactReadError, ArtifactStore, classify_uri};
use crate::audit::AuditReport;
use crate::output::{OutContent, TargetAccess, ToolEffect, ToolOutput};
use crate::output_policy::{AppliedOutcome, OutputPolicy, ToolCallOutcome};
use crate::params::*;
use crate::tools::{self, BatchToolResult, ToolResult};

/// A synchronous tool body plus where to send its result — run on the dedicated
/// `glass-platform` thread (see [`GlassServer::new`]).
type Job = (
    &'static str,
    ToolEffect,
    Box<dyn FnOnce(&mut Glass) -> ToolCallOutcome + Send>,
    tokio::sync::oneshot::Sender<ToolCallOutcome>,
);

#[derive(Clone)]
pub struct GlassServer {
    glass: Arc<Mutex<Glass>>,
    /// Hands tool bodies to the long-lived `glass-platform` thread.
    jobs: Option<std::sync::mpsc::Sender<Job>>,
    /// Audit-log posture, carried for `glass_doctor` display.
    report: AuditReport,
    artifacts: Option<ArtifactStore>,
    artifact_server_id: String,
    output_policy: Arc<OutputPolicy>,
    tool_router: ToolRouter<GlassServer>,
}

fn tool_effect(tool: &str) -> ToolEffect {
    match tool {
        "glass_a11y_marks"
        | "glass_a11y_snapshot"
        | "glass_capabilities"
        | "glass_clipboard_get"
        | "glass_diff"
        | "glass_find_elements"
        | "glass_list_windows"
        | "glass_logs"
        | "glass_screenshot"
        | "glass_wait_for_element"
        | "glass_wait_for_log"
        | "glass_wait_for_region"
        | "glass_wait_stable" => ToolEffect::ReadOnly,
        _ => ToolEffect::MayMutate,
    }
}

fn target_access(access: HostPathAccess) -> TargetAccess {
    match access {
        HostPathAccess::DeniedBySandbox => TargetAccess::DeniedBySandbox,
        HostPathAccess::NotGuaranteedSandboxOff => TargetAccess::NotGuaranteedSandboxOff,
        HostPathAccess::HostFilesystemUnreachable => TargetAccess::HostFilesystemUnreachable,
        HostPathAccess::NoActiveTarget => TargetAccess::NoActiveTarget,
    }
}

fn applied_to_call_result(applied: AppliedOutcome, target_access: TargetAccess) -> CallToolResult {
    let (output, is_error, _metadata, response_pin) = applied.into_parts();
    let content = output
        .0
        .into_iter()
        .map(|item| match item {
            OutContent::Envelope(envelope) => ContentBlock::text(envelope.render()),
            OutContent::Text(text) => ContentBlock::text(text.body),
            OutContent::Image(bytes) => ContentBlock::image(
                base64::engine::general_purpose::STANDARD.encode(bytes),
                "image/webp",
            ),
            OutContent::ResourceLink(descriptor) => {
                ContentBlock::ResourceLink(descriptor.to_resource(target_access))
            }
        })
        .collect();
    let result = if is_error {
        CallToolResult::error(content)
    } else {
        CallToolResult::success(content)
    };
    #[cfg(test)]
    fire_construction_observer();
    // The pin must cover descriptor rendering and allocation of every rmcp content block.
    drop(response_pin);
    result
}

#[cfg(test)]
thread_local! {
    static CONSTRUCTION_OBSERVER: std::cell::RefCell<Option<Box<dyn FnOnce()>>> =
        const { std::cell::RefCell::new(None) };
}

#[cfg(test)]
fn set_construction_observer(observer: impl FnOnce() + 'static) {
    CONSTRUCTION_OBSERVER.with(|slot| *slot.borrow_mut() = Some(Box::new(observer)));
}

#[cfg(test)]
fn fire_construction_observer() {
    CONSTRUCTION_OBSERVER.with(|slot| {
        if let Some(observer) = slot.borrow_mut().take() {
            observer();
        }
    });
}

#[cfg(test)]
fn map_call_outcome(outcome: ToolCallOutcome) -> CallToolResult {
    let access = outcome.target_access;
    applied_to_call_result(OutputPolicy::unavailable().apply(outcome), access)
}

#[cfg(test)]
fn map_tool_result(result: ToolResult) -> CallToolResult {
    let (is_error, output) = match result {
        Ok(output) => (false, output),
        Err(message) => (true, ToolOutput(vec![OutContent::trusted_error(message)])),
    };
    map_call_outcome(ToolCallOutcome {
        tool: "glass_test",
        effect: ToolEffect::ReadOnly,
        is_error,
        target_access: TargetAccess::NoActiveTarget,
        output,
    })
}

/// The `glass_doctor` result payload: the rendered report humans and existing
/// consumers read, plus the same data structured — `sections` and the `overall`
/// verdict — so an agent can branch on status without parsing prose. Kept pure
/// (no async / no probing) so the shape is unit-testable against a hand-built
/// `Diagnosis`.
fn doctor_result(diag: &glass_core::Diagnosis, backend: &str) -> serde_json::Value {
    serde_json::json!({
        "report": diag.render_text(backend),
        "sections": diag.sections,
        "overall": diag.overall(backend),
    })
}

#[cfg(test)]
thread_local! {
    static FAIL_WORKER_SPAWN: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

fn worker_spawn_enabled() -> bool {
    #[cfg(test)]
    if FAIL_WORKER_SPAWN.with(std::cell::Cell::get) {
        return false;
    }
    true
}

#[tool_router]
impl GlassServer {
    pub fn new(mut glass: Glass, report: AuditReport) -> Self {
        const ARTIFACT_LIMIT_BYTES: u64 = 64 * 1024 * 1024;
        let store = match ArtifactStore::new(ARTIFACT_LIMIT_BYTES) {
            Ok(store) => {
                let registration = OutputPolicy::validate_store_paths(&store).and_then(|()| {
                    glass
                        .set_protected_host_paths(vec![
                            ProtectedHostPath::directory(store.process_dir()),
                            ProtectedHostPath::file(store.lease_path()),
                        ])
                        .map_err(|_| crate::artifacts::ArtifactError::ProtectionRegistrationFailed)
                });
                if registration.is_ok() {
                    Some(store)
                } else {
                    if store.shutdown().is_err() {
                        eprintln!("glass: artifact storage cleanup failed during startup");
                    }
                    None
                }
            }
            Err(_) => None,
        };
        if store.is_none() {
            eprintln!(
                "glass: artifact storage unavailable; oversized responses will be bounded and incomplete"
            );
        }
        Self::new_with_state(glass, report, store)
    }

    #[cfg(test)]
    pub(crate) fn new_with_store(
        mut glass: Glass,
        report: AuditReport,
        store: ArtifactStore,
    ) -> Result<Self, crate::artifacts::ArtifactError> {
        OutputPolicy::validate_store_paths(&store)?;
        glass
            .set_protected_host_paths(vec![
                ProtectedHostPath::directory(store.process_dir()),
                ProtectedHostPath::file(store.lease_path()),
            ])
            .map_err(|_| crate::artifacts::ArtifactError::ProtectionRegistrationFailed)?;
        Ok(Self::new_with_state(glass, report, Some(store)))
    }

    fn new_with_state(glass: Glass, report: AuditReport, artifacts: Option<ArtifactStore>) -> Self {
        let artifact_server_id = artifacts
            .as_ref()
            .map_or_else(crate::artifacts::new_server_id, ArtifactStore::server_id);
        let output_policy = Arc::new(match artifacts.clone() {
            Some(store) => OutputPolicy::new(store),
            None => OutputPolicy::unavailable(),
        });
        let glass = Arc::new(Mutex::new(glass));
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        let worker_glass = glass.clone();
        // This long-lived thread parents contained targets and serializes the one active session.
        let worker = if worker_spawn_enabled() {
            std::thread::Builder::new()
                .name("glass-platform".into())
                .spawn(move || {
                    while let Ok((tool, effect, job, reply)) = rx.recv() {
                        let mut g = worker_glass.blocking_lock();
                        let mut outcome =
                            std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut g)))
                                .unwrap_or_else(|_| ToolCallOutcome {
                                    tool,
                                    effect,
                                    is_error: true,
                                    target_access: target_access(g.host_path_access()),
                                    output: ToolOutput(vec![OutContent::trusted_error(
                                        "tool handler panicked",
                                    )]),
                                });
                        outcome.target_access = target_access(g.host_path_access());
                        let _ = reply.send(outcome);
                    }
                })
                .ok()
        } else {
            None
        };
        Self {
            glass,
            jobs: worker.map(|_| jobs),
            report,
            artifacts,
            artifact_server_id,
            output_policy,
            tool_router: Self::tool_router(),
        }
    }

    pub fn sessions(&self) -> Arc<Mutex<Glass>> {
        self.glass.clone()
    }

    #[cfg(test)]
    fn new_unavailable_for_test(server_id: &str) -> Self {
        let mut server = Self::new_with_state(
            crate::boot(None),
            crate::audit::report_from_config(None, |_| None),
            None,
        );
        server.artifact_server_id = server_id.to_owned();
        server
    }

    #[cfg(test)]
    fn read_resource_for_test(&self, uri: &str) -> Result<ReadResourceResult, McpError> {
        read_resource_result(self.artifacts.as_ref(), &self.artifact_server_id, uri)
    }

    pub(crate) fn artifact_store(&self) -> Option<ArtifactStore> {
        self.artifacts.clone()
    }

    async fn run<F>(
        &self,
        tool: &'static str,
        effect: ToolEffect,
        f: F,
    ) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&mut Glass) -> ToolResult + Send + 'static,
    {
        self.run_outcome(tool, effect, move |g| match f(g) {
            Ok(output) => (false, output),
            Err(message) => (true, ToolOutput(vec![OutContent::trusted_error(message)])),
        })
        .await
    }

    async fn run_batch<F>(
        &self,
        tool: &'static str,
        effect: ToolEffect,
        f: F,
    ) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&mut Glass) -> BatchToolResult + Send + 'static,
    {
        self.run_outcome(tool, effect, move |g| match f(g) {
            Ok(output) => (false, output),
            Err(output) => (true, output),
        })
        .await
    }

    async fn run_outcome<F>(
        &self,
        tool: &'static str,
        effect: ToolEffect,
        f: F,
    ) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&mut Glass) -> (bool, ToolOutput) + Send + 'static,
    {
        let job = move |g: &mut Glass| {
            let (is_error, output) = f(g);
            ToolCallOutcome {
                tool,
                effect,
                is_error,
                target_access: target_access(g.host_path_access()),
                output,
            }
        };
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        let fallback = || ToolCallOutcome {
            tool,
            effect,
            is_error: true,
            target_access: TargetAccess::NoActiveTarget,
            output: ToolOutput(vec![OutContent::trusted_error(
                "glass platform worker unavailable",
            )]),
        };
        let Some(jobs) = &self.jobs else {
            return Ok(applied_to_call_result(
                self.output_policy.apply(fallback()),
                TargetAccess::NoActiveTarget,
            ));
        };
        if jobs.send((tool, effect, Box::new(job), reply_tx)).is_err() {
            return Ok(applied_to_call_result(
                self.output_policy.apply(fallback()),
                TargetAccess::NoActiveTarget,
            ));
        }
        let outcome = reply_rx.await.unwrap_or_else(|_| fallback());
        let access = outcome.target_access;
        let policy = self.output_policy.clone();
        let applied = tokio::task::spawn_blocking(move || policy.apply(outcome))
            .await
            .map_err(|_| {
                McpError::new(ErrorCode::INTERNAL_ERROR, "output processing failed", None)
            })?;
        Ok(applied_to_call_result(applied, access))
    }

    #[cfg(test)]
    pub(crate) async fn run_test_outcome(
        &self,
        outcome: ToolCallOutcome,
    ) -> Result<CallToolResult, McpError> {
        let tool = outcome.tool;
        let effect = outcome.effect;
        let is_error = outcome.is_error;
        let output = outcome.output;
        self.run_outcome(tool, effect, move |_| (is_error, output))
            .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = true
        ),
        description = "Build, launch, and locate a native GUI app; returns its window geometry. Choose a backend with the `backend` param (defaults to the host). The accessibility tools are enabled by default; pass `a11y:false` to skip the accessibility bus for canvas/pixel-only apps. Optional `window_hint` ({ title?, class? }) picks the right window when several appear, or locates one the launched process hands off to another process."
    )]
    async fn glass_start(
        &self,
        Parameters(a): Parameters<StartArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_start", tool_effect("glass_start"), move |g| {
            tools::start(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = true,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Stop the running app and end the session. The app is asked to close first, \
                       so it saves state and starts clean next time; one that will not close is \
                       terminated, which takes a moment longer. Ends everything session-scoped: \
                       captured logs and a11y element ids are gone afterwards, so read what you \
                       need first (saved baselines outlive it, until the server exits). There is \
                       no resume — only glass_start runs the app again, as a fresh session. Not \
                       needed between steps of a task; one session can be driven for as long as \
                       you need it, and errors if no session is running."
    )]
    async fn glass_stop(&self) -> Result<CallToolResult, McpError> {
        self.run("glass_stop", tool_effect("glass_stop"), tools::stop)
            .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Focus/resize/move the window or read its geometry. op: focus|resize|move|geometry."
    )]
    async fn glass_window(
        &self,
        Parameters(a): Parameters<WindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_window", tool_effect("glass_window"), move |g| {
            tools::window(g, &a)
        })
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Capture current visual evidence from the app window (or an optional window-relative `region`) as a lossless WebP screenshot. This proves only what pixels are visible at capture time, not semantic state or transition completion; use glass_wait_for_element for an accessible condition/value, glass_wait_for_region for pixel transition completion, or glass_wait_stable for visual quiescence. A capture reaching off the display edge is clipped to the on-screen portion — the returned `width`/`height` are the actual captured size, so a frame smaller than the window/region means it was clipped; only a fully off-screen surface errors."
    )]
    async fn glass_screenshot(
        &self,
        Parameters(a): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_screenshot",
            tool_effect("glass_screenshot"),
            move |g| tools::screenshot(g, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Wait for visual quiescence: consecutive frames stop changing, then return the last frame. This proves stability, not that an expected semantic state or pixel design was reached; use glass_wait_for_element for a semantic condition/value or glass_wait_for_region with a baseline for expected pixels. Optional `stability_region` watches only that sub-rectangle; optional `region` crops the returned frame. Set `include_image:false` for text-only metadata. If at least two next actions or waits are known, use glass_do instead of separate calls."
    )]
    async fn glass_wait_stable(
        &self,
        Parameters(a): Parameters<WaitStableArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_wait_stable",
            tool_effect("glass_wait_stable"),
            move |g| tools::wait_stable(g, &a),
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Click at window-relative coordinates. button: left|right|middle; count for multi-click. Optional modifiers held during the action, e.g. [\"ctrl\"] or [\"ctrl\",\"shift\"] for multi/range-select. If at least two next actions or waits are known, use glass_do instead of separate calls."
    )]
    async fn glass_click(
        &self,
        Parameters(a): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_click", tool_effect("glass_click"), move |g| {
            tools::click(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Move the pointer to window-relative coordinates. If at least two next actions or waits are known, use glass_do instead of separate calls."
    )]
    async fn glass_move(
        &self,
        Parameters(a): Parameters<MoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_move", tool_effect("glass_move"), move |g| {
            tools::mouse_move(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Drag with a button held from (x1,y1) to (x2,y2) — window-relative \
                       coordinates, so 0,0 is the window's top-left, not the screen's. Presses \
                       at the start point, moves across in steps over `duration_ms`, and releases \
                       at the end; the button is left (`button` overrides) and optional modifiers \
                       are held throughout, e.g. [\"ctrl\"] or [\"ctrl\",\"shift\"] for \
                       multi/range-select. Either endpoint outside the window is refused with an \
                       error giving the window size, so a drag never lands somewhere you did not \
                       aim. Use this for a single pointer — selecting text, moving an item, \
                       resizing a pane; glass_gesture is the multi-touch equivalent (2+ pointers, \
                       for pinch/rotate), and glass_click is the press-and-release-in-place case. \
                       If at least two next actions or waits are known, use glass_do instead of \
                       separate calls."
    )]
    async fn glass_drag(
        &self,
        Parameters(a): Parameters<DragArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_drag", tool_effect("glass_drag"), move |g| {
            tools::drag(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Scroll at window-relative coordinates by (dx,dy) wheel steps. Optional modifiers held during the action, e.g. [\"ctrl\"] or [\"ctrl\",\"shift\"] for multi/range-select. If at least two next actions or waits are known, use glass_do instead of separate calls."
    )]
    async fn glass_scroll(
        &self,
        Parameters(a): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_scroll", tool_effect("glass_scroll"), move |g| {
            tools::scroll(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Perform a multi-touch gesture: 2–10 pointers, each a straight from→to \
                       segment in window-relative px, all down together at t=0 and up at \
                       duration_ms. Pinch = two pointers toward/apart; rotate = two on an arc; \
                       two-finger swipe = two parallel segments; a from==to pointer is held. \
                       Multi-touch isn't available on every backend — it returns a clear \
                       Unsupported error where the active backend can't do it."
    )]
    async fn glass_gesture(
        &self,
        Parameters(a): Parameters<GestureArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_gesture", tool_effect("glass_gesture"), move |g| {
            tools::gesture(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Type a string of text into the focused window. Does not focus anything \
                       itself — click the field first (glass_click_element, or glass_click), or \
                       the text goes wherever focus already was. Sent as individual keystrokes, \
                       not a paste, so per-key handlers, autocomplete and validation all run; a \
                       newline in `text` does not press Return, so send that as a separate \
                       glass_key. Prefer glass_set_value for a field the a11y tree exposes: it \
                       addresses the field directly and reports whether the value landed, where \
                       this types wherever the cursor already sits and cannot tell you what it \
                       hit. \
                       Optional `return`: \"snapshot\" settles the UI then folds a fresh a11y \
                       tree into the result (and refreshes the snapshot cache); \"settle\" waits \
                       for the UI to stop changing (text-only); omit or \"none\" for no observe \
                       (default). If at least two next actions or waits are known, use glass_do \
                       instead of separate calls."
    )]
    async fn glass_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_type", tool_effect("glass_type"), move |g| {
            tools::type_text(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Press a key chord like 'ctrl+s', 'Return', 'alt+F4'. One key with any \
                       number of modifiers, joined by '+': the last token is the key, every \
                       earlier one a modifier (ctrl, shift, alt, super — `cmd`, `win` and `meta` \
                       are accepted names for super, and all of them are case-insensitive). The \
                       key is a named key such as Return, Escape, Tab, Delete, an arrow or F1-F12, \
                       or a single printable ASCII character. An unrecognised modifier or \
                       key name is rejected with an error naming the token, so nothing is \
                       half-pressed; modifiers are released again when the chord completes. Use \
                       this for shortcuts and named keys — glass_type is for literal text and \
                       cannot express either. If at least two next actions or waits are known, use \
                       glass_do instead of separate calls."
    )]
    async fn glass_key(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_key", tool_effect("glass_key"), move |g| {
            tools::key(g, &a)
        })
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Read the app's clipboard as text (\"\" if empty). Also the cheap \
                       text-extraction path: glass_do ctrl+a then ctrl+c, then read here \
                       (beats OCR for selectable text). Returns Unsupported where the backend \
                       can't provide clipboard access."
    )]
    async fn glass_clipboard_get(&self) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_clipboard_get",
            tool_effect("glass_clipboard_get"),
            tools::clipboard_get,
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Write text to the app's clipboard so it can paste it. Returns \
                       Unsupported where the backend can't provide clipboard access."
    )]
    async fn glass_clipboard_set(
        &self,
        Parameters(a): Parameters<ClipboardSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_clipboard_set",
            tool_effect("glass_clipboard_set"),
            move |g| tools::clipboard_set(g, &a),
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Save the current frame as a named visual baseline — a reference image \
                       glass_diff and glass_wait_for_region later compare against, so you can ask \
                       what changed without spending image tokens on a before-and-after pair. \
                       Captures the whole window at call time (not a saved region), so settle the \
                       UI first if something is still animating. Saving over an existing name \
                       replaces it silently; baselines live outside the app under a per-server \
                       directory and last until the server exits, surviving glass_stop. Use this \
                       plus glass_diff to detect change; use glass_screenshot when you actually \
                       need to look at the pixels."
    )]
    async fn glass_baseline_save(
        &self,
        Parameters(a): Parameters<BaselineSaveArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_baseline_save",
            tool_effect("glass_baseline_save"),
            move |g| tools::baseline_save(g, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Compare current visual evidence with a named pixel baseline; returns change stats and a bounding box. This is a single current-state comparison, not a wait for transition completion or stability; use glass_wait_for_region to wait for pixel change/match and glass_wait_stable for quiescence. Set `include_image:true` to return the changed crop when pixels differ."
    )]
    async fn glass_diff(
        &self,
        Parameters(a): Parameters<DiffArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_diff", tool_effect("glass_diff"), move |g| {
            tools::diff(g, &a)
        })
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Diagnose the glass environment and report per-check status + how to \
                       fix anything missing. Use this to self-diagnose a glass_start failure. \
                       Optional `deep`: also spin up and tear down the default backend's \
                       headless display to verify it starts. Returns `report` (the rendered \
                       text above) plus structured data: `sections` (each a `{title, backend, \
                       checks: [{name, status, detail, remedy?, remedy_action?}]}`, where \
                       `backend` is null for general checks that apply to every backend, and \
                       `status` is one of `\"ok\"`/`\"warn\"`/`\"fail\"`/`\"skip\"`; `remedy` and \
                       `remedy_action` are each omitted when absent, so a failing check may \
                       carry neither) and `overall` — the single \
                       verdict to branch on, since it already downgrades a non-default backend's \
                       failing check to a warning the way the rendered summary does."
    )]
    async fn glass_doctor(
        &self,
        Parameters(a): Parameters<DoctorArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = crate::default_backend(std::env::var("GLASS_BACKEND").ok().as_deref());
        let deep = a.deep.unwrap_or(false);
        let report = self.report.clone();
        self.run("glass_doctor", tool_effect("glass_doctor"), move |_| {
            let diag = crate::doctor::diagnose_with_audit(deep, &report);
            Ok(ToolOutput::result(
                "glass_doctor",
                doctor_result(&diag, backend),
            ))
        })
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Report which operations (input, multi-touch, clipboard, accessibility, \
                       window move/resize) can be performed right now on a backend, and any \
                       setup a blocked one needs — so you can check before acting instead of \
                       hitting an Unsupported error. Each operation reports a live `status` \
                       (`supported`, `degraded` — works now at reduced fidelity, `note` says \
                       what's lost; `requires_setup` — a setup step is missing, `note` says \
                       what; or `unsupported` — this backend never does it) plus the `tools` \
                       it gates, so a degraded or blocked operation names exactly which tool \
                       calls to expect trouble from. Pass `backend` to query a specific backend \
                       by name; omit for the active one. Static — no session required."
    )]
    async fn glass_capabilities(
        &self,
        Parameters(a): Parameters<CapabilitiesArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_capabilities",
            tool_effect("glass_capabilities"),
            move |_| {
                crate::capabilities::render_value(a.backend.as_deref())
                    .map(|value| ToolOutput::result("glass_capabilities", value))
            },
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "List the app's top-level windows: id, title, class, geometry, and which is active. Returns a JSON array. Window ids are not stable across calls — re-list after windows open/close instead of caching ids."
    )]
    async fn glass_list_windows(&self) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_list_windows",
            tool_effect("glass_list_windows"),
            tools::list_windows,
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            idempotent_hint = true,
            open_world_hint = false
        ),
        description = "Make a window active by id (from glass_list_windows). Subsequent screenshot/click/type/window ops target it; coordinates are relative to it."
    )]
    async fn glass_select_window(
        &self,
        Parameters(a): Parameters<SelectWindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_select_window",
            tool_effect("glass_select_window"),
            move |g| tools::select_window(g, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Find a bounded ranked set of accessibility elements from one fresh read. Use this when the target text is approximate, duplicated, or not yet identified; use glass_wait_for_element for one precise runtime condition and glass_a11y_snapshot for broad tree inspection. `query` is a deterministic case-insensitive substring over accessible name, description and non-secure value; optional `role`/`states` narrow targets, optional `within` must match one semantic scope, `max_results` defaults to 10 and is capped at 20, `max_nodes` uses snapshot walk-limit semantics, and positive `timeout_ms` waits for a match. Returns trusted counts plus one untrusted match array with actionable ids and compact context. Complete success and error text is capped at 8 KiB."
    )]
    async fn glass_find_elements(
        &self,
        Parameters(a): Parameters<FindElementsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_find_elements",
            tool_effect("glass_find_elements"),
            move |glass| tools::find_elements(glass, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Capture the active window's current semantic state as a compact \
                       accessibility tree (role, name, description, bounded editable value, \
                       window-relative bounds and states). This is one observation, not proof of \
                       transition completion or visual appearance. For exact runtime verification, \
                       call glass_wait_for_element with the element's `name`, `description` and/or \
                       `role`, plus `value` for an exact editable value (`value_contains` for a \
                       substring). The compact value may be \
                       unavailable, redacted or truncated; use that wait rather than repeated \
                       snapshots when the full queryable value matters. Rendered as compact \
                       text — deterministic, low-token element addressing alongside \
                       screenshots. Each line is `#<id> <Role> \"<name>\" desc=\"<description>\" \
                       (x,y wxh) [states]`. desc carries a second label the platform exposes \
                       apart from the name, and appears only where one exists and differs from \
                       the name; glass_wait_for_element and glass_scroll_to_element can select \
                       it with the description parameter. Pass an #id to \
                       glass_click_element. Errors if the backend or app exposes no \
                       accessibility tree (e.g. a canvas/black-box app) — fall back to \
                       glass_screenshot then. Web content arrives under a `Document` element, \
                       and a childless `Document` is disclosed in its own notice: take a fresh \
                       snapshot first, then pixels. A placeholder the app published for content \
                       it has not exposed gets its own notice — only pixels reach it. Optional \
                       max_nodes: raise the element cap, or 0 to remove the element-count limit \
                       (default caps protect the token budget)."
    )]
    async fn glass_a11y_snapshot(
        &self,
        Parameters(a): Parameters<A11ySnapshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_a11y_snapshot",
            tool_effect("glass_a11y_snapshot"),
            move |g| tools::a11y_snapshot(g, &a),
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Click an element by its #id from glass_a11y_snapshot (actuates via the \
                       platform's native accessibility action when the element exposes one — \
                       works even when it's occluded or scrolled off-screen — else falls back \
                       to a synthetic pointer click at the center of its bounds; the result's \
                       `method` field says which path ran, `native_fallback` says why when \
                       the pointer path was used, and `actuated_id` names the element actually \
                       clicked when a control's label is a separate element from the control \
                       itself). If the element actually \
                       renders in a popover owned by a different window than the active one \
                       (e.g. an open dropdown's option row), the click is automatically routed \
                       into that popover window and the previously-active window is restored \
                       afterward. Ids are only valid within the latest snapshot — re-run \
                       glass_a11y_snapshot if the UI changed. Optional `return`: \"snapshot\" \
                       settles the UI then folds a fresh a11y tree into the result (and \
                       refreshes the snapshot \
                       cache); \"settle\" waits for the UI to stop changing (text-only); omit or \
                       \"none\" for no observe (default). If at least two next actions or waits are \
                       known, use glass_do instead of separate calls."
    )]
    async fn glass_click_element(
        &self,
        Parameters(a): Parameters<ClickElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_click_element",
            tool_effect("glass_click_element"),
            move |g| tools::click_element(g, &a),
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Set an editable element's value — pick the element's #id from \
                       glass_a11y_snapshot. Where the platform can write the value directly this is \
                       instant and takes no keystrokes; where it has to be typed, glass taps the \
                       element, clears it and types, then reads the element back to confirm — up to \
                       three accessibility reads, since a field may commit a frame or two later. \
                       Errors if the element isn't editable, if it changed \
                       since the snapshot (re-snapshot), if the element does not hold the requested \
                       value afterwards, or if the app exposes no accessibility tree. That \
                       does-not-hold error names both what you asked for and what the element \
                       holds, and which one it is decides your next move: your text in another \
                       form means the element transformed it and writing again will not help; part \
                       of your text means a keystroke was dropped, so write again; what it held \
                       before means the write took no effect, and the error then closes with what \
                       this backend knows about that. A separate \
                       error says the text WAS typed but the write could not be confirmed — the \
                       read-back failed, or could not tell which element now holds it. Do NOT write \
                       again on that one: the keystrokes already went out, and re-snapshotting is \
                       how you see where they landed. \
                       Optional `return`: \"snapshot\" settles the UI then folds a fresh a11y \
                       tree into the result (and refreshes the snapshot cache); \"settle\" waits \
                       for the UI to stop \
                       changing (text-only); omit or \"none\" for no observe (default). If at least \
                       two next actions or waits are known, use glass_do instead of separate calls."
    )]
    async fn glass_set_value(
        &self,
        Parameters(a): Parameters<SetValueArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_set_value",
            tool_effect("glass_set_value"),
            move |g| tools::set_value(g, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Screenshot of the active window with a numbered box drawn on each \
                       interactable element (Set-of-Mark) — returns the annotated image plus a \
                       text legend (`#<id> <Role> \"<name>\"`, or `#<id> <Role> \
                       desc=\"<description>\"` for an element that has only a description). \
                       Pick an element visually, then \
                       click it with glass_click_element using its #id (same ids as \
                       glass_a11y_snapshot). Chips sit just outside each element so small icon \
                       buttons stay visible. The box is only as precise as the toolkit's \
                       accessibility geometry (it can drift ~10-20px), but the #id and the click \
                       are exact (click_element actuates via the native accessibility action \
                       when available, else clicks the element's center). Errors if no \
                       accessibility tree is available — use glass_screenshot then."
    )]
    async fn glass_a11y_marks(&self) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_a11y_marks",
            tool_effect("glass_a11y_marks"),
            tools::a11y_marks,
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Read captured stdout/stderr log lines with a resumable cursor. glass_start \
                       captures the app's output from launch; this returns what has accumulated \
                       and a `cursor` to pass back next time, so a loop reads each line once. \
                       Returns immediately with whatever is there, including nothing at all — it \
                       does not wait, so use glass_wait_for_log when you want to block until a \
                       line appears (starting up, finishing work). Filter server-side with \
                       `stream` and `contains` rather than reading everything and scanning it \
                       yourself. The buffer keeps the most recent lines and drops the oldest, so \
                       a chatty app can age out lines you never read; the lines are the app's own \
                       output and are returned marked as untrusted."
    )]
    async fn glass_logs(
        &self,
        Parameters(a): Parameters<LogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run("glass_logs", tool_effect("glass_logs"), move |g| {
            tools::logs(g, &a)
        })
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Wait for semantic transition completion: block until an accessible element \
                       reaches a condition and optional value, then return it as text (no image). \
                       This verifies runtime semantic state, not pixels or visual stability. Select \
                       by `name` (accessible-name substring), `description` (accessible-description \
                       substring) and/or `role` (e.g. \"Button\"); `condition` (default appears): \
                       appears|disappears|enabled|\
                       disabled|checked|unchecked|selected|unselected|expanded|collapsed|focused|\
                       visible|hidden; `value` additionally requires an exact editable value, while \
                       `value_contains` requires a substring (combine either with a selector). Returns \
                       {matched,elapsed_ms} plus the matched element, including value — its id is usable \
                       with glass_click_element. On timeout returns {matched:false}. Waits through a \
                       just-launched app that has not published its accessibility tree yet, and \
                       errors if none appeared before the timeout. Collapses screenshot poll-loops \
                       into one call. If at least two next actions or waits are known, use glass_do \
                       instead of separate calls."
    )]
    async fn glass_wait_for_element(
        &self,
        Parameters(a): Parameters<WaitForElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_wait_for_element",
            tool_effect("glass_wait_for_element"),
            move |g| tools::wait_for_element(g, &a),
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Scroll a container (any axis) until an accessibility element is on-screen, \
                       then return it (text-only, no image). Requires the element to be actually \
                       visible — not merely present in the a11y tree — so the returned id is usable \
                       with glass_click_element. Select by `name` (accessible-name substring) \
                       and/or `role` (e.g. \"Button\"); optional `value_contains`. `direction`: \
                       \"up\"/\"down\"/\"left\"/\"right\"; omit to infer it from the target's \
                       off-screen position (falls back to a vertical down→up sweep when the target \
                       isn't in the tree yet). It sweeps that way to the end, then reverses. \
                       Optional `x`,`y` aim the swipe at a specific container; by default it anchors \
                       on the target's own row/column so a container that isn't window-centered \
                       (e.g. a top toolbar) is still driven. `step` sets wheel notches per move \
                       (default 3). Returns {matched,elapsed_ms,element{id,role,name,bounds,states},\
                       scrolled{steps,reversed,direction}} — the id is usable with \
                       glass_click_element. Returns {matched:false} if it never becomes visible \
                       after sweeping both ends or `timeout_ms` (default 20000). Errors if the app \
                       exposes no accessibility tree. If at least two next actions or waits are \
                       known, use glass_do instead of separate calls."
    )]
    async fn glass_scroll_to_element(
        &self,
        Parameters(a): Parameters<ScrollToElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_scroll_to_element",
            tool_effect("glass_scroll_to_element"),
            move |g| tools::scroll_to_element(g, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Wait for pixel transition completion: block until a visual region changes \
                       (diverges from a reference) or matches (converges to a saved baseline), then \
                       return text metrics (no image unless \
                       `include_image:true`). `until`: \"changes\" (default) or \"matches\" (needs \
                       `baseline`); optional window-relative `region`; `mode` perceptual|exact with \
                       `threshold`/`tolerance`. Returns {matched,changed_pct,bbox,elapsed_ms}. Use \
                       \"matches\" to confirm the UI reached an approved design without spending \
                       vision tokens. This verifies pixels, not semantic state or subsequent \
                       stability; use glass_wait_for_element for accessible conditions/values and \
                       glass_wait_stable when animation completion means visual quiescence."
    )]
    async fn glass_wait_for_region(
        &self,
        Parameters(a): Parameters<WaitForRegionArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_wait_for_region",
            tool_effect("glass_wait_for_region"),
            move |g| tools::wait_for_region(g, &a),
        )
        .await
    }

    #[tool(
        annotations(read_only_hint = true, open_world_hint = false),
        description = "Block until a log line containing `contains` (optionally on a given \
                       `stream`) appears, then return it as text. By default only lines emitted \
                       after this call count; pass a `cursor` from glass_logs to catch a line \
                       emitted just before. Returns {matched,line{seq,stream,text},cursor,elapsed_ms}; \
                       on timeout {matched:false}. Resume reading from the returned `cursor`."
    )]
    async fn glass_wait_for_log(
        &self,
        Parameters(a): Parameters<WaitForLogArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(
            "glass_wait_for_log",
            tool_effect("glass_wait_for_log"),
            move |g| tools::wait_for_log(g, &a),
        )
        .await
    }

    #[tool(
        annotations(
            read_only_hint = false,
            destructive_hint = false,
            open_world_hint = false
        ),
        description = "Prefer glass_do whenever at least two upcoming actions or verification waits \
                       are already known. Typical form flow: take one fresh glass_a11y_snapshot, \
                       retain the needed ids, then run set_value, wait_for_element, click_element, \
                       and wait_for_element here in one ordered call. Use standalone tools only when \
                       the next step depends on newly observed state. Inspect the structured outcomes \
                       before recovery. Run fixed static ordered actions in one call: click, move, drag, scroll, type, \
                       key, settle, click_element, set_value, wait_for_element, scroll_to_element. \
                       At most 64 actions and 65536 compact argument bytes. Optional absolute sequence \
                       timeout_ms defaults to 30000ms, is valid from 1 through 120000ms, and uses one absolute deadline shared by all actions and \
                       terminal settle/diff/screenshot. Fail-fast on action errors, sequence deadline, \
                       and unmatched batched wait_for_element/scroll_to_element predicates; standalone \
                       predicates remain soft. Successful calls return a structured completed outcome for every \
                       action. Once execution starts, action failures return completed, failed, and unexecuted action \
                       outcomes in the MCP error; terminal-observation failures return completed action outcomes plus \
                       terminal_steps. Preflight validation failures return an invalid_sequence error without step \
                       outcomes. Optional terminal settle, diff, screenshot adds corresponding terminal_steps outcomes. \
                       type retains return:\"none|settle|snapshot\" support. No variables, \
                       result bindings, interpolation, branching, loops, retries, or dynamic action generation."
    )]
    async fn glass_do(
        &self,
        Parameters(a): Parameters<DoArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run_batch("glass_do", tool_effect("glass_do"), move |g| {
            tools::do_actions(g, &a)
        })
        .await
    }
}

/// Server-level instructions shown once to the agent, describing glass's tool loop.
/// Must not name a backend (see `descriptions_name_no_backend`): capability support is a
/// runtime property, not documentation.
const SERVER_INSTRUCTIONS: &str = "glass gives you a build → see → interact → debug loop over a real native GUI \
     app — no app integration needed. One active session; tools target it implicitly; \
     choose a backend at glass_start (defaults to the host; see the `backend` param). \
     glass_start launches the app and captures its logs (glass_logs for stdout/stderr).\n\n\
     SEE AND ADDRESS THE UI CHEAPLY FIRST — the low-token default. When the target is approximate, \
     duplicated, or not yet identified, call glass_find_elements first: it returns a small ranked \
     set of actionable ids and context from a fresh accessibility read. Use glass_a11y_snapshot for \
     broad structural inspection, and glass_wait_for_element when one precise known condition must \
     become true. Retain returned ids only until the UI changes. Prefer this semantic path over \
     screenshots and pixel-hunting whenever it works.\n\n\
     BATCH KNOWN WORK: Prefer glass_do whenever at least two upcoming actions or verification waits \
     are already known. Typical form flow: take one fresh glass_a11y_snapshot, retain the needed ids, \
     then run set_value, wait_for_element, click_element, and wait_for_element in one ordered call. \
     Use standalone tools only when the next step depends on newly observed state. Inspect the \
     structured outcomes before recovery.\n\n\
     ADDRESS RETURNED IDS DIRECTLY: glass_click_element clicks one, glass_set_value writes an \
     editable element's value, and glass_wait_for_element verifies semantic transition completion, \
     including exact editable text with value.\n\n\
     PIXELS ARE THE FALLBACK — for a canvas/black-box app with no tree (glass_a11y_snapshot \
     errors there): glass_screenshot to see it, then glass_click / glass_type / glass_key / \
     glass_scroll / glass_drag (glass_gesture for multi-touch where supported) to interact. \
     Coordinates are WINDOW-RELATIVE — 0,0 is the app window's top-left. A screenshot is current \
     visual evidence only; glass_wait_for_region verifies pixel transition completion and \
     glass_wait_stable verifies visual quiescence.\n\n\
     VERIFY WITHOUT VISION TOKENS: glass_baseline_save a good frame, act, then glass_diff, \
     which returns changed_pct and a bbox as TEXT (no image). Only call glass_diff with \
     include_image=true (a cropped image of the changed region) when changed_pct shows \
     something moved — don't screenshot to check every step. glass_wait_for_region blocks \
     until a region changes or matches a saved baseline; glass_wait_for_log until a log line \
     appears. Successful input dispatch does not prove runtime state; verify the expected outcome \
     with the strongest matching wait. Waits return text only and time out softly with {matched:false} — branch on \
     that rather than retrying blindly.\n\n\
     Multiple windows: glass_list_windows and glass_select_window. Errors are real — a \
     failed capture or input returns a message, never a blank or stale frame; fix the \
     cause instead of retrying blindly.";

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GlassServer {
    fn get_info(&self) -> ServerInfo {
        let mut info = ServerInfo::new(
            ServerCapabilities::builder()
                .enable_tools()
                .enable_resources()
                .build(),
        )
        .with_instructions(SERVER_INSTRUCTIONS);
        // Identify the server as glass in the MCP `initialize` handshake. The rmcp default
        // (`Implementation::from_build_env`) reports the transport crate's own name and version
        // (`rmcp` / its crate version), not glass's — so every connecting client would see the wrong
        // server identity. `name` stays glass-mcp rather than server.json's
        // `io.github.fixed-width/glass`: the registry name is a namespaced identity token, and
        // `Implementation.name` is not.
        info.server_info.name = "glass-mcp".to_string();
        info.server_info.version = crate::VERSION.to_string();
        // Mirrored from `server.json` by build.rs so the handshake and the registry entry are
        // written once.
        info.server_info.title = Some(crate::TITLE.to_string());
        info.server_info.description = Some(crate::DESCRIPTION.to_string());
        info.server_info.website_url = crate::WEBSITE_URL.map(str::to_string);
        info
    }

    fn list_resources(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<ListResourcesResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListResourcesResult::default()))
    }

    fn list_resource_templates(
        &self,
        _request: Option<rmcp::model::PaginatedRequestParams>,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<ListResourceTemplatesResult, McpError>> + Send + '_ {
        std::future::ready(Ok(ListResourceTemplatesResult::default()))
    }

    fn read_resource(
        &self,
        request: ReadResourceRequestParams,
        _context: rmcp::service::RequestContext<rmcp::RoleServer>,
    ) -> impl Future<Output = Result<ReadResourceResult, McpError>> + Send + '_ {
        let store = self.artifacts.clone();
        let server_id = self.artifact_server_id.clone();
        async move {
            let uri = request.uri;
            tokio::task::spawn_blocking(move || {
                read_resource_result(store.as_ref(), &server_id, &uri)
            })
            .await
            .map_err(|_| resource_error(ArtifactReadError::ReadFailed, "artifact_read_failed"))?
        }
    }
}

fn read_resource_result(
    store: Option<&ArtifactStore>,
    server_id: &str,
    uri: &str,
) -> Result<ReadResourceResult, McpError> {
    classify_uri(uri, server_id).map_err(|error| resource_error(error, "resource_not_found"))?;
    let Some(store) = store else {
        return Err(resource_error(
            ArtifactReadError::ExpiredOrUnavailable,
            "artifact_expired_or_unavailable",
        ));
    };
    let read = store.read(uri).map_err(|error| match error {
        ArtifactReadError::ResourceNotFound => resource_error(error, "resource_not_found"),
        ArtifactReadError::ExpiredOrUnavailable => {
            resource_error(error, "artifact_expired_or_unavailable")
        }
        ArtifactReadError::ReadFailed => resource_error(error, "artifact_read_failed"),
        ArtifactReadError::IntegrityFailed => resource_error(error, "artifact_integrity_failed"),
    })?;
    let meta = Meta(serde_json::Map::from_iter([(
        "glass".to_string(),
        serde_json::json!({ "untrusted": read.untrusted, "sha256": read.sha256 }),
    )]));
    let contents = ResourceContents::text(read.text.clone(), uri)
        .with_mime_type(read.mime_type.clone())
        .with_meta(meta);
    let result = ReadResourceResult::new(vec![contents]);
    #[cfg(test)]
    fire_construction_observer();
    drop(read);
    Ok(result)
}

fn resource_error(error: ArtifactReadError, category: &'static str) -> McpError {
    let data = Some(serde_json::json!({ "category": category }));
    match error {
        ArtifactReadError::ResourceNotFound => {
            McpError::resource_not_found("resource not found", data)
        }
        ArtifactReadError::ExpiredOrUnavailable => McpError::resource_not_found(
            "artifact expired or unavailable; rerun the producing read-only operation if safe",
            data,
        ),
        ArtifactReadError::ReadFailed => {
            McpError::new(ErrorCode::INTERNAL_ERROR, "artifact read failed", data)
        }
        ArtifactReadError::IntegrityFailed => McpError::new(
            ErrorCode::INTERNAL_ERROR,
            "artifact integrity check failed",
            data,
        ),
    }
}

/// The live `#[tool]` registry's names. The doc-sync guard tests below bind
/// `docs/reference/tools.md` to this; [`crate::tools::testutil::assert_envelope`] binds a
/// test's expected envelope `tool` string to it too, so a co-typo shared between a tool impl
/// and its test (e.g. both saying `"glass_stop"` when the registered name is `"glass_stopp"`)
/// fails loudly instead of passing green.
#[cfg(test)]
pub(crate) fn registered_tools() -> std::collections::BTreeSet<String> {
    GlassServer::tool_router()
        .list_all()
        .into_iter()
        .map(|tool| tool.name.into_owned())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::{AxNode, AxRole, AxStates, HostPathAccess};
    use std::collections::{BTreeMap, BTreeSet};

    #[test]
    fn resources_are_advertised_without_subscription_or_list_changed_capabilities() {
        let server = GlassServer::new(
            crate::boot(None),
            crate::audit::report_from_config(None, |_| None),
        );
        let resources = server
            .get_info()
            .capabilities
            .resources
            .expect("resources capability");
        assert_ne!(resources.subscribe, Some(true));
        assert_ne!(resources.list_changed, Some(true));
    }

    #[test]
    fn every_host_path_access_maps_exactly_to_target_access() {
        let cases = [
            (
                HostPathAccess::DeniedBySandbox,
                TargetAccess::DeniedBySandbox,
            ),
            (
                HostPathAccess::NotGuaranteedSandboxOff,
                TargetAccess::NotGuaranteedSandboxOff,
            ),
            (
                HostPathAccess::HostFilesystemUnreachable,
                TargetAccess::HostFilesystemUnreachable,
            ),
            (HostPathAccess::NoActiveTarget, TargetAccess::NoActiveTarget),
        ];
        for (host, expected) in cases {
            assert_eq!(target_access(host), expected);
        }
    }

    #[test]
    fn unavailable_resource_identity_distinguishes_invalid_foreign_and_current_uris() {
        let server = GlassServer::new_unavailable_for_test("current-server");
        let cases = [
            ("not-a-uri", "resource_not_found"),
            (
                "https://current-server/0123456789abcdef0123456789abcdef",
                "resource_not_found",
            ),
            (
                "glass-artifact://foreign-server/0123456789abcdef0123456789abcdef",
                "resource_not_found",
            ),
            (
                "glass-artifact://current-server/0123456789abcdef0123456789abcdef",
                "artifact_expired_or_unavailable",
            ),
        ];

        for (uri, category) in cases {
            let error = server
                .read_resource_for_test(uri)
                .expect_err("read must fail");
            assert_eq!(
                error.data.as_ref().and_then(|v| v["category"].as_str()),
                Some(category)
            );
            assert!(error.message.len() < 160);
        }
    }

    #[test]
    fn registered_resource_failures_are_bounded_and_categorized_at_server_boundary() {
        for (fault, category) in [
            (
                crate::artifacts::FaultStage::ReadBodyFails,
                "artifact_read_failed",
            ),
            (
                crate::artifacts::FaultStage::GrowDuringRead,
                "artifact_integrity_failed",
            ),
        ] {
            let root = tempfile::tempdir().expect("temporary root");
            let store = ArtifactStore::for_test_with_fault(root.path(), 1 << 20, fault)
                .expect("fault store");
            let prepared = store
                .prepare(crate::artifacts::ArtifactDraft::content_block(
                    "secret-artifact-body",
                    "text/plain",
                    true,
                    0,
                ))
                .expect("prepare");
            let published = store.publish(vec![prepared]).expect("publish");
            let uri = published.descriptors()[0].uri().to_owned();
            let error = read_resource_result(Some(&store), &store.server_id(), &uri)
                .expect_err("read must fail");
            let rendered = serde_json::to_string(&error).expect("serialize error");
            assert_eq!(
                error
                    .data
                    .as_ref()
                    .and_then(|value| value["category"].as_str()),
                Some(category)
            );
            assert!(!rendered.contains("secret-artifact-body"));
            assert!(!rendered.contains(root.path().to_string_lossy().as_ref()));
            assert!(error.message.len() < 160);
        }
    }

    #[test]
    fn production_tool_effects_match_live_annotations() {
        for tool in GlassServer::tool_router().list_all() {
            let annotated_read_only = tool
                .annotations
                .as_ref()
                .and_then(|annotations| annotations.read_only_hint)
                == Some(true);
            assert_eq!(
                tool_effect(tool.name.as_ref()),
                if annotated_read_only {
                    ToolEffect::ReadOnly
                } else {
                    ToolEffect::MayMutate
                },
                "{}",
                tool.name
            );
        }
    }

    #[tokio::test]
    async fn worker_spawn_failure_returns_bounded_unavailable_result() {
        FAIL_WORKER_SPAWN.with(|fail| fail.set(true));
        let server = GlassServer::new_unavailable_for_test("worker-test");
        FAIL_WORKER_SPAWN.with(|fail| fail.set(false));
        let result = server
            .run("glass_test", ToolEffect::ReadOnly, |_| {
                panic!("worker must not run the target body")
            })
            .await
            .expect("bounded tool result");
        assert_eq!(result.is_error, Some(true));
        assert!(first_text(&result).len() < 160);
    }

    #[test]
    fn response_pin_survives_complete_call_result_construction() {
        let root = tempfile::tempdir().expect("temporary root");
        let store = ArtifactStore::for_test(root.path(), 8_299).expect("artifact store");
        let applied = OutputPolicy::new(store.clone()).apply(ToolCallOutcome {
            tool: "glass_logs",
            effect: ToolEffect::ReadOnly,
            is_error: false,
            target_access: TargetAccess::NoActiveTarget,
            output: ToolOutput::result_with(
                "glass_logs",
                serde_json::json!({}),
                vec![OutContent::trusted_guidance("x".repeat(8_300))],
            ),
        });
        let uri = applied
            .output
            .0
            .iter()
            .find_map(|content| match content {
                OutContent::ResourceLink(descriptor) => Some(descriptor.uri().to_owned()),
                _ => None,
            })
            .expect("resource link");
        let observed = store.clone();
        let observed_uri = uri.clone();
        set_construction_observer(move || {
            observed
                .enforce_retention()
                .expect("retention during construction");
            assert!(observed.read(&observed_uri).is_ok());
        });

        let _result = applied_to_call_result(applied, TargetAccess::NoActiveTarget);
        store.enforce_retention().expect("retention after handoff");
        assert_eq!(
            store.read(&uri).unwrap_err(),
            ArtifactReadError::ExpiredOrUnavailable
        );
    }

    #[test]
    fn read_pin_survives_complete_resource_result_construction() {
        let root = tempfile::tempdir().expect("temporary root");
        let store = ArtifactStore::for_test(root.path(), 16).expect("artifact store");
        let prepared = store
            .prepare(crate::artifacts::ArtifactDraft::content_block(
                "0123456789abcdef",
                "text/plain",
                false,
                0,
            ))
            .expect("prepare");
        let published = store.publish(vec![prepared]).expect("publish");
        let uri = published.descriptors()[0].uri().to_owned();
        let pin = published.into_pin();
        drop(pin);
        std::fs::write(store.process_dir().join("residue"), b"x").expect("residue");
        let observed = store.clone();
        let observed_uri = uri.clone();
        set_construction_observer(move || {
            observed
                .enforce_retention()
                .expect("retention during construction");
            assert!(observed.read(&observed_uri).is_ok());
        });

        assert!(read_resource_result(Some(&store), &store.server_id(), &uri).is_ok());
        store.enforce_retention().expect("retention after handoff");
        assert_eq!(
            store.read(&uri).unwrap_err(),
            ArtifactReadError::ExpiredOrUnavailable
        );
    }

    #[test]
    fn oversized_read_only_outcome_is_bounded_and_link_reads_exact_full_text() {
        let root = tempfile::tempdir().expect("temporary artifact root");
        let store = ArtifactStore::for_test(root.path(), 64 * 1024 * 1024).expect("artifact store");
        let body = "application-output-".repeat(700);
        let observation = OutContent::untrusted_observation(&body);
        let expected = observation.render_text().expect("wrapped observation");
        let output =
            ToolOutput::result_with("glass_logs", serde_json::json!({}), vec![observation]);
        let applied = OutputPolicy::new(store.clone()).apply(ToolCallOutcome {
            tool: "glass_logs",
            effect: ToolEffect::ReadOnly,
            is_error: false,
            target_access: TargetAccess::DeniedBySandbox,
            output,
        });
        assert!(applied.output.text_bytes() <= crate::output_policy::MAX_TEXT_BYTES);
        let descriptor = applied
            .output
            .0
            .iter()
            .find_map(|content| match content {
                OutContent::ResourceLink(descriptor) => Some(descriptor),
                _ => None,
            })
            .expect("resource link");
        let read = store
            .read(descriptor.uri())
            .expect("complete artifact read");
        assert_eq!(read.text, expected);
        assert!(read.untrusted);
        assert_eq!(read.sha256, descriptor.sha256());
    }

    #[test]
    fn server_clones_share_policy_and_artifact_registry() {
        let root = tempfile::tempdir().expect("temporary artifact root");
        let store = ArtifactStore::for_test(root.path(), 64 * 1024 * 1024).expect("artifact store");
        let server = GlassServer::new_with_store(
            crate::boot(None),
            crate::audit::report_from_config(None, |_| None),
            store,
        )
        .expect("server with store");
        let clone = server.clone();
        assert!(Arc::ptr_eq(&server.output_policy, &clone.output_policy));
        assert_eq!(
            server.artifact_store().map(|store| store.server_id()),
            clone.artifact_store().map(|store| store.server_id())
        );
    }

    #[test]
    fn glass_do_schema_and_description_advertise_the_bounded_static_contract() {
        let tool = GlassServer::tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "glass_do")
            .expect("glass_do is registered");
        let schema = serde_json::Value::Object((*tool.input_schema).clone());
        let defs = schema["$defs"].as_object().expect("schema definitions");
        let alternatives = defs["Action"]["oneOf"]
            .as_array()
            .expect("Action alternatives");
        let discriminators: BTreeSet<&str> = alternatives
            .iter()
            .map(|alternative| {
                alternative["properties"]["action"]["const"]
                    .as_str()
                    .expect("action discriminator const")
            })
            .collect();
        let expected: BTreeSet<&str> = [
            "click",
            "move",
            "drag",
            "scroll",
            "type",
            "key",
            "settle",
            "click_element",
            "set_value",
            "wait_for_element",
            "scroll_to_element",
        ]
        .into_iter()
        .collect();
        assert_eq!(discriminators, expected);
        assert!(schema["properties"].get("timeout_ms").is_some(), "{schema}");
        let sequence_timeout = schema["properties"]["timeout_ms"]["description"]
            .as_str()
            .expect("glass_do timeout description");
        assert!(sequence_timeout.contains("Overall sequence budget"));
        assert!(sequence_timeout.contains("1..=120000"));
        assert!(sequence_timeout.contains("One absolute deadline"));

        let settle_timeout = defs["SettleArgs"]["properties"]["timeout_ms"]["description"]
            .as_str()
            .expect("settle timeout description");
        assert!(settle_timeout.contains("settled:false and completes the step"));
        assert!(settle_timeout.contains("enclosing glass_do"));
        assert!(settle_timeout.contains("deadline fails the sequence"));

        fn integer_keyword(value: &serde_json::Value, keyword: &str) -> Option<i64> {
            value
                .get(keyword)
                .and_then(serde_json::Value::as_i64)
                .or_else(|| {
                    value.as_object().and_then(|object| {
                        object
                            .values()
                            .find_map(|child| integer_keyword(child, keyword))
                    })
                })
                .or_else(|| {
                    value.as_array().and_then(|items| {
                        items
                            .iter()
                            .find_map(|child| integer_keyword(child, keyword))
                    })
                })
        }

        let click_count = &defs["ClickArgs"]["properties"]["count"];
        assert_eq!(integer_keyword(click_count, "minimum"), Some(1));
        assert_eq!(integer_keyword(click_count, "maximum"), Some(10));
        assert!(
            click_count["description"]
                .as_str()
                .is_some_and(|description| description.contains("1 through 10")),
            "{click_count}"
        );

        for axis in ["dx", "dy"] {
            let magnitude = &defs["ScrollArgs"]["properties"][axis];
            assert_eq!(integer_keyword(magnitude, "minimum"), Some(-100));
            assert_eq!(integer_keyword(magnitude, "maximum"), Some(100));
            assert!(
                magnitude["description"]
                    .as_str()
                    .is_some_and(|description| description.contains("-100 through 100")),
                "{axis}: {magnitude}"
            );
        }

        let click_element_id = defs["ClickElementArgs"]["properties"]["id"]["description"]
            .as_str()
            .expect("click_element id description");
        assert!(click_element_id.contains("role-appropriate native accessibility operation"));
        let click_element_id_lower = click_element_id.to_ascii_lowercase();
        assert!(click_element_id_lower.contains("text editors"));
        assert!(click_element_id_lower.contains("may receive focus"));
        fn has_property(value: &serde_json::Value, name: &str) -> bool {
            value.as_object().is_some_and(|object| {
                object
                    .get("properties")
                    .and_then(serde_json::Value::as_object)
                    .is_some_and(|properties| properties.contains_key(name))
                    || object.values().any(|child| has_property(child, name))
            }) || value
                .as_array()
                .is_some_and(|items| items.iter().any(|item| has_property(item, name)))
        }
        assert!(!has_property(&schema, "encoded_argument_bytes"), "{schema}");

        let description = tool.description.as_deref().expect("description");
        for required in [
            "fixed static ordered actions",
            "64 actions",
            "65536 compact argument bytes",
            "timeout_ms",
            "30000",
            "1 through 120000",
            "120000",
            "click, move, drag, scroll, type, key, settle, click_element, set_value, wait_for_element, scroll_to_element",
            "Fail-fast",
            "action errors, sequence deadline, and unmatched batched wait_for_element/scroll_to_element predicates",
            "standalone predicates remain soft",
            "Successful calls return a structured completed outcome for every action",
            "Once execution starts, action failures return completed, failed, and unexecuted action outcomes in the MCP error",
            "terminal-observation failures return completed action outcomes plus terminal_steps",
            "Preflight validation failures return an invalid_sequence error without step outcomes",
            "wait_for_element",
            "scroll_to_element",
            "Optional terminal settle, diff, screenshot",
            "type retains return:\"none|settle|snapshot\"",
            "No variables, result bindings, interpolation, branching, loops, retries, or dynamic action generation",
        ] {
            assert!(
                description.contains(required),
                "description missing {required:?}: {description}"
            );
        }
        assert!(
            !description.contains("type.return is rejected"),
            "{description}"
        );
    }

    #[test]
    fn glass_do_guidance_leads_with_the_selection_rule() {
        let description = GlassServer::tool_router()
            .list_all()
            .into_iter()
            .find(|tool| tool.name == "glass_do")
            .expect("glass_do is registered")
            .description
            .expect("glass_do has a description");
        let selection_rule = "Prefer glass_do whenever at least two upcoming actions or verification waits are already known";
        assert!(
            description.starts_with(selection_rule),
            "glass_do must lead with when to choose it: {description}"
        );
        for required in [
            "one fresh glass_a11y_snapshot",
            "set_value, wait_for_element, click_element, and wait_for_element",
            "Use standalone tools only when the next step depends on newly observed state",
            "Inspect the structured outcomes before recovery",
        ] {
            assert!(
                description.contains(required),
                "glass_do description missing {required:?}: {description}"
            );
            assert!(
                SERVER_INSTRUCTIONS.contains(required),
                "server instructions missing {required:?}"
            );
        }

        let batch = SERVER_INSTRUCTIONS
            .find(selection_rule)
            .expect("server instructions lead agents toward glass_do");
        let standalone = SERVER_INSTRUCTIONS
            .find("glass_click_element clicks one")
            .expect("server instructions describe standalone semantic tools");
        assert!(
            batch < standalone,
            "the batching decision rule must appear before standalone action guidance"
        );
    }

    #[test]
    fn batch_eligible_standalone_descriptions_redirect_known_sequences() {
        let descriptions: BTreeMap<String, String> = GlassServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    tool.description.unwrap_or_default().to_string(),
                )
            })
            .collect();
        let redirect = "If at least two next actions or waits are known, use glass_do instead of separate calls.";
        for name in [
            "glass_click",
            "glass_move",
            "glass_drag",
            "glass_scroll",
            "glass_type",
            "glass_key",
            "glass_wait_stable",
            "glass_click_element",
            "glass_set_value",
            "glass_wait_for_element",
            "glass_scroll_to_element",
        ] {
            assert!(
                descriptions[name].contains(redirect),
                "{name} must redirect known multi-step work to glass_do: {}",
                descriptions[name]
            );
        }
    }

    fn first_text(r: &CallToolResult) -> String {
        r.content[0].as_text().expect("text content").text.clone()
    }

    fn assert_complete_externalized_result(result: &CallToolResult) {
        let text_bytes = result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .map(|text| text.text.len())
            .sum::<usize>();
        let envelope: serde_json::Value = result
            .content
            .iter()
            .filter_map(|content| content.as_text())
            .find_map(|text| serde_json::from_str(&text.text).ok())
            .unwrap_or_else(|| panic!("result envelope missing: {result:?}"));
        let rendered = serde_json::to_string(result).expect("serialized MCP result");
        assert!(text_bytes <= crate::output_policy::MAX_TEXT_BYTES);
        assert!(rendered.contains("glass-artifact://"), "{rendered}");
        assert_eq!(envelope["result"]["output"]["complete"], true);
    }

    fn large_server_tree() -> glass_core::AxTree {
        let mut tree = crate::tools::testutil::fake_tree();
        tree.root.children.extend((0..240).map(|index| AxNode {
            id: glass_core::AxNodeId(0),
            role: AxRole::Other,
            raw_role: "static_text".into(),
            name: Some(format!("application row {index} {}", "x".repeat(80))),
            description: None,
            value: None,
            states: AxStates::default(),
            bounds: None,
            children: vec![],
        }));
        tree.assign_ids();
        tree
    }

    #[tokio::test]
    async fn explicit_and_automatic_snapshots_share_complete_server_output_policy() {
        let root = tempfile::tempdir().expect("artifact root");
        let store = ArtifactStore::for_test(root.path(), 64 * 1024 * 1024).expect("artifact store");
        let glass = crate::tools::testutil::glass_with_a11y(
            crate::tools::testutil::FakePlatform::new(100, 100)
                .with_frames(vec![glass_core::Frame::solid(100, 100, [0, 0, 0, 255]); 4]),
            large_server_tree(),
        );
        let server = GlassServer::new_with_store(
            glass,
            crate::audit::report_from_config(None, |_| None),
            store,
        )
        .expect("server with store");
        let sessions = server.sessions();
        let mut glass = sessions.lock().await;
        glass
            .set_protected_host_paths(vec![])
            .expect("clear fake-backend-only protection paths");
        glass
            .start(&glass_core::AppSpec {
                build: None,
                run: vec!["app".into()],
                cwd: None,
                env: vec![],
                window_hint: None,
                timeout_ms: 1,
                sandbox: glass_core::SandboxLevel::Off,
                a11y: true,
            })
            .expect("start target");
        drop(glass);

        let explicit = server
            .glass_a11y_snapshot(Parameters(A11ySnapshotArgs { max_nodes: Some(0) }))
            .await
            .expect("explicit snapshot");
        let automatic = server
            .glass_click_element(Parameters(ClickElementArgs {
                id: 1,
                return_: Some("snapshot".into()),
            }))
            .await
            .expect("automatic snapshot");

        assert_complete_externalized_result(&explicit);
        assert_complete_externalized_result(&automatic);
    }

    #[test]
    fn structured_error_preserves_every_content_block_and_sets_is_error() {
        let out = ToolOutput(vec![
            OutContent::trusted_error(
                r#"{"ok":false,"tool":"glass_do","error":{"code":"step_failed"}}"#,
            ),
            OutContent::trusted_error("detail"),
        ]);
        let r = map_call_outcome(ToolCallOutcome {
            tool: "glass_test",
            effect: ToolEffect::ReadOnly,
            is_error: true,
            target_access: TargetAccess::NoActiveTarget,
            output: out,
        });
        assert_eq!(r.is_error, Some(true));
        assert_eq!(r.content.len(), 2);
        assert!(first_text(&r).contains("step_failed"));
    }

    #[test]
    fn ordinary_string_error_keeps_its_one_block_wire_shape() {
        let r = map_tool_result(Err("capture failed".to_string()));
        assert_eq!(r.is_error, Some(true));
        assert_eq!(r.content.len(), 1);
        assert!(first_text(&r).contains("capture failed"));
    }

    #[test]
    fn map_tool_result_flags_err_as_error() {
        let r = map_tool_result(Err("capture failed".to_string()));
        assert_eq!(
            r.is_error,
            Some(true),
            "an Err must surface as an MCP error result"
        );
        assert!(
            first_text(&r).contains("capture failed"),
            "got {:?}",
            first_text(&r)
        );
    }

    #[test]
    fn map_tool_result_marks_ok_as_success() {
        let r = map_tool_result(Ok(ToolOutput::result(
            "glass_test",
            serde_json::json!("done"),
        )));
        assert_eq!(
            r.is_error,
            Some(false),
            "an Ok must surface as a success result"
        );
        assert!(first_text(&r).contains("done"), "got {:?}", first_text(&r));
    }

    fn find_args() -> FindElementsArgs {
        FindElementsArgs {
            query: Some("save".into()),
            role: None,
            states: None,
            within: None,
            max_results: None,
            max_nodes: None,
            timeout_ms: None,
        }
    }

    fn assert_bounded_find_error(result: ToolResult, category: &str, guidance: &str) {
        let mapped = map_tool_result(result);
        assert_eq!(mapped.is_error, Some(true));
        let text = first_text(&mapped);
        assert!(text.contains(category), "{text}");
        assert!(text.contains(guidance), "{text}");
        assert!(text.len() <= crate::output_policy::MAX_TEXT_BYTES);
    }

    #[test]
    fn find_mcp_error_preserves_no_session_category_and_guidance() {
        let mut glass =
            crate::tools::testutil::glass_with(crate::tools::testutil::FakePlatform::new(100, 100));
        assert_bounded_find_error(
            crate::tools::find_elements(&mut glass, &find_args()),
            "no_active_session",
            "Call glass_start",
        );
    }

    #[test]
    fn find_mcp_error_preserves_unsupported_accessibility_category_and_guidance() {
        let mut glass = crate::tools::testutil::started_without_a11y();
        assert_bounded_find_error(
            crate::tools::find_elements(&mut glass, &find_args()),
            "unsupported_accessibility",
            "Use glass_screenshot",
        );
    }

    #[test]
    fn find_mcp_transport_error_is_bounded_and_does_not_echo_backend_detail() {
        let sentinel = "backend-secret-sentinel";
        let mut glass = crate::tools::testutil::started_failing_a11y(
            glass_core::GlassError::Backend(format!("{sentinel} {}", "x".repeat(20_000))),
        );
        let result = crate::tools::find_elements(&mut glass, &find_args());
        let mapped = map_tool_result(result);
        let text = first_text(&mapped);
        assert_eq!(mapped.is_error, Some(true));
        assert!(text.contains("transport_failure"), "{text}");
        assert!(
            text.contains("Retry after checking the backend connection"),
            "{text}"
        );
        assert!(!text.contains(sentinel), "{text}");
        assert!(text.len() <= crate::output_policy::MAX_TEXT_BYTES);
    }

    /// android is always compiled in (host-OS-agnostic), so it's a stable choice for
    /// exercising the registered handler end to end without depending on the host OS.
    #[tokio::test]
    async fn glass_capabilities_returns_json_for_the_active_backend() {
        let glass =
            crate::tools::testutil::glass_with(crate::tools::testutil::FakePlatform::new(100, 100));
        let report = crate::audit::report_from_config(None, |_| None);
        let server = GlassServer::new(glass, report);

        let out = server
            .glass_capabilities(Parameters(CapabilitiesArgs {
                backend: Some("android".into()),
            }))
            .await
            .unwrap();

        let text = first_text(&out);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["tool"], "glass_capabilities");
        let result = &v["result"];
        assert_eq!(result["backend"], "android");
        assert_eq!(result["available"], true);
        assert_eq!(
            result["capabilities"]["window_move_resize"]["status"],
            "unsupported"
        );
    }

    #[tokio::test]
    async fn glass_capabilities_rejects_an_unknown_backend() {
        let glass =
            crate::tools::testutil::glass_with(crate::tools::testutil::FakePlatform::new(100, 100));
        let report = crate::audit::report_from_config(None, |_| None);
        let server = GlassServer::new(glass, report);

        let out = server
            .glass_capabilities(Parameters(CapabilitiesArgs {
                backend: Some("nope".into()),
            }))
            .await
            .unwrap();

        assert_eq!(out.is_error, Some(true));
        assert!(first_text(&out).contains("nope"));
    }

    #[test]
    fn get_info_identifies_the_server_as_glass_not_the_transport_crate() {
        let glass =
            crate::tools::testutil::glass_with(crate::tools::testutil::FakePlatform::new(10, 10));
        let report = crate::audit::report_from_config(None, |_| None);
        let server = GlassServer::new(glass, report);
        let info = server.get_info();
        // Must override the rmcp default (which reports the transport crate's own name/version).
        assert_eq!(
            info.server_info.name, "glass-mcp",
            "the MCP handshake must identify glass, not the rmcp transport crate"
        );
        assert_eq!(info.server_info.title.as_deref(), Some("glass"));
        assert_eq!(
            info.server_info.version,
            crate::VERSION,
            "handshake version must be glass's build-time version, not the crate's 0.0.0 or rmcp's"
        );
    }

    /// The descriptor this repo publishes to the MCP registry.
    const SERVER_JSON: &str = include_str!("../../../server.json");

    /// The `server.json` keys build.rs mirrors onto the handshake, split by whether the descriptor
    /// must carry them: the registry schema makes `websiteUrl` optional, so the handshake omits it
    /// rather than the build failing.
    const MIRRORED_REQUIRED_KEYS: &[&str] = &["title", "description"];
    const MIRRORED_OPTIONAL_KEYS: &[&str] = &["websiteUrl"];

    fn server_json() -> serde_json::Value {
        serde_json::from_str(SERVER_JSON).expect("server.json is valid JSON")
    }

    /// Compares against `server.json` rather than repeating its strings here: a literal copy would
    /// go stale exactly when the handshake did, and pass.
    #[test]
    fn get_info_mirrors_server_jsons_title_description_and_website_url() {
        let descriptor = server_json();

        let glass =
            crate::tools::testutil::glass_with(crate::tools::testutil::FakePlatform::new(10, 10));
        let report = crate::audit::report_from_config(None, |_| None);
        let info = GlassServer::new(glass, report).get_info();

        for (key, got) in [
            ("title", info.server_info.title.as_deref()),
            ("description", info.server_info.description.as_deref()),
        ] {
            // Unwrap the expected value rather than comparing Option-to-Option: a missing key
            // would otherwise match an unset handshake field and pass green with neither
            // existing. `every_server_json_key_is_classified` keeps both present.
            let want = descriptor[key]
                .as_str()
                .unwrap_or_else(|| panic!("server.json carries a `{key}`"));
            assert_eq!(
                got,
                Some(want),
                "the handshake must carry server.json's `{key}`, not omit it or echo another field"
            );
            // Reachable only if build.rs's length check is dropped; the MCP registry rejects an
            // empty value at publish time (minLength 1).
            assert!(!want.is_empty(), "server.json's `{key}` must not be empty");
        }

        // Option-to-Option is the contract here, not a weakened assertion: an absent `websiteUrl`
        // must reach the handshake as an omitted field, not an empty string.
        assert_eq!(
            info.server_info.website_url.as_deref(),
            descriptor["websiteUrl"].as_str(),
            "the handshake must carry server.json's `websiteUrl` when it has one, and omit the \
             field when it does not"
        );
    }

    /// Partition every `server.json` key, so a key added later has to be classified rather than
    /// silently left off the handshake — the omission #510 fixed, one field over.
    #[test]
    fn every_server_json_key_is_classified() {
        // `name` is the registry's namespaced identity token, not `Implementation.name` (which
        // stays `glass-mcp`); `version` is rewritten from the release tag at publish time while
        // the handshake reports the running build's own git-derived version; `$schema` and
        // `repository` have no handshake counterpart in any spec revision.
        const REGISTRY_ONLY_KEYS: &[&str] = &["$schema", "name", "version", "repository"];

        let descriptor = server_json();
        let keys: Vec<&str> = descriptor
            .as_object()
            .expect("server.json is a JSON object")
            .keys()
            .map(String::as_str)
            .collect();

        let classified = |k: &&str| {
            MIRRORED_REQUIRED_KEYS.contains(k)
                || MIRRORED_OPTIONAL_KEYS.contains(k)
                || REGISTRY_ONLY_KEYS.contains(k)
        };
        let unclassified: Vec<&str> = keys.iter().copied().filter(|k| !classified(k)).collect();
        assert!(
            unclassified.is_empty(),
            "server.json key(s) {unclassified:?} are neither mirrored onto the handshake nor \
             registry-only: mirror them in build.rs's emit_server_json_identity, or add them to \
             REGISTRY_ONLY_KEYS"
        );
        for key in MIRRORED_REQUIRED_KEYS {
            assert!(
                keys.contains(key),
                "`{key}` is mirrored onto the handshake but server.json no longer carries it"
            );
        }
    }

    #[tokio::test]
    async fn glass_doctor_envelopes_the_report_text() {
        let glass =
            crate::tools::testutil::glass_with(crate::tools::testutil::FakePlatform::new(100, 100));
        let report = crate::audit::report_from_config(None, |_| None);
        let server = GlassServer::new(glass, report);

        let out = server
            .glass_doctor(Parameters(DoctorArgs { deep: None }))
            .await
            .unwrap();

        let text = first_text(&out);
        let v: serde_json::Value = serde_json::from_str(&text).unwrap();
        assert_eq!(v["ok"], true);
        assert_eq!(v["tool"], "glass_doctor");
        // Escape-free as well as non-empty — this is the seam where a styled render would reach
        // an agent's JSON.
        assert!(
            v["result"]["report"]
                .as_str()
                .is_some_and(|s| !s.is_empty() && !s.contains('\x1b')),
            "expected a non-empty, escape-free result.report string, got {v}"
        );
    }

    #[test]
    fn doctor_result_carries_overall_and_the_section_check_structure() {
        use glass_core::{Check, CheckStatus, Diagnosis, Section};

        let diag = Diagnosis::new(vec![Section::new(
            "x11",
            Some("x11".into()),
            vec![
                Check::new("Xvfb", CheckStatus::Fail, "not found").with_remedy("install it"),
                Check::new("software GL", CheckStatus::Ok, "present"),
            ],
        )]);

        let v = doctor_result(&diag, "x11");

        // The single field a consumer should branch on: x11 is the queried backend, so its
        // Fail is critical and the verdict is "fail" — lowercase, not the Rust-side
        // `CheckStatus::Fail` debug spelling.
        assert_eq!(v["overall"], "fail");
        assert_eq!(
            v["sections"][0]["checks"][0]["status"], "fail",
            "a Fail check must survive serialization as the lowercase string: {v}"
        );
        assert_eq!(v["sections"][0]["checks"][0]["name"], "Xvfb");
        assert_eq!(v["sections"][0]["checks"][1]["status"], "ok");
        assert!(
            v["report"].as_str().is_some_and(|s| s.contains("Xvfb")),
            "report must still carry the rendered text: {v}"
        );
    }

    // "windows" is excluded from `banned` below (collides with the plain noun, e.g.
    // glass_list_windows). These phrases name the Windows *backend* specifically and don't
    // occur as innocent noun usage, so match them directly.
    const WINDOWS_BACKEND_PHRASES: &[&str] = &["windows backend", "windows host", "on windows"];

    /// True if `text` names a Windows-*backend* phrase. Each phrase must appear as a run of
    /// CONSECUTIVE words (same word-boundary tokenizer as `names` below), case-insensitively —
    /// a raw substring match would false-positive on e.g. "repositi·on windows".
    fn names_windows_backend(text: &str) -> bool {
        let words: Vec<&str> = text
            .split(|c: char| !c.is_ascii_alphanumeric())
            .filter(|w| !w.is_empty())
            .collect();
        WINDOWS_BACKEND_PHRASES.iter().any(|phrase| {
            let needle: Vec<&str> = phrase.split(' ').collect();
            words.windows(needle.len()).any(|run| {
                run.iter()
                    .zip(&needle)
                    .all(|(w, n)| w.eq_ignore_ascii_case(n))
            })
        })
    }

    #[test]
    fn windows_backend_phrase_is_flagged_not_the_plain_noun() {
        for phrase in [
            "Only supported on the Windows backend.",
            "runs on Windows host",
            "available on Windows",
        ] {
            assert!(
                names_windows_backend(phrase),
                "should flag windows-backend phrase: {phrase:?}"
            );
        }
        for phrase in [
            "tile the windows",
            "lists top-level windows",
            "reposition windows to tile them",
            "position windows on the screen",
        ] {
            assert!(
                !names_windows_backend(phrase),
                "should not flag the plain noun: {phrase:?}"
            );
        }
    }

    /// A session runs one backend, so naming a backend in a description is a mode-conditional
    /// the agent can't resolve — and even a single named gate rots the day another backend
    /// gains the capability. Which backends support a capability is a runtime property, not
    /// documentation. Vocabulary = BACKENDS minus "windows" (collides with the plain noun,
    /// e.g. glass_list_windows) plus the two Linux display servers that ARE those backends.
    #[test]
    fn descriptions_name_no_backend() {
        let banned: Vec<&str> = crate::BACKENDS
            .iter()
            .copied()
            .filter(|&b| b != "windows")
            .chain(["xvfb", "sway"])
            .collect();

        // Case-insensitive, word-boundary match so "ios" can't hit inside another word.
        fn names(text: &str, tok: &str) -> bool {
            text.split(|c: char| !c.is_ascii_alphanumeric())
                .any(|w| w.eq_ignore_ascii_case(tok))
        }

        let mut problems: Vec<String> = Vec::new();
        for tool in GlassServer::tool_router().list_all() {
            // A tool with no description gives the guard nothing to check — that's a gap in
            // the guard, not a pass, so it's a recorded problem rather than silently ok.
            let desc = match tool.description.as_deref() {
                Some(d) if !d.is_empty() => d,
                _ => {
                    problems.push(format!("  {}: has no description to check", tool.name));
                    continue;
                }
            };
            for tok in &banned {
                if names(desc, tok) {
                    problems.push(format!("  {}: names backend '{tok}'", tool.name));
                }
            }
            if names_windows_backend(desc) {
                problems.push(format!("  {}: names the Windows backend", tool.name));
            }
        }
        for tok in &banned {
            if names(SERVER_INSTRUCTIONS, tok) {
                problems.push(format!("  get_info: names backend '{tok}'"));
            }
        }
        if names_windows_backend(SERVER_INSTRUCTIONS) {
            problems.push("  get_info: names the Windows backend".to_string());
        }
        assert!(
            problems.is_empty(),
            "tool descriptions/instructions must not name a backend \
             (capability support is dynamic, not documentation):\n{}",
            problems.join("\n")
        );
    }

    #[test]
    fn find_elements_is_registered_read_only_and_semantic_first() {
        let tools = GlassServer::tool_router().list_all();
        let find = tools
            .iter()
            .find(|tool| tool.name == "glass_find_elements")
            .expect("registered");
        assert_eq!(
            find.annotations.as_ref().and_then(|a| a.read_only_hint),
            Some(true)
        );
        assert_eq!(
            find.annotations.as_ref().and_then(|a| a.open_world_hint),
            Some(false)
        );
        let find_pos = SERVER_INSTRUCTIONS.find("glass_find_elements").unwrap();
        let snapshot_pos = SERVER_INSTRUCTIONS.find("glass_a11y_snapshot").unwrap();
        let screenshot_pos = SERVER_INSTRUCTIONS.find("glass_screenshot").unwrap();
        assert!(find_pos < snapshot_pos);
        assert!(snapshot_pos < screenshot_pos);
    }

    /// `destructive_hint` and `open_world_hint` default to `true` in the MCP spec, so a tool
    /// shipping without annotations reads to a host as destructive and open-world.
    #[test]
    fn annotations_classify_every_tool() {
        let mut problems: Vec<String> = Vec::new();

        for tool in GlassServer::tool_router().list_all() {
            let name = tool.name.to_string();

            let Some(ann) = tool.annotations.as_ref() else {
                problems.push(format!("  {name}: carries no annotations"));
                continue;
            };

            match ann.read_only_hint {
                None => problems.push(format!("  {name}: sets no read_only_hint")),
                Some(got) if got != (tool_effect(&name) == ToolEffect::ReadOnly) => {
                    problems.push(format!("  {name}: annotation and runtime effect differ"));
                }
                Some(_) => {}
            }

            if ann.open_world_hint.is_none() {
                problems.push(format!("  {name}: sets no open_world_hint"));
            }

            // The spec says destructive_hint and idempotent_hint are meaningful only when
            // read_only_hint is false.
            if ann.read_only_hint == Some(true) {
                for (field, set) in [
                    ("destructive_hint", ann.destructive_hint.is_some()),
                    ("idempotent_hint", ann.idempotent_hint.is_some()),
                ] {
                    if set {
                        problems.push(format!("  {name}: read-only, so {field} says nothing"));
                    }
                }
            } else if ann.destructive_hint.is_none() {
                problems.push(format!("  {name}: sets no destructive_hint"));
            }
        }

        assert!(
            problems.is_empty(),
            "every tool must carry annotations a host can act on:\n{}",
            problems.join("\n")
        );
    }

    #[test]
    fn instructions_lead_with_the_low_token_semantic_path() {
        // The cheap, text-only accessibility path must be presented as the default —
        // before pixels — so an agent reaches for it first. Guards against drift back
        // to a screenshot-first framing of the loop.
        let a11y = SERVER_INSTRUCTIONS
            .find("glass_a11y_snapshot")
            .expect("instructions mention glass_a11y_snapshot");
        let shot = SERVER_INSTRUCTIONS
            .find("glass_screenshot")
            .expect("instructions mention glass_screenshot");
        assert!(
            a11y < shot,
            "semantic addressing (glass_a11y_snapshot) must be introduced before \
             pixel capture (glass_screenshot)"
        );
    }

    #[test]
    fn descriptions_route_common_verification_claims() {
        let descriptions: BTreeMap<String, String> = GlassServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                (
                    tool.name.to_string(),
                    tool.description.unwrap_or_default().to_string(),
                )
            })
            .collect();

        let find = &descriptions["glass_find_elements"];
        for required in [
            "approximate, duplicated, or not yet identified",
            "fresh read",
            "defaults to 10",
            "capped at 20",
            "within` must match one semantic scope",
            "timeout_ms",
            "8 KiB",
        ] {
            assert!(
                find.contains(required),
                "glass_find_elements description missing {required:?}: {find}"
            );
        }

        let element = &descriptions["glass_wait_for_element"];
        assert!(element.contains("semantic transition completion"));
        assert!(element.contains("description"));
        assert!(element.contains("`value`"));
        assert!(element.contains("value_contains"));
        assert!(element.contains("disappears"));

        let region = &descriptions["glass_wait_for_region"];
        assert!(region.contains("pixel transition completion"));
        assert!(region.contains("glass_wait_stable"));

        let stable = &descriptions["glass_wait_stable"];
        assert!(stable.contains("visual quiescence"));
        assert!(stable.contains("not that an expected semantic state or pixel design was reached"));

        let snapshot = &descriptions["glass_a11y_snapshot"];
        assert!(snapshot.contains("current semantic state"));
        assert!(snapshot.contains("`value`"));

        let screenshot = &descriptions["glass_screenshot"];
        assert!(screenshot.contains("current visual evidence"));
        assert!(screenshot.contains("not semantic state or transition completion"));
    }

    /// The tool reference is the only user-facing list of glass's tools. Bind it to the
    /// registry so a tool added, removed, or renamed in code cannot silently diverge from
    /// the documentation.
    const TOOLS_MD: &str = include_str!("../../../docs/reference/tools.md");

    /// Tool names are keyed off level-3 headings wrapping the name in backticks. Prose also
    /// mentions a `glass_wait_for_*` family glob, which a looser scan would report as a tool.
    fn documented_tools() -> BTreeSet<String> {
        TOOLS_MD
            .lines()
            .filter_map(|line| line.strip_prefix("### `"))
            .filter_map(|rest| rest.strip_suffix('`'))
            .map(str::to_owned)
            .collect()
    }

    /// Strip `(...)` spans (depth-aware) so backtick *literals* inside type/default
    /// annotations — `` `0.1` ``, `` `{x,y,width,height}` `` — don't count as params.
    fn strip_parens(s: &str) -> String {
        let mut out = String::new();
        let mut depth = 0usize;
        for c in s.chars() {
            match c {
                '(' => depth += 1,
                ')' => depth = depth.saturating_sub(1),
                _ if depth == 0 => out.push(c),
                _ => {}
            }
        }
        out
    }

    /// Collect the contents of each `` `backtick` `` span, in order.
    fn backtick_tokens(s: &str) -> Vec<String> {
        let mut toks = Vec::new();
        let mut rest = s;
        while let Some(start) = rest.find('`') {
            let after = &rest[start + 1..];
            match after.find('`') {
                Some(end) => {
                    toks.push(after[..end].to_string());
                    rest = &after[end + 1..];
                }
                None => break,
            }
        }
        toks
    }

    /// Parameter names documented under a tool's ``### `tool` `` section: the backtick
    /// tokens at the head of each `- ` bullet (before the ` — ` description dash, with
    /// `(...)` stripped). Handles shared (`` `x`, `y` ``) and `/`-joined bullets.
    fn documented_params(tool: &str) -> BTreeSet<String> {
        let heading = format!("### `{tool}`");
        let mut lines = TOOLS_MD.lines();
        for line in lines.by_ref() {
            if line.trim_end() == heading {
                break;
            }
        }

        // Fold each bullet (a `- ` line plus its indented continuation lines) into one
        // logical string; a blank line or non-indented prose line ends the current bullet.
        let mut bullets: Vec<String> = Vec::new();
        let mut in_bullet = false;
        for line in lines.by_ref() {
            if line.starts_with("## ") || line.starts_with("### ") {
                break; // next section
            }
            if let Some(rest) = line.trim_end().strip_prefix("- ") {
                bullets.push(rest.trim().to_string());
                in_bullet = true;
            } else if line.trim().is_empty() {
                in_bullet = false;
            } else if in_bullet && line.starts_with(char::is_whitespace) {
                let last = bullets
                    .last_mut()
                    .expect("in_bullet implies a bullet exists");
                last.push(' ');
                last.push_str(line.trim());
            } else {
                in_bullet = false; // non-indented prose ends the list
            }
        }

        let mut params = BTreeSet::new();
        for bullet in bullets {
            if !bullet.starts_with('`') {
                continue; // only backtick-leading bullets document parameters
            }
            let head = bullet.split(" — ").next().unwrap_or(&bullet);
            for tok in backtick_tokens(&strip_parens(head)) {
                params.insert(tok);
            }
        }
        params
    }

    /// Top-level parameter names each tool advertises, straight from the registry's
    /// `input_schema.properties` — the same schema MCP shows agents. Correct across
    /// serde renames (`return_` → `"return"`); no hand-maintained list to drift.
    fn registered_params() -> BTreeMap<String, BTreeSet<String>> {
        GlassServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| {
                let params = tool
                    .input_schema
                    .get("properties")
                    .and_then(|v| v.as_object())
                    .map(|props| props.keys().cloned().collect::<BTreeSet<String>>())
                    .unwrap_or_default();
                (tool.name.into_owned(), params)
            })
            .collect()
    }

    #[test]
    fn find_elements_reference_documents_public_contract() {
        let heading = "### `glass_find_elements`";
        let start = TOOLS_MD.find(heading).expect("canonical reference heading");
        let snapshot = TOOLS_MD
            .find("### `glass_a11y_snapshot`")
            .expect("snapshot reference heading");
        assert!(
            start < snapshot,
            "find-elements reference must precede snapshot"
        );
        let section = &TOOLS_MD[start..snapshot];
        for required in [
            "`query` (string)",
            "`role` (string)",
            "`states` (array of string)",
            "`within` (object)",
            "`max_results` (integer, default 10, range 1 through 20)",
            "`max_nodes` (integer)",
            "`timeout_ms` (integer, default 0)",
            "Ranking",
            "context",
            "soft timeout",
            "ambiguous",
            "Secure values",
            "untrusted",
            "8 KiB",
        ] {
            assert!(
                section.contains(required),
                "glass_find_elements reference missing {required:?}"
            );
        }
    }

    #[test]
    fn tool_reference_documents_every_parameter() {
        let mut problems: Vec<String> = Vec::new();
        for (tool, schema_params) in &registered_params() {
            let documented = documented_params(tool);
            let undocumented: Vec<_> = schema_params.difference(&documented).collect();
            let phantom: Vec<_> = documented.difference(schema_params).collect();
            if !undocumented.is_empty() || !phantom.is_empty() {
                problems.push(format!(
                    "  {tool}: in schema but undocumented: {undocumented:?}; \
                     in docs but not a real param: {phantom:?}"
                ));
            }
        }
        assert!(
            problems.is_empty(),
            "docs/reference/tools.md parameter lists are out of sync with the #[tool] registry:\n{}",
            problems.join("\n")
        );
    }

    /// The `backend` param doc is the single place the backend list is spelled out for agents.
    /// Lock it to BACKENDS so a backend added in code can't ship undocumented — the bug that
    /// left `ios` out of glass_start's description text.
    #[test]
    fn backend_param_documents_every_backend() {
        let start = GlassServer::tool_router()
            .list_all()
            .into_iter()
            .find(|t| t.name == "glass_start")
            .expect("glass_start is registered");
        let doc = start
            .input_schema
            .get("properties")
            .and_then(|v| v.get("backend"))
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .expect("backend param has a description")
            .to_ascii_lowercase();
        let missing: Vec<&str> = crate::BACKENDS
            .iter()
            .copied()
            .filter(|b| !doc.contains(*b))
            .collect();
        assert!(
            missing.is_empty(),
            "the `backend` param doc must name every backend in BACKENDS; missing: {missing:?}"
        );
    }

    #[test]
    fn start_metadata_documents_the_android_run_tuple() {
        let start = GlassServer::tool_router()
            .list_all()
            .into_iter()
            .find(|t| t.name == "glass_start")
            .expect("glass_start is registered");
        let run_doc = start
            .input_schema
            .get("properties")
            .and_then(|v| v.get("run"))
            .and_then(|v| v.get("description"))
            .and_then(|v| v.as_str())
            .expect("run param has a description");

        assert!(run_doc.contains("/absolute/path/app.apk"), "{run_doc}");
        assert!(
            run_doc.contains("com.example.app/.MainActivity"),
            "{run_doc}"
        );
    }

    /// `glass_diff`, `glass_wait_stable`, and `glass_wait_for_region` must each advertise an
    /// `ignore` schema property typed as an array (`Option<Vec<RegionArgs>>` renders as
    /// `type: ["array","null"]` under schemars 1.x) — the shape an agent's MCP client reads to
    /// know it can pass ignore rects at all. Also pin the item shape itself: schemars renders
    /// `Vec<RegionArgs>`'s `items` as a `$ref` to the shared `RegionArgs` `$defs` entry, so this
    /// must resolve there — an `items` typed as a bare integer (or anything else) would still
    /// pass the array-only check above but reject every `{x,y,width,height}` rect a caller sends.
    #[test]
    fn ignore_param_is_an_array_on_diff_and_the_waits() {
        let router = GlassServer::tool_router();
        for tool_name in ["glass_diff", "glass_wait_stable", "glass_wait_for_region"] {
            let tool = router
                .list_all()
                .into_iter()
                .find(|t| t.name == tool_name)
                .unwrap_or_else(|| panic!("{tool_name} is registered"));
            let ignore_prop = tool
                .input_schema
                .get("properties")
                .and_then(|p| p.get("ignore"))
                .unwrap_or_else(|| panic!("{tool_name} has no `ignore` schema property"));
            let ignore_type = ignore_prop
                .get("type")
                .unwrap_or_else(|| panic!("{tool_name} has no `ignore` schema property"));
            let is_array = match ignore_type {
                serde_json::Value::String(s) => s == "array",
                serde_json::Value::Array(vs) => vs.iter().any(|v| v == "array"),
                _ => false,
            };
            assert!(
                is_array,
                "{tool_name}'s `ignore` property must be array-typed; got {ignore_type:?}"
            );
            let items_ref = ignore_prop
                .get("items")
                .and_then(|v| v.get("$ref"))
                .and_then(|v| v.as_str())
                .unwrap_or_else(|| {
                    panic!("{tool_name}'s `ignore.items` must be a $ref (got {ignore_prop:?})")
                });
            assert_eq!(
                items_ref, "#/$defs/RegionArgs",
                "{tool_name}'s `ignore.items` must reference the shared RegionArgs $def; got {items_ref}"
            );
        }
    }

    #[test]
    fn tool_reference_documents_exactly_the_registry() {
        let documented = documented_tools();
        let registered = registered_tools();

        let undocumented: Vec<_> = registered.difference(&documented).collect();
        let phantom: Vec<_> = documented.difference(&registered).collect();

        assert!(
            undocumented.is_empty() && phantom.is_empty(),
            "docs/reference/tools.md is out of sync with the #[tool] registry\n  \
             registered but undocumented: {undocumented:?}\n  \
             documented but not registered: {phantom:?}"
        );
    }

    /// `glass_capabilities`'s per-operation tool lists (`OPERATION_TOOLS`, single-sourced in
    /// `capabilities.rs`) must each name a tool actually in the registry — otherwise the
    /// reported `tools` array would point an agent at a tool that doesn't exist. `tool_router`
    /// is only reachable from this module (see `registered_tools` above), so the test lives
    /// here rather than alongside the table in `capabilities.rs`.
    #[test]
    fn every_mapped_tool_is_a_registered_tool() {
        let registered = registered_tools();
        for (op, tools) in crate::capabilities::OPERATION_TOOLS {
            for t in *tools {
                assert!(
                    registered.contains(*t),
                    "operation {op:?} maps to unregistered tool {t:?}"
                );
            }
        }
    }
}
