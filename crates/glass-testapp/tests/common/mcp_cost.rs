//! Verification-loop cost benchmark. Drives one fixed task two ways against
//! `glass-fixture-egui` over the real `glass-mcp` HTTP path and measures what each way puts
//! in an agent's context: round-trips, request/response bytes, and image dimensions. The
//! point is the number, published as reproducible primitives — a reader applies their own
//! model's image-token formula (see docs/how-to/verification-cost.md).
//!
//! `#[ignore]`d and run via `scripts/verification-cost.sh` (needs Xvfb + an AT-SPI bus).
//!
//! This module is `pub mod mcp_cost;` in `common/mod.rs`, so it is compiled into every test
//! binary that declares `mod common;` (not only `verification_cost.rs`, the only one that
//! calls it) — which means the `#[cfg(test)]` unit tests below run under `cargo test
//! --workspace` regardless.

use std::path::PathBuf;
use std::time::Duration;

use glass_mcp::serve::config::ServeConfig;
use rmcp::model::CallToolRequestParams;
use rmcp::service::RunningService;
use rmcp::transport::streamable_http_client::StreamableHttpClientTransportConfig;
use rmcp::transport::StreamableHttpClientTransport;
use rmcp::{Peer, RoleClient, ServiceExt};
use serde_json::{json, Value};

/// Repo root: `crates/glass-testapp` is two levels below it.
fn repo_root() -> PathBuf {
    PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .ancestors()
        .nth(2)
        .expect("repo root is two levels above crates/glass-testapp")
        .to_path_buf()
}

/// (build command, absolute run path, cwd) for the workspace-excluded egui fixture.
pub fn fixture_run_spec() -> (String, String, String) {
    let root = repo_root();
    let exe = root.join("crates/glass-fixture-egui/target/release/glass-fixture-egui");
    (
        "cargo build --release --manifest-path crates/glass-fixture-egui/Cargo.toml".to_string(),
        exe.to_string_lossy().into_owned(),
        root.to_string_lossy().into_owned(),
    )
}

/// Boot an in-process glass-mcp HTTP server on an ephemeral loopback port; return a client.
/// Mirrors `common::mcp_ignore`. Returns the `RunningService`, not a bare `Peer`: dropping the
/// service cancels the connection (its `DropGuard`), so the caller must hold this until done
/// and call `.cancel()` itself — a bare `Peer` clone would silently outlive a live connection.
pub async fn boot_mcp() -> RunningService<RoleClient, ()> {
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("bind ephemeral loopback port");
    let addr = listener.local_addr().unwrap();
    let glass = glass_mcp::boot(None);
    let report = glass_mcp::audit::report_from_config(None, |_| None);
    tokio::spawn(async move {
        let cfg = ServeConfig {
            addr,
            token: Some("vcost".into()),
        };
        let _ = glass_mcp::serve::run_on(listener, cfg, glass, report).await;
    });
    tokio::time::sleep(Duration::from_millis(50)).await;
    let mut tcfg = StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}/"));
    tcfg = tcfg.auth_header("vcost".to_string());
    ().serve(StreamableHttpClientTransport::from_config(tcfg))
        .await
        .expect("initialize over http")
}

/// One tool call: `Ok` with the parsed `result` object and the concatenation of all text
/// blocks, `Err` with that same text concatenation if the tool reported an error. A
/// transport-level failure (the call never got a response at all) still panics — that's an
/// infrastructure fault, not a condition any caller here retries on.
async fn try_call(
    client: &Peer<RoleClient>,
    tool: &str,
    args: Value,
) -> Result<(Value, String), String> {
    let arguments = args
        .as_object()
        .expect("args must be a JSON object")
        .clone();
    let res = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("{tool} call failed: {e}"));
    let mut all_text = String::new();
    let mut result = Value::Null;
    for c in &res.content {
        if let Some(t) = c.as_text() {
            all_text.push_str(&t.text);
            all_text.push('\n');
            if let Ok(v) = serde_json::from_str::<Value>(&t.text) {
                if v.get("ok").is_some() && v.get("result").is_some() {
                    result = v["result"].clone();
                }
            }
        }
    }
    if res.is_error == Some(true) {
        Err(all_text)
    } else {
        Ok((result, all_text))
    }
}

/// Plain call (unmetered): assert no error, return the parsed `result` object and the
/// concatenation of all text blocks. Used for setup/probe steps that must not count.
pub async fn call(client: &Peer<RoleClient>, tool: &str, args: Value) -> (Value, String) {
    try_call(client, tool, args)
        .await
        .unwrap_or_else(|text| panic!("{tool} errored: {text}"))
}

/// `glass_start` the fixture with a private AT-SPI bus on the x11 backend.
pub async fn start_fixture(client: &Peer<RoleClient>) {
    let (build, run, cwd) = fixture_run_spec();
    call(
        client,
        "glass_start",
        json!({
            "build": build,
            "run": [run],
            "cwd": cwd,
            "backend": "x11",
            "a11y": true,
            "timeout_ms": 120_000, // first egui build is slow
        }),
    )
    .await;
}

/// How long `a11y_outline` retries a snapshot that reports the launched app isn't publishing
/// an accessibility tree yet.
pub(crate) const A11Y_SETTLE_TIMEOUT: Duration = Duration::from_secs(5);
pub(crate) const A11Y_POLL_INTERVAL: Duration = Duration::from_millis(200);

/// Snapshot the a11y tree; return the full text (envelope + untrusted outline). Retries for
/// up to `A11Y_SETTLE_TIMEOUT`: immediately after `glass_start` returns, the launched app's
/// toolkit (accesskit, for the egui fixture) may not have finished registering its AT-SPI
/// subtree yet, so a snapshot taken right away can transiently report the tree as
/// unavailable even though the private a11y bus itself is up and reachable — the same
/// startup race `glass-a11y-linux`'s integration test settles with a fixed sleep before its
/// first snapshot.
pub async fn a11y_outline(client: &Peer<RoleClient>) -> String {
    let deadline = tokio::time::Instant::now() + A11Y_SETTLE_TIMEOUT;
    loop {
        match try_call(client, "glass_a11y_snapshot", json!({})).await {
            Ok((_r, text)) => return text,
            Err(text) if tokio::time::Instant::now() < deadline => {
                let _ = text; // transient during app startup; keep polling
                tokio::time::sleep(A11Y_POLL_INTERVAL).await;
            }
            Err(text) => panic!("glass_a11y_snapshot errored: {text}"),
        }
    }
}

/// First `#id` on an outline line whose role token (Debug-formatted, e.g. `Slider`,
/// `TextField`) contains `role_substr` (case-insensitive), excluding label/static roles.
/// Outline line shape (see glass-core AxTree::to_outline):
/// `  #12 Button "Apply" (10,240 80x30) [focusable]`
pub fn find_by_role(outline: &str, role_substr: &str) -> Option<u32> {
    let want = role_substr.to_ascii_lowercase();
    for line in outline.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix('#') else {
            continue;
        };
        let mut it = rest.splitn(2, ' ');
        let Some(Ok(id)) = it.next().map(str::parse::<u32>) else {
            continue;
        };
        let after = it.next().unwrap_or("");
        let role = after
            .split([' ', '('])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if role.contains("label") || role.contains("static") {
            continue;
        }
        if role.contains(&want) {
            return Some(id);
        }
    }
    None
}

/// First `#id` of a Button whose quoted name equals `name`.
pub fn find_named_button(outline: &str, name: &str) -> Option<u32> {
    let needle = format!("{name:?}"); // includes the quotes, e.g. "Apply"
    for line in outline.lines() {
        let l = line.trim_start();
        let Some(rest) = l.strip_prefix('#') else {
            continue;
        };
        let mut it = rest.splitn(2, ' ');
        let Some(Ok(id)) = it.next().map(str::parse::<u32>) else {
            continue;
        };
        let after = it.next().unwrap_or("");
        let role = after
            .split([' ', '('])
            .next()
            .unwrap_or("")
            .to_ascii_lowercase();
        if role.contains("button") && after.contains(&needle) {
            return Some(id);
        }
    }
    None
}

#[derive(Debug, Default, Clone)]
pub struct CallMeter {
    pub round_trips: u64,
    pub request_bytes: u64,
    pub text_bytes: u64,
    pub image_count: u64,
    pub image_b64_bytes: u64,
    pub image_dims: Vec<(u32, u32)>,
}

impl CallMeter {
    /// Count the request: one round-trip, plus the byte length of the wire-shaped
    /// `{tool, arguments}` JSON an agent would send.
    pub fn record_request(&mut self, tool: &str, args: &Value) {
        self.round_trips += 1;
        let wire = json!({ "tool": tool, "arguments": args });
        self.request_bytes += wire.to_string().len() as u64;
    }

    /// Count every content block. Returns the parsed envelope `result`, all text
    /// concatenated, and the image dims (from the envelope's width/height) when an image
    /// block is present.
    pub fn record_response(
        &mut self,
        content: &[rmcp::model::Content],
    ) -> (Value, String, Option<(u32, u32)>) {
        let mut all_text = String::new();
        let mut result = Value::Null;
        let mut has_image = false;
        for c in content {
            if let Some(t) = c.as_text() {
                self.text_bytes += t.text.len() as u64;
                all_text.push_str(&t.text);
                all_text.push('\n');
                if let Ok(v) = serde_json::from_str::<Value>(&t.text) {
                    if v.get("ok").is_some() && v.get("result").is_some() {
                        result = v["result"].clone();
                    }
                }
            } else if let Some(img) = c.as_image() {
                self.image_count += 1;
                self.image_b64_bytes += img.data.len() as u64;
                has_image = true;
            }
        }
        let dims = if has_image {
            match (
                result.get("width").and_then(Value::as_u64),
                result.get("height").and_then(Value::as_u64),
            ) {
                (Some(w), Some(h)) => {
                    let d = (w as u32, h as u32);
                    self.image_dims.push(d);
                    Some(d)
                }
                _ => None,
            }
        } else {
            None
        };
        // The harness assumes at most one image block per response: `glass_screenshot` emits
        // exactly one, and `ToolOutput::image_result` takes a single image, so `image_dims`
        // (pushed at most once per call above) can never outgrow `image_count`.
        debug_assert!(
            self.image_dims.len() as u64 <= self.image_count,
            "image_dims must not exceed image_count"
        );
        (result, all_text, dims)
    }
}

/// Metered call: record request, call over MCP, record every response block. Panics on a
/// tool error (a broken step makes the measurement meaningless).
pub async fn metered_call(
    client: &Peer<RoleClient>,
    meter: &mut CallMeter,
    tool: &str,
    args: Value,
) -> (Value, String) {
    meter.record_request(tool, &args);
    let arguments = args
        .as_object()
        .expect("args must be a JSON object")
        .clone();
    let res = client
        .call_tool(CallToolRequestParams::new(tool.to_string()).with_arguments(arguments))
        .await
        .unwrap_or_else(|e| panic!("{tool} call failed: {e}"));
    assert_ne!(res.is_error, Some(true), "{tool} errored: {res:?}");
    let (result, all_text, _dims) = meter.record_response(&res.content);
    (result, all_text)
}

#[derive(Debug, Clone)]
pub struct ArmReport {
    pub name: String,
    pub round_trips: u64,
    pub request_bytes: u64,
    pub text_bytes: u64,
    pub image_count: u64,
    pub image_b64_bytes: u64,
    pub image_dims: Vec<(u32, u32)>,
}

impl ArmReport {
    pub fn from_meter(name: &str, m: &CallMeter) -> ArmReport {
        ArmReport {
            name: name.to_string(),
            round_trips: m.round_trips,
            request_bytes: m.request_bytes,
            text_bytes: m.text_bytes,
            image_count: m.image_count,
            image_b64_bytes: m.image_b64_bytes,
            image_dims: m.image_dims.clone(),
        }
    }

    /// Approximate text-token count, derived from `text_bytes` rather than stored, so it can
    /// never drift out of sync with the byte count it's computed from.
    pub fn approx_text_tokens(&self) -> u64 {
        self.text_bytes.div_ceil(4)
    }

    pub fn to_json(&self) -> Value {
        json!({
            "arm": self.name,
            "round_trips": self.round_trips,
            "request_bytes": self.request_bytes,
            "text_bytes": self.text_bytes,
            "approx_text_tokens": self.approx_text_tokens(),
            "image_count": self.image_count,
            "image_b64_bytes": self.image_b64_bytes,
            "image_dims": self.image_dims.iter().map(|(w, h)| json!([w, h])).collect::<Vec<_>>(),
        })
    }
}

impl std::fmt::Display for ArmReport {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:<14} round_trips={} req_bytes={} text_bytes={} ~text_tokens={} images={} img_dims={:?}",
            self.name, self.round_trips, self.request_bytes, self.text_bytes,
            self.approx_text_tokens(), self.image_count, self.image_dims
        )
    }
}

/// How long `wait_for_widgets` polls for the fixture's three widgets to appear in the a11y
/// tree before giving up. The larger of the two deadlines this consolidates (mcp_cost.rs
/// previously used 5s, `verification_cost.rs`'s now-removed `poll_until_widgets_present`
/// used 10s) so the shared function has the more generous headroom of the two under load.
pub(crate) const WIDGETS_SETTLE_TIMEOUT: Duration = Duration::from_secs(10);

/// Poll (unmetered) until the fixture's three widgets are all present in the a11y tree, or
/// `WIDGETS_SETTLE_TIMEOUT` elapses (in which case this panics, embedding the last-seen
/// outline so a timeout is legible rather than surfacing later as a downstream `.expect("...
/// #id")`). accesskit's winit adapter briefly publishes a placeholder root (just `#0
/// Application`) before its `InitialTreeRequested` handshake fills in the real tree, so the
/// very first snapshot after `start_fixture` returns can race the app's own startup. That
/// race is a property of the fixture's launch, not of either arm's verification strategy, so
/// it is settled here, unmetered, before a caller takes the snapshot it actually pays for (or,
/// for arm B and the probe test, reuses this function's own settled outline directly). Shared
/// by both arms here and by `verification_cost.rs`'s probe test.
pub async fn wait_for_widgets(client: &Peer<RoleClient>) -> String {
    let deadline = tokio::time::Instant::now() + WIDGETS_SETTLE_TIMEOUT;
    loop {
        let outline = a11y_outline(client).await;
        let ready = find_named_button(&outline, "Apply").is_some()
            && find_by_role(&outline, "slider").is_some()
            && find_by_role(&outline, "text").is_some();
        if ready {
            return outline;
        }
        if tokio::time::Instant::now() >= deadline {
            panic!(
                "wait_for_widgets: timed out after {WIDGETS_SETTLE_TIMEOUT:?} waiting for the \
                 fixture's widgets to appear in the a11y tree; last-seen outline:\n{outline}"
            );
        }
        tokio::time::sleep(A11Y_POLL_INTERVAL).await;
    }
}

/// Arm A — the semantic/text-only loop. One a11y snapshot to resolve #ids, then set_value
/// on the field and slider, click Apply by #id, and confirm completion by the fixture's log
/// line. No screenshots: verification is the a11y tree and the app log.
pub async fn run_arm_a(client: &Peer<RoleClient>) -> ArmReport {
    let mut m = CallMeter::default();

    wait_for_widgets(client).await;

    // Resolve #ids (this snapshot IS part of arm A's cost — it is how the arm "sees").
    let (_r, outline) = metered_call(client, &mut m, "glass_a11y_snapshot", json!({})).await;
    let text_id = find_by_role(&outline, "text").expect("text field #id");
    let slider_id = find_by_role(&outline, "slider").expect("slider #id");
    let apply_id = find_named_button(&outline, "Apply").expect("Apply #id");

    // Set the text field. The fixture's plain egui TextEdit exposes no writable
    // EditableText/Value interface over AT-SPI (`AxElementNotEditable` — a known toolkit
    // gap; see glass-core's error text), so `glass_set_value` fails on it. Work around that
    // by focusing the field with id-based `glass_click_element` (keeping arm A id-based, not
    // pixel) and then typing at the focused element with `glass_type`. Still image-free.
    metered_call(
        client,
        &mut m,
        "glass_click_element",
        json!({ "id": text_id }),
    )
    .await;
    metered_call(client, &mut m, "glass_type", json!({ "text": "benchmark" })).await;

    // Set the slider. glass_set_value writes numeric/range widgets through the AT-SPI Value
    // interface (see glass-a11y-linux's `writes_value_only`), which the egui slider exposes.
    metered_call(
        client,
        &mut m,
        "glass_set_value",
        json!({ "id": slider_id, "text": "50" }),
    )
    .await;

    // Click Apply by #id.
    metered_call(
        client,
        &mut m,
        "glass_click_element",
        json!({ "id": apply_id }),
    )
    .await;

    // Confirm completion via the app log — no image. The fixture logs synchronously on
    // click, so by the time this call is made "[fixture] apply" may already be buffered;
    // `cursor: 0` scans the whole buffer instead of only lines emitted after this call
    // (glass_wait_for_log's own documented behavior for that case).
    let (res, _t) = metered_call(
        client,
        &mut m,
        "glass_wait_for_log",
        json!({ "contains": "[fixture] apply", "cursor": 0, "timeout_ms": 3_000 }),
    )
    .await;
    assert_eq!(
        res["matched"],
        json!(true),
        "arm A never observed the apply log line"
    );

    ArmReport::from_meter("semantic", &m)
}

/// `(x, y, w, h)` for node #id, parsed from its outline line's `(x,y wxh)` field. Uses the
/// LAST `(...)` group on the line, not the first: a widget's quoted name precedes its
/// bounds and can itself contain a literal `(` (e.g. a button named "Apply (default)"), so
/// `rfind` is required — the bounds are always the final parenthesized group (states after
/// them use `[...]`, not parens).
pub fn parse_bounds(outline: &str, id: u32) -> Option<(i64, i64, u32, u32)> {
    let tag = format!("#{id} ");
    let line = outline.lines().find(|l| l.trim_start().starts_with(&tag))?;
    let open = line.rfind('(')?;
    let close = line[open..].find(')')? + open;
    let inner = &line[open + 1..close]; // "x,y wxh"
    let (xy, wh) = inner.split_once(' ')?;
    let (xs, ys) = xy.split_once(',')?;
    let (ws, hs) = wh.split_once('x')?;
    Some((
        xs.trim().parse().ok()?,
        ys.trim().parse().ok()?,
        ws.trim().parse().ok()?,
        hs.trim().parse().ok()?,
    ))
}

/// Center of a node's bounds as window-relative (x, y) for a pixel click.
fn center(b: (i64, i64, u32, u32)) -> (i64, i64) {
    (b.0 + b.2 as i64 / 2, b.1 + b.3 as i64 / 2)
}

/// Arm B — the screenshot-every-step loop. See-act-see: screenshot to locate, act by pixel,
/// screenshot to verify, at each step. Click targets come from ONE unmetered setup snapshot
/// (a vision agent would locate them from the image; using a11y bounds here is deterministic
/// test scaffolding that keeps the run reproducible without charging that setup step to arm
/// B's metered cost). Arm B's metered sequence below is screenshots and pixel clicks/type/drag
/// only — never a metered a11y snapshot.
pub async fn run_arm_b(client: &Peer<RoleClient>) -> ArmReport {
    // Unmetered setup: settle the a11y-tree-population race and reuse the outline
    // `wait_for_widgets` already settled on as arm B's one unmetered setup snapshot, rather
    // than paying for a second `a11y_outline` call.
    let outline = wait_for_widgets(client).await;
    let text_id = find_by_role(&outline, "text").expect("text field #id");
    let slider_id = find_by_role(&outline, "slider").expect("slider #id");
    let apply_id = find_named_button(&outline, "Apply").expect("Apply #id");
    let (tx, ty) = center(parse_bounds(&outline, text_id).expect("text bounds"));
    let (sx, sy) = center(parse_bounds(&outline, slider_id).expect("slider bounds"));
    let (ax, ay) = center(parse_bounds(&outline, apply_id).expect("apply bounds"));

    let mut m = CallMeter::default();

    // See.
    metered_call(client, &mut m, "glass_screenshot", json!({})).await;
    // Act: click the text field, type.
    metered_call(client, &mut m, "glass_click", json!({ "x": tx, "y": ty })).await;
    metered_call(client, &mut m, "glass_type", json!({ "text": "benchmark" })).await;
    // See.
    metered_call(client, &mut m, "glass_screenshot", json!({})).await;
    // Act: drag the slider a little to change its value (pixel drag).
    metered_call(
        client,
        &mut m,
        "glass_drag",
        json!({ "x1": sx, "y1": sy, "x2": sx + 20, "y2": sy }),
    )
    .await;
    // See.
    metered_call(client, &mut m, "glass_screenshot", json!({})).await;
    // Act: click Apply by pixel.
    metered_call(client, &mut m, "glass_click", json!({ "x": ax, "y": ay })).await;
    // See (final verify).
    metered_call(client, &mut m, "glass_screenshot", json!({})).await;

    // Confirm completion the same way as arm A (unmetered `call`, not `metered_call`): a test
    // correctness gate that both arms reach the same end state, not part of arm B's
    // screenshot-driven cost. `cursor: 0` scans the whole log buffer, since the fixture may
    // already have logged the apply line by the time this call is made.
    let (res, _t) = call(
        client,
        "glass_wait_for_log",
        json!({ "contains": "[fixture] apply", "cursor": 0, "timeout_ms": 3_000 }),
    )
    .await;
    assert_eq!(
        res["matched"],
        json!(true),
        "arm B never observed the apply log line"
    );

    ArmReport::from_meter("screenshot", &m)
}

/// Run both arms (restarting the fixture between them for a clean UI), assert the
/// determinism and cross-arm invariants, write `target/verification-cost.json`, and return
/// both reports.
pub async fn run_verification_cost(client: &Peer<RoleClient>) -> (ArmReport, ArmReport) {
    // Arm A, twice, to prove the primitives the headline result depends on are deterministic.
    let a1 = run_arm_a(client).await;
    restart_fixture(client).await;
    let a2 = run_arm_a(client).await;
    // Determinism gate: assert the structurally-stable primitives are identical across two runs.
    // round_trips is fixed; Arm A carries no images. text_bytes is NOT gated: it is stable
    // to within a few bytes, but glass_wait_for_log's response embeds a wall-clock elapsed_ms
    // whose digit count can shift if the apply line isn't buffered on the first poll tick —
    // the published image-token result does not depend on that field.
    assert_eq!(
        a1.round_trips, a2.round_trips,
        "arm A round-trips not deterministic"
    );
    assert_eq!(
        a1.image_count, a2.image_count,
        "arm A image count not deterministic"
    );
    assert_eq!(
        a1.image_dims, a2.image_dims,
        "arm A image dims not deterministic"
    );

    // Arm B, twice, same determinism proof as arm A above — arm B carries images, so
    // round_trips, image_count, and image_dims are all meaningful to gate here.
    restart_fixture(client).await;
    let b1 = run_arm_b(client).await;
    restart_fixture(client).await;
    let b2 = run_arm_b(client).await;
    assert_eq!(
        b1.round_trips, b2.round_trips,
        "arm B round-trips not deterministic"
    );
    assert_eq!(
        b1.image_count, b2.image_count,
        "arm B image count not deterministic"
    );
    assert_eq!(
        b1.image_dims, b2.image_dims,
        "arm B image dims not deterministic"
    );
    let b = b1;

    // Cross-arm invariants (the design's claims).
    assert_eq!(a1.image_count, 0, "arm A must be image-free");
    assert!(b.image_count > 0, "arm B must use images");

    let artifact = json!({
        "task": "set a text field, set a slider, click Apply, confirm via the app log",
        "fixture": "glass-fixture-egui @ 400x300, x11 headless",
        "note": "approx_text_tokens = text_bytes.div_ceil(4); image cost is left as dims — \
                 apply your own model's vision-token formula to them",
        "arms": [a1.to_json(), b.to_json()],
    });
    let path = repo_root().join("target/verification-cost.json");
    let body = serde_json::to_string_pretty(&artifact)
        .unwrap_or_else(|e| panic!("serialize verification-cost artifact: {e}"));
    std::fs::write(&path, body).unwrap_or_else(|e| panic!("write {}: {e}", path.display()));
    eprintln!("verification-cost artifact -> {}", path.display());

    (a1, b)
}

/// Stop and relaunch the fixture for a clean UI between arms. `glass_stop` takes no
/// arguments (see `glass_mcp::server::glass_stop`).
async fn restart_fixture(client: &Peer<RoleClient>) {
    call(client, "glass_stop", json!({})).await;
    start_fixture(client).await;
}

#[cfg(test)]
mod tests {
    use super::*;
    use rmcp::model::Content;

    #[test]
    fn meter_counts_text_and_images_and_reads_dims() {
        let mut m = CallMeter::default();
        m.record_request("glass_screenshot", &json!({ "include_image": true }));
        // A screenshot response: image block leads, then the envelope carrying dims.
        let content = vec![
            Content::image("Zm9vYmFy".to_string(), "image/webp".to_string()), // 8 b64 chars
            Content::text(
                json!({ "ok": true, "tool": "glass_screenshot",
                                  "result": { "width": 400, "height": 300 } })
                .to_string(),
            ),
        ];
        let (result, _all, dims) = m.record_response(&content);
        assert_eq!(dims, Some((400, 300)));
        assert_eq!(result["width"], json!(400));
        assert_eq!(m.round_trips, 1);
        assert_eq!(m.image_count, 1);
        assert_eq!(m.image_b64_bytes, 8);
        assert_eq!(m.image_dims, vec![(400, 300)]);
        assert!(m.request_bytes > 0);
        assert!(m.text_bytes > 0);
    }

    #[test]
    fn parse_bounds_happy_path() {
        let outline = "  #12 Button \"Apply\" (10,240 80x30) [focusable]";
        assert_eq!(parse_bounds(outline, 12), Some((10, 240, 80, 30)));
    }

    #[test]
    fn parse_bounds_handles_a_name_containing_parens() {
        // The quoted name precedes the bounds and can itself contain '(': `find` (first
        // paren) would break on this; `rfind` (last paren) must not.
        let outline = "  #7 Button \"Apply (default)\" (8,69 40x18) [focusable]";
        assert_eq!(parse_bounds(outline, 7), Some((8, 69, 40, 18)));
    }

    #[test]
    fn arm_report_tokens_are_div_ceil_of_text_bytes() {
        let m = CallMeter {
            text_bytes: 1241,
            ..Default::default()
        };
        let r = ArmReport::from_meter("a", &m);
        assert_eq!(r.approx_text_tokens(), 311); // 1241.div_ceil(4)
    }
}
