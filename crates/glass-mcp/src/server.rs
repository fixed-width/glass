//! rmcp handler exposing the tool-logic layer over MCP. Thin wrappers only.

use std::sync::Arc;

use base64::Engine;
use glass_core::Glass;
use rmcp::handler::server::router::tool::ToolRouter;
use rmcp::handler::server::wrapper::Parameters;
use rmcp::model::{CallToolResult, Content, ServerCapabilities, ServerInfo};
use rmcp::{tool, tool_handler, tool_router, ErrorData as McpError, ServerHandler};
use tokio::sync::Mutex;

use crate::audit::AuditReport;
use crate::params::*;
use crate::tools::{self, OutContent, ToolOutput, ToolResult};

/// A synchronous tool body plus where to send its result — run on the dedicated
/// `glass-platform` thread (see [`GlassServer::new`]).
type Job = (
    Box<dyn FnOnce(&mut Glass) -> ToolResult + Send>,
    tokio::sync::oneshot::Sender<ToolResult>,
);

#[derive(Clone)]
pub struct GlassServer {
    glass: Arc<Mutex<Glass>>,
    /// Hands tool bodies to the long-lived `glass-platform` thread.
    jobs: std::sync::mpsc::Sender<Job>,
    /// Audit-log posture, carried for `glass_doctor` display.
    report: AuditReport,
    tool_router: ToolRouter<GlassServer>,
}

fn to_call_result(out: ToolOutput) -> CallToolResult {
    let contents = out
        .0
        .into_iter()
        .map(|c| match c {
            OutContent::Text(t) => Content::text(t),
            OutContent::Image(bytes) => {
                let b64 = base64::engine::general_purpose::STANDARD.encode(bytes);
                Content::image(b64, "image/webp")
            }
        })
        .collect();
    CallToolResult::success(contents)
}

/// Map a tool-logic result into an MCP call result. An `Err` becomes an MCP
/// *error* result (`is_error == true`) carrying the message, so a failed backend
/// op is surfaced to the agent as a failure rather than read as a successful
/// call — the "no silent fallback" invariant at the protocol boundary. Kept pure
/// (no async / no lock) so this contract is unit-testable.
fn map_tool_result(result: ToolResult) -> CallToolResult {
    match result {
        Ok(out) => to_call_result(out),
        Err(msg) => CallToolResult::error(vec![Content::text(msg)]),
    }
}

#[tool_router]
impl GlassServer {
    pub fn new(glass: Glass, report: AuditReport) -> Self {
        let glass = Arc::new(Mutex::new(glass));
        let (jobs, rx) = std::sync::mpsc::channel::<Job>();
        // One long-lived OS thread runs EVERY tool body. Tool bodies that spawn a
        // long-lived child — a sandboxed app under `bwrap --die-with-parent`, which SIGKILLs
        // the sandbox on the death of its parent *thread* (PR_SET_PDEATHSIG is thread-scoped
        // on Linux) — must be parented to a thread that lives for the whole process, NOT an
        // ephemeral tokio blocking-pool thread. A pool thread, recycled after the launch
        // call returns, would trigger that kill and the app would vanish right after launch.
        // Running here also keeps blocking build/launch/wait work off the async executor and
        // serializes tools (glass has one active session).
        let worker_glass = glass.clone();
        std::thread::Builder::new()
            .name("glass-platform".into())
            .spawn(move || {
                while let Ok((job, reply)) = rx.recv() {
                    let mut g = worker_glass.blocking_lock();
                    // A panicking tool becomes a loud error AND the thread survives — so it
                    // keeps serving calls and keeps parenting any still-running sandbox.
                    let result =
                        std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| job(&mut g)))
                            .unwrap_or_else(|_| Err("tool handler panicked".to_string()));
                    let _ = reply.send(result);
                }
            })
            .expect("spawn glass-platform thread");
        Self {
            glass,
            jobs,
            report,
            tool_router: Self::tool_router(),
        }
    }

    /// A clone of the shared session registry, for the process-exit teardown path in
    /// `main`. Taken before `serve()` consumes the server.
    pub fn sessions(&self) -> Arc<Mutex<Glass>> {
        self.glass.clone()
    }

    async fn run<F>(&self, f: F) -> Result<CallToolResult, McpError>
    where
        F: FnOnce(&mut Glass) -> ToolResult + Send + 'static,
    {
        // Hand the (synchronous, possibly slow) tool body to the dedicated glass-platform
        // thread and await its result. That thread — not an ephemeral blocking-pool thread —
        // is the parent of any process the body spawns, so a sandboxed app's
        // `--die-with-parent` only fires when glass itself exits. It also keeps blocking work
        // off the async executor and serializes tools (glass has one session). A handler
        // panic comes back as a loud error, never an unanswered request.
        let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
        if self.jobs.send((Box::new(f), reply_tx)).is_err() {
            return Ok(map_tool_result(Err(
                "glass-platform thread is gone".to_string()
            )));
        }
        let outcome = reply_rx
            .await
            .unwrap_or_else(|_| Err("glass-platform thread dropped the job".to_string()));
        Ok(map_tool_result(outcome))
    }

    #[tool(
        description = "Build, launch, and locate a native GUI app; returns its window geometry. Optional `backend`: \"x11\" (headless Xvfb) or \"wayland\" (headless sway) on Linux, or \"windows\" on a Windows host, or \"macos\" on a macOS host, or \"android\" for an AVD emulator on any host; defaults to the host backend (windows on Windows, macos on macOS, else x11). Optional `window_hint` ({ title?, class? }) picks the right window when several appear, or locates one the launched process hands off to an unrelated process (some packaged Windows apps)."
    )]
    async fn glass_start(
        &self,
        Parameters(a): Parameters<StartArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::start(g, &a)).await
    }

    #[tool(description = "Stop the running app and end the session.")]
    async fn glass_stop(&self) -> Result<CallToolResult, McpError> {
        self.run(tools::stop).await
    }

    #[tool(
        description = "Focus/resize/move the window or read its geometry. op: focus|resize|move|geometry."
    )]
    async fn glass_window(
        &self,
        Parameters(a): Parameters<WindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::window(g, &a)).await
    }

    #[tool(
        description = "Capture the app window (or an optional window-relative `region`) as a screenshot (lossless WebP image). A capture reaching off the display edge is clipped to the on-screen portion — the returned `width`/`height` are the actual captured size, so a frame smaller than the window/region means it was clipped; only a fully off-screen surface errors."
    )]
    async fn glass_screenshot(
        &self,
        Parameters(a): Parameters<ScreenshotArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::screenshot(g, &a)).await
    }

    #[tool(
        description = "Wait until the window stops changing, then return the settled frame. Optional `stability_region` watches only that sub-rectangle for settling (ignore unrelated motion); optional `region` crops the returned frame. Set `include_image: false` for a text-only `{settled,width,height}` result with no image (`region` ignored) — cheap before a text `glass_diff`."
    )]
    async fn glass_wait_stable(
        &self,
        Parameters(a): Parameters<WaitStableArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::wait_stable(g, &a)).await
    }

    #[tool(
        description = "Click at window-relative coordinates. button: left|right|middle; count for multi-click. Optional modifiers held during the action, e.g. [\"ctrl\"] or [\"ctrl\",\"shift\"] for multi/range-select."
    )]
    async fn glass_click(
        &self,
        Parameters(a): Parameters<ClickArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::click(g, &a)).await
    }

    #[tool(description = "Move the pointer to window-relative coordinates.")]
    async fn glass_move(
        &self,
        Parameters(a): Parameters<MoveArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::mouse_move(g, &a)).await
    }

    #[tool(
        description = "Drag with a button held from (x1,y1) to (x2,y2) — window-relative coordinates. Optional modifiers held during the action, e.g. [\"ctrl\"] or [\"ctrl\",\"shift\"] for multi/range-select."
    )]
    async fn glass_drag(
        &self,
        Parameters(a): Parameters<DragArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::drag(g, &a)).await
    }

    #[tool(
        description = "Scroll at window-relative coordinates by (dx,dy) wheel steps. Optional modifiers held during the action, e.g. [\"ctrl\"] or [\"ctrl\",\"shift\"] for multi/range-select."
    )]
    async fn glass_scroll(
        &self,
        Parameters(a): Parameters<ScrollArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::scroll(g, &a)).await
    }

    #[tool(
        description = "Perform a multi-touch gesture: 2–10 pointers, each a straight from→to \
                       segment in window-relative px, all down together at t=0 and up at \
                       duration_ms. Pinch = two pointers toward/apart; rotate = two on an arc; \
                       two-finger swipe = two parallel segments; a from==to pointer is held. \
                       Android backend only (needs the on-device agent)."
    )]
    async fn glass_gesture(
        &self,
        Parameters(a): Parameters<GestureArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::gesture(g, &a)).await
    }

    #[tool(description = "Type a string of text into the focused window.")]
    async fn glass_type(
        &self,
        Parameters(a): Parameters<TypeArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::type_text(g, &a)).await
    }

    #[tool(description = "Press a key chord like 'ctrl+s', 'Return', 'alt+F4'.")]
    async fn glass_key(
        &self,
        Parameters(a): Parameters<KeyArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::key(g, &a)).await
    }

    #[tool(
        description = "Read the clipboard as text (\"\" if empty). Acts on the app's clipboard — \
                       isolated from your real clipboard on the private Xvfb/sway backends and on \
                       a contained Windows app (a private boxed clipboard). Also the cheap \
                       text-extraction path: glass_do ctrl+a then ctrl+c, then read here (beats \
                       OCR for selectable text). On a contained macOS app (sandbox: Default/Strict), \
                       an app not built with Apple's hardened runtime (e.g. a debug or unsigned \
                       build) is transparently redirected to a private pasteboard glass shares — \
                       isolated from your real clipboard and fully working; an app that runs under \
                       hardened runtime (App Store / notarized) can't be redirected, so this returns \
                       Unsupported. Uncontained (sandbox: off) reads your REAL system clipboard."
    )]
    async fn glass_clipboard_get(&self) -> Result<CallToolResult, McpError> {
        self.run(tools::clipboard_get).await
    }

    #[tool(
        description = "Write text to the clipboard so the app can paste it. Isolated from your \
                       real clipboard on the private Xvfb/sway backends and on a contained Windows \
                       app (a private boxed clipboard); only shared-desktop modes (GLASS_DISPLAY=:0, \
                       or the Windows backend with sandbox=off) write your real clipboard — \
                       snapshot with glass_clipboard_get first if needed. On a contained macOS app \
                       (sandbox: Default/Strict), an app not built with Apple's hardened runtime \
                       (e.g. a debug or unsigned build) is transparently redirected to a private \
                       pasteboard glass shares — isolated from your real clipboard and fully \
                       working; an app that runs under hardened runtime (App Store / notarized) \
                       can't be redirected, so this returns Unsupported. Uncontained (sandbox: off) \
                       writes your REAL system clipboard — \
                       snapshot with glass_clipboard_get first if you need to preserve it."
    )]
    async fn glass_clipboard_set(
        &self,
        Parameters(a): Parameters<ClipboardSetArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::clipboard_set(g, &a)).await
    }

    #[tool(description = "Save the current frame as a named visual baseline.")]
    async fn glass_baseline_save(
        &self,
        Parameters(a): Parameters<BaselineSaveArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::baseline_save(g, &a)).await
    }

    #[tool(
        description = "Diff the current frame against a named baseline; returns change stats + bbox. Set `include_image: true` to also return the current frame cropped to the changed region (omitted when nothing changed)."
    )]
    async fn glass_diff(
        &self,
        Parameters(a): Parameters<DiffArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::diff(g, &a)).await
    }

    #[tool(
        description = "Diagnose the glass environment (Xvfb, sway, software GL) and report \
                       per-check status + how to fix anything missing. Use this to self-diagnose \
                       a glass_start failure. Optional `deep`: also spawn+tear-down the default \
                       backend's headless display to verify it actually starts."
    )]
    async fn glass_doctor(
        &self,
        Parameters(a): Parameters<DoctorArgs>,
    ) -> Result<CallToolResult, McpError> {
        let backend = crate::default_backend(std::env::var("GLASS_BACKEND").ok().as_deref());
        let deep = a.deep.unwrap_or(false);
        // The probes are blocking (and `deep` spawns a display), so keep them off the
        // stdio reactor thread.
        let report = self.report.clone();
        let diag =
            tokio::task::spawn_blocking(move || crate::doctor::diagnose_with_audit(deep, &report))
                .await
                .expect("doctor task panicked");
        Ok(to_call_result(ToolOutput::text(diag.render_text(backend))))
    }

    #[tool(
        description = "List the app's top-level windows: id, title, class, geometry, and which is active. Returns a JSON array. Window ids are not stable across calls — re-list after windows open/close instead of caching ids."
    )]
    async fn glass_list_windows(&self) -> Result<CallToolResult, McpError> {
        self.run(tools::list_windows).await
    }

    #[tool(
        description = "Make a window active by id (from glass_list_windows). Subsequent screenshot/click/type/window ops target it; coordinates are relative to it."
    )]
    async fn glass_select_window(
        &self,
        Parameters(a): Parameters<SelectWindowArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::select_window(g, &a)).await
    }

    #[tool(
        description = "Capture the active window's accessibility tree (semantic \
                       elements: role, name, window-relative bounds) as compact text — \
                       deterministic, low-token element addressing alongside screenshots. \
                       Each line is `#<id> <Role> \"<name>\" (x,y wxh) [states]`; pass an \
                       #id to glass_click_element. Errors if the backend or app exposes no \
                       accessibility tree (e.g. a canvas/black-box app) — fall back to \
                       glass_screenshot then."
    )]
    async fn glass_a11y_snapshot(&self) -> Result<CallToolResult, McpError> {
        self.run(tools::a11y_snapshot).await
    }

    #[tool(
        description = "Click an element by its #id from glass_a11y_snapshot (clicks the \
                       center of its bounds, via the normal click path). If the element actually \
                       renders in a popover owned by a different window than the active one \
                       (e.g. an open dropdown's option row), the click is automatically routed \
                       into that popover window and the previously-active window is restored \
                       afterward. Ids are only valid within the latest snapshot — re-run \
                       glass_a11y_snapshot if the UI changed. Optional `return`: \"snapshot\" \
                       folds a fresh a11y tree into the result (and refreshes the snapshot \
                       cache); \"settle\" waits for the UI to stop changing (text-only); omit or \
                       \"none\" for no observe (default)."
    )]
    async fn glass_click_element(
        &self,
        Parameters(a): Parameters<ClickElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::click_element(g, &a)).await
    }

    #[tool(
        description = "Set an editable element's value directly via accessibility (instant, no \
                       keystrokes) — pick the element's #id from glass_a11y_snapshot. Errors if the \
                       element isn't editable, if it changed since the snapshot (re-snapshot), or if \
                       the app exposes no accessibility tree. \
                       Optional `return`: \"snapshot\" folds a fresh a11y tree into the result \
                       (and refreshes the snapshot cache); \"settle\" waits for the UI to stop \
                       changing (text-only); omit or \"none\" for no observe (default)."
    )]
    async fn glass_set_value(
        &self,
        Parameters(a): Parameters<SetValueArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::set_value(g, &a)).await
    }

    #[tool(
        description = "Screenshot of the active window with a numbered box drawn on each \
                       interactable element (Set-of-Mark) — returns the annotated image plus a \
                       text legend (`#<id> <Role> \"<name>\"`). Pick an element visually, then \
                       click it with glass_click_element using its #id (same ids as \
                       glass_a11y_snapshot). Chips sit just outside each element so small icon \
                       buttons stay visible. The box is only as precise as the toolkit's \
                       accessibility geometry (it can drift ~10-20px), but the #id and the click \
                       are exact (click_element targets the element's center). Errors if no \
                       accessibility tree is available — use glass_screenshot then."
    )]
    async fn glass_a11y_marks(&self) -> Result<CallToolResult, McpError> {
        self.run(tools::a11y_marks).await
    }

    #[tool(description = "Read captured stdout/stderr log lines with a resumable cursor.")]
    async fn glass_logs(
        &self,
        Parameters(a): Parameters<LogsArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::logs(g, &a)).await
    }

    #[tool(
        description = "Block until a UI element reaches a precise state, then return it as text \
                       (no image). Select by `name` (accessible-name substring) and/or `role` \
                       (e.g. \"Button\"); `condition` (default appears): appears|disappears|enabled|\
                       disabled|checked|unchecked|selected|unselected|expanded|collapsed|focused|\
                       visible|hidden; optional `value_contains`. Returns \
                       {matched,elapsed_ms,element{id,role,name,bounds,states}} — the id is usable \
                       with glass_click_element. On timeout returns {matched:false}. Errors if the \
                       app exposes no accessibility tree. Collapses screenshot poll-loops into one call."
    )]
    async fn glass_wait_for_element(
        &self,
        Parameters(a): Parameters<WaitForElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::wait_for_element(g, &a)).await
    }

    #[tool(
        description = "Scroll a container until an accessibility element becomes visible, then \
                       return it (text-only, no image). For a virtualized list only on-screen \
                       rows exist in the a11y tree, so an off-screen row can't be clicked until \
                       scrolled to; this collapses the scroll+snapshot loop into one call. Select \
                       by `name` (accessible-name substring) and/or `role` (e.g. \"ListItem\"); \
                       optional `value_contains`. `direction` \"down\" (default) or \"up\" — it \
                       sweeps that way to the end, then reverses to cover the other end. Optional \
                       `x`,`y` aim the wheel at a specific container (default: window center); \
                       `step` sets wheel notches per move (default 3). Returns \
                       {matched,elapsed_ms,element{id,role,name,bounds,states},scrolled{steps,reversed}} \
                       — the id is usable with glass_click_element. Returns {matched:false} if the \
                       element never appears after sweeping both ends or `timeout_ms` (default \
                       20000). Errors if the app exposes no accessibility tree."
    )]
    async fn glass_scroll_to_element(
        &self,
        Parameters(a): Parameters<ScrollToElementArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::scroll_to_element(g, &a)).await
    }

    #[tool(
        description = "Block until a visual region changes (diverges from a reference) or matches \
                       (converges to a saved baseline), then return text metrics (no image unless \
                       `include_image:true`). `until`: \"changes\" (default) or \"matches\" (needs \
                       `baseline`); optional window-relative `region`; `mode` perceptual|exact with \
                       `threshold`/`tolerance`. Returns {matched,changed_pct,bbox,elapsed_ms}. Use \
                       \"matches\" to confirm the UI reached an approved design without spending \
                       vision tokens."
    )]
    async fn glass_wait_for_region(
        &self,
        Parameters(a): Parameters<WaitForRegionArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::wait_for_region(g, &a)).await
    }

    #[tool(
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
        self.run(move |g| tools::wait_for_log(g, &a)).await
    }

    #[tool(
        description = "Run an ordered sequence of input actions in ONE call (collapsing per-action \
                       round-trips), then optionally observe. `actions` is a list of \
                       {\"action\":\"click|move|drag|scroll|type|key|settle\", …same fields as the \
                       matching tool}; `settle` waits for the screen to stop changing between steps. \
                       Optional `then` runs after all actions succeed: {settle?, diff?, screenshot?} \
                       (text-only unless screenshot/diff image). Fails fast: if an action errors it \
                       reports which index failed and how many ran. Use for KNOWN sequences (login, \
                       form-fill, menu→item); if you must see a result to choose the next action, \
                       don't batch that part."
    )]
    async fn glass_do(
        &self,
        Parameters(a): Parameters<DoArgs>,
    ) -> Result<CallToolResult, McpError> {
        self.run(move |g| tools::do_actions(g, &a)).await
    }
}

#[tool_handler(router = self.tool_router)]
impl ServerHandler for GlassServer {
    fn get_info(&self) -> ServerInfo {
        ServerInfo::new(ServerCapabilities::builder().enable_tools().build()).with_instructions(
            "glass gives you a build → see → interact → debug loop over a real native GUI \
                 app — no app integration needed. One active session; tools target it implicitly; \
                 choose a backend (x11 or wayland) at glass_start.\n\n\
                 Loop: glass_start launches the app and captures its logs; glass_screenshot to see \
                 it; glass_click / glass_type / glass_key / glass_scroll / glass_drag (and \
                 glass_gesture for android multi-touch) to interact \
                 (coordinates are WINDOW-RELATIVE — 0,0 is the app window's top-left); \
                 glass_wait_stable to let a render settle before you look or compare; glass_logs \
                 for the app's stdout/stderr.\n\n\
                 Verify cheaply on the CPU: glass_baseline_save a good frame, act, then \
                 glass_wait_stable with include_image=false and glass_diff, which returns \
                 changed_pct and a bbox as TEXT (no image). Only call glass_diff with \
                 include_image=true (a cropped image of the changed region) when changed_pct shows \
                 something moved — don't screenshot to check every step.\n\n\
                 Wait for a specific condition instead of polling with screenshots: \
                 glass_wait_for_element (until a UI element reaches a state, e.g. Save becomes \
                 enabled — returns the element id for glass_click_element), glass_wait_for_region \
                 (until a region changes, or matches a saved baseline), glass_wait_for_log (until \
                 a log line appears). All return text only and time out softly with \
                 {matched:false} — branch on that rather than retrying blindly.\n\n\
                 Batch a known input sequence into one call with glass_do (actions: click/type/key/\
                 move/drag/scroll/settle), with an optional text-first `then` observe \
                 (settle/diff/screenshot) — fewer round-trips, and it fails fast naming the action \
                 that broke.\n\n\
                 Semantic addressing (when the app exposes an accessibility tree): \
                 glass_a11y_snapshot returns the elements as text (#id, role, name, \
                 window-relative bounds); glass_click_element clicks one by #id \
                 and glass_set_value writes an editable element's value by #id. Prefer \
                 this over pixel-hunting when it works; it errors for canvas/black-box \
                 apps, so fall back to screenshots then.\n\n\
                 Multiple windows: glass_list_windows and glass_select_window. Errors are real — a \
                 failed capture or input returns a message, never a blank or stale frame; fix the \
                 cause instead of retrying blindly.",
        )
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeSet;

    fn first_text(r: &CallToolResult) -> String {
        r.content[0].as_text().expect("text content").text.clone()
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
        let r = map_tool_result(Ok(ToolOutput::text("done")));
        assert_eq!(
            r.is_error,
            Some(false),
            "an Ok must surface as a success result"
        );
        assert!(first_text(&r).contains("done"), "got {:?}", first_text(&r));
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

    fn registered_tools() -> BTreeSet<String> {
        GlassServer::tool_router()
            .list_all()
            .into_iter()
            .map(|tool| tool.name.into_owned())
            .collect()
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
}
