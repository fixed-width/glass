//! `LinuxA11y`: the AT-SPI `Accessibility` reader. Runs each bounded call on a detached thread
//! (`glass_core::A11yThread`) with its own current-thread runtime, so it never `block_on`s inside
//! the caller's tokio runtime; finds the launched app by PID, and walks its subtree into an
//! `AxTree`.

use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::component::ComponentProxy;
use atspi_common::{CoordType, ObjectRefOwned};
use glass_core::{
    A11yThread, Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTarget, AxTree, GlassError,
    Result, WalkBudget, normalize_description, read_back_confirms, write_took_no_effect,
};

use crate::mapping::{map_role, map_states};

/// Every bounded call runs on a fresh detached thread: this reader drives an async API with
/// `block_on`, which panics inside the caller's tokio runtime. The cap is what stops a wedged bus
/// hanging the calling tool for longer than it.
static BUS: A11yThread = A11yThread::new("a11y bus", Duration::from_secs(10));

#[derive(Default)]
pub struct LinuxA11y;

impl LinuxA11y {
    pub fn new() -> Self {
        Self
    }
}

impl Accessibility for LinuxA11y {
    fn subscribe_changes(&mut self, ctx: &AxContext) -> Option<Box<dyn glass_core::ChangeSignal>> {
        // Not one of [`BUS`]'s bounded calls: the subscription's own thread is the long-lived one,
        // and `subscribe` already bounds how long it waits for the registrations to land.
        crate::events::subscribe(ctx)
    }

    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let ctx = ctx.clone();
        BUS.snapshot(ctx.deadline, move || run_snapshot(&ctx))
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let ctx = ctx.clone();
        let target = target.clone();
        let text = text.to_string();
        BUS.set_value(move || run_set_value(&ctx, &target, &text))
    }

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let ctx = ctx.clone();
        let target = target.clone();
        // This reader actuates the element it resolved, so it never substitutes another.
        BUS.invoke(move || run_invoke(&ctx, &target)).map(|()| None)
    }
}

fn run_snapshot(ctx: &AxContext) -> Result<AxTree> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| GlassError::AccessibilityUnavailable(format!("runtime: {e}")))?;
    rt.block_on(snapshot_async(ctx))
}

fn run_set_value(ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| GlassError::AccessibilityUnavailable(format!("runtime: {e}")))?;
    rt.block_on(set_value_async(ctx, target, text))
}

fn run_invoke(ctx: &AxContext, target: &AxTarget) -> Result<()> {
    let rt = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .map_err(|e| GlassError::AccessibilityUnavailable(format!("runtime: {e}")))?;
    rt.block_on(invoke_async(ctx, target))
}

fn bus_err(e: impl std::fmt::Display) -> GlassError {
    GlassError::AccessibilityUnavailable(format!("accessibility bus error: {e}"))
}

/// Error shown when glass reached the a11y bus but the launched app publishes no accessible
/// tree — framed for the developer (it's their app's choice), distinct from a glass/bus problem.
///
/// Carried by [`GlassError::AccessibilityNotReady`].
fn no_app_tree_message(pids: &[u32]) -> String {
    format!(
        "the launched app (pid {pids:?}) isn't publishing an accessibility tree. If it should, \
         enable accessibility in your UI toolkit (e.g. AccessKit for egui/winit, or your GTK/Qt \
         a11y); some apps (games, canvas) intentionally don't — use the pixel loop (screenshot / \
         click / diff) there instead."
    )
}

/// Connect + find the launched app's accessible ref (shared by snapshot and set_value).
/// Returns the app's `ObjectRefOwned` (`'static`) and the connection — NOT a proxy (a
/// proxy would borrow the connection and can't be returned alongside it).
async fn find_app(ctx: &AxContext) -> Result<(ObjectRefOwned, zbus::Connection)> {
    let conn = match ctx.a11y_bus_addr.as_deref() {
        Some(addr) => {
            let parsed = addr.try_into().map_err(|e| {
                GlassError::AccessibilityUnavailable(format!("bad a11y address: {e}"))
            })?;
            AccessibilityConnection::from_address(parsed)
                .await
                .map_err(|e| {
                    GlassError::AccessibilityUnavailable(format!(
                        "cannot reach the private a11y bus ({e})"
                    ))
                })?
        }
        None => {
            return Err(GlassError::AccessibilityUnavailable(
                "no accessibility bus for this launch — relaunch the app with a11y:true \
                 to enable the accessibility tree (Linux)"
                    .into(),
            ));
        }
    };
    let zbus_conn = conn.connection().clone();
    let root = conn.root_accessible_on_registry().await.map_err(bus_err)?;

    // The registry root's children are the registered applications. Pick ours by
    // PID. We keep the matching `ObjectRefOwned` (which is `'static`) and build the
    // proxy after the loop, so the proxy doesn't borrow a loop-local `ObjectRef`.
    let mut chosen: Option<ObjectRefOwned> = None;
    for app_ref in root.get_children().await.map_err(bus_err)? {
        if app_matches(&app_ref, ctx, &zbus_conn).await {
            chosen = Some(app_ref);
            break;
        }
    }
    let app_ref =
        chosen.ok_or_else(|| GlassError::AccessibilityNotReady(no_app_tree_message(&ctx.pids)))?;
    Ok((app_ref, zbus_conn))
}

async fn snapshot_async(ctx: &AxContext) -> Result<AxTree> {
    let (app_ref, zbus_conn) = find_app(ctx).await?;
    let app = app_ref
        .as_accessible_proxy(&zbus_conn)
        .await
        .map_err(bus_err)?;

    let mut budget = WalkBudget::with_limits(ctx.limits);
    let root_node = Box::pin(walk(&app, &zbus_conn, 0, &mut budget)).await?;
    let mut tree = AxTree::new(root_node);
    tree.truncated = budget.truncation();
    tree.unreadable = budget.unreadable();
    tree.unexposed = budget.unexposed();
    tree.assign_ids();
    Ok(tree)
}

/// Whether `set_value` must write through the AT-SPI `Value` interface only, skipping
/// `EditableText`. A `GtkSpinButton` exposes both interfaces, but `EditableText` writes its
/// inner entry buffer without committing to the adjustment (the value silently reverts);
/// numeric/range widgets with a numeric target must go through `Value`, the sole interface
/// that applies the change.
fn writes_value_only(role: glass_core::AxRole, text: &str) -> bool {
    use glass_core::AxRole::*;
    matches!(role, Slider | SpinButton | ScrollBar) && text.parse::<f64>().is_ok()
}

/// Poll bound for confirming a write actually landed — a value written, or a toggle moved: the
/// toolkit applies it on a later main-loop pass, so the first read after dispatch is expected to
/// be stale. Generous enough for a loaded headless session without letting a real failure hang
/// the tool.
const VERIFY_POLLS: usize = 6;
/// See [`VERIFY_POLLS`].
const VERIFY_INTERVAL: Duration = Duration::from_millis(120);

/// What a write that took no effect looks like on this backend: the element's `EditableText`
/// answers the write with `true` and keeps the text it had (read on Firefox 153 web content,
/// 2026-08-24). The AT-SPI sibling of the `READ_ONLY_PROJECTION` consts in the Windows and macOS
/// readers.
const ACKNOWLEDGED_NOT_APPLIED: &str = "this element's accessibility interface acknowledged the write without applying it — focus the \
     element and type into it instead";

async fn set_value_async(ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
    let (app_ref, conn) = find_app(ctx).await?;
    let app = app_ref.as_accessible_proxy(&conn).await.map_err(bus_err)?;
    let mut budget = WalkBudget::with_limits(ctx.limits);
    let node_ref = Box::pin(find_nth(&app_ref, &app, &conn, 0, target.id.0, &mut budget))
        .await?
        .ok_or(GlassError::AxElementChanged(target.id.0))?;
    let node = node_ref.as_accessible_proxy(&conn).await.map_err(bus_err)?;

    // Verify role + name against the fingerprint (guards a stale id / mirror drift).
    let role = map_role(node.get_role().await.map_err(bus_err)?);
    let name = nonempty(node.name().await.unwrap_or_default());
    if !target.matches(role, name.as_deref()) {
        return Err(GlassError::AxElementChanged(target.id.0));
    }

    // Boolean widgets (switch/checkbox/toggle/radio) have no text buffer: set them
    // through the Action interface (`toggle`) + `Checked` state, before the
    // EditableText/Value paths. Combos are handled a layer up (session-level
    // keyboard navigation), never reaching here.
    {
        use glass_core::AxRole::{CheckBox, RadioButton, ToggleButton};
        if matches!(role, CheckBox | ToggleButton | RadioButton)
            && let Some(on) = parse_bool(text)
        {
            return set_toggle(&conn, &node, role, on, text, target.id.0).await;
        }
    }

    let dest = node.inner().destination().to_owned();
    let path = node.inner().path().to_owned();
    // Numeric/range widgets go through Value only (see `writes_value_only`): a GtkSpinButton
    // also exposes EditableText, but writing its entry buffer doesn't commit to the adjustment.
    // Text widgets prefer EditableText, falling back to Value for anything numeric that lacks it.
    // The builder `.ok()` chaining mirrors the working ComponentProxy build in `extents`.
    if !writes_value_only(role, text) {
        let editable = atspi::proxy::editable_text::EditableTextProxy::builder(&conn)
            .destination(dest.clone())
            .ok()
            .and_then(|b| b.path(path.clone()).ok());
        if let Some(b) = editable
            && let Ok(et) = b.build().await
        {
            // The baseline for the confirmation below: without one, only an exact read-back
            // confirms.
            let before = read_text(&node, &conn).await;
            match et.set_text_contents(text).await {
                Ok(true) => {
                    return confirm_write(
                        &node,
                        &conn,
                        WrittenVia::EditableText,
                        before,
                        text,
                        target.id.0,
                    )
                    .await;
                }
                // EditableText is present but rejected the write — don't try Value.
                Ok(false) => return Err(GlassError::AxElementNotEditable(target.id.0)),
                Err(_) => {} // interface absent / call failed — fall through to Value
            }
        }
    }
    if let Ok(v) = text.parse::<f64>() {
        let value_proxy = atspi::proxy::value::ValueProxy::builder(&conn)
            .destination(dest)
            .ok()
            .and_then(|b| b.path(path).ok());
        if let Some(b) = value_proxy
            && let Ok(vp) = b.build().await
        {
            let before = read_number(&node, &conn).await;
            if vp.set_current_value(v).await.is_ok() {
                return confirm_write(&node, &conn, WrittenVia::Value, before, text, target.id.0)
                    .await;
            }
        }
    }
    Err(GlassError::AxElementNotEditable(target.id.0))
}

/// Which interface a write went through, and so which read confirms it.
#[derive(Clone, Copy)]
enum WrittenVia {
    EditableText,
    Value,
}

/// Confirm a dispatched write landed, by reading the element back through the interface it was
/// written to (`before` is what that read returned beforehand, `None` when it failed). Polls,
/// because the toolkit applies the write on a later main-loop pass.
///
/// Without it the toolkit's own answer is the only evidence, and an engine can acknowledge a write
/// it never applies — see [`ACKNOWLEDGED_NOT_APPLIED`].
async fn confirm_write(
    node: &AccessibleProxy<'_>,
    conn: &zbus::Connection,
    via: WrittenVia,
    before: Option<String>,
    requested: &str,
    id: u32,
) -> Result<()> {
    let mut observed = None;
    for poll in 0..VERIFY_POLLS {
        if poll > 0 {
            tokio::time::sleep(VERIFY_INTERVAL).await;
        }
        observed = match via {
            WrittenVia::EditableText => read_text(node, conn).await,
            WrittenVia::Value => read_number(node, conn).await,
        };
        if read_back_confirms(observed.as_deref(), before.as_deref(), requested) {
            return Ok(());
        }
    }
    Err(write_verdict(
        id,
        requested,
        before.as_deref(),
        observed.as_deref(),
    ))
}

/// The verdict for a write whose read-back never held the request. [`ACKNOWLEDGED_NOT_APPLIED`]
/// only where the element still holds exactly what it held before — a read that failed, or one
/// showing a value the element reformatted, is not evidence the write never arrived.
fn write_verdict(
    id: u32,
    requested: &str,
    before: Option<&str>,
    observed: Option<&str>,
) -> GlassError {
    match observed {
        Some(seen) if write_took_no_effect(seen, before) => GlassError::value_not_applied_because(
            id,
            requested,
            Some(seen),
            ACKNOWLEDGED_NOT_APPLIED,
        ),
        seen => GlassError::value_not_applied(id, requested, seen),
    }
}

/// Pre-order DFS to the node at index `target`, mirroring `walk` exactly: visit the node (its
/// id is the arrival count), then recurse each child in `get_children` order, skipping children
/// published as a null ref and those whose proxy fails to build — **and stopping at the same
/// depth/node/sibling bounds**. The
/// bounds must stay in lockstep with `walk`: if this traversal visited nodes `walk` skipped,
/// a `set_value` id would resolve against a different tree and write to the wrong element.
async fn find_nth(
    node_ref: &ObjectRefOwned,
    proxy: &AccessibleProxy<'_>,
    conn: &zbus::Connection,
    depth: usize,
    target: u32,
    budget: &mut WalkBudget,
) -> Result<Option<ObjectRefOwned>> {
    if budget.nodes_walked() == target as usize {
        return Ok(Some(node_ref.clone()));
    }
    budget.visit();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let child_refs = proxy.get_children().await.map_err(bus_err)?;
    if child_refs.is_empty() || !budget.may_explore_children(depth) {
        return Ok(None);
    }
    for (scanned, child_ref) in child_refs.into_iter().enumerate() {
        if !budget.may_visit_sibling(scanned) {
            break;
        }
        // Both skips are counted in `walk` too, so the two traversals report the same drops.
        if child_ref.is_null() {
            budget.note_unexposed();
            continue;
        }
        let Ok(child) = child_ref.as_accessible_proxy(conn).await else {
            budget.note_unreadable();
            continue;
        };
        if let Some(found) = Box::pin(find_nth(
            &child_ref,
            &child,
            conn,
            depth + 1,
            target,
            budget,
        ))
        .await?
        {
            return Ok(Some(found));
        }
    }
    Ok(None)
}

/// Every unique D-Bus name belonging to the app in `ctx` — the keys the event filter matches on.
///
/// A set, not one name: a walk crosses process boundaries (each child is proxied at *its own* bus
/// name), so an app that embeds an out-of-process view publishes part of its tree from a second
/// connection. Filtering on one name would walk that subtree while dropping every event it emits.
/// Matched by pid, the same rule [`find_app`] applies — though not at the same moment: this
/// resolves once when the subscription is taken, and a walk re-resolves each time.
pub(crate) async fn app_bus_names(ctx: &AxContext, conn: &AccessibilityConnection) -> Vec<String> {
    let zbus_conn = conn.connection().clone();
    let Ok(root) = conn.root_accessible_on_registry().await else {
        return Vec::new();
    };
    let Ok(children) = root.get_children().await else {
        return Vec::new();
    };
    let mut names = Vec::new();
    for app_ref in children {
        if app_matches(&app_ref, ctx, &zbus_conn).await
            && let Some(name) = app_ref.name()
        {
            names.push(name.to_string());
        }
    }
    names
}

/// Does this AT-SPI application belong to the launched process? PID is the reliable
/// signal: the app matches when its owning pid is in `ctx.pids` (the launched app's PID
/// set — root + enumerable descendants). An empty set (no pid hint, e.g. a backend that
/// can't enumerate) accepts the first app (refine later).
async fn app_matches(app_ref: &ObjectRefOwned, ctx: &AxContext, conn: &zbus::Connection) -> bool {
    if ctx.pids.is_empty() {
        return true; // no pid hint: accept the first app (refine by geometry/title elsewhere)
    }
    let Some(unique) = app_ref.name() else {
        return false;
    };
    let Ok(dbus) = zbus::fdo::DBusProxy::new(conn).await else {
        return false;
    };
    match dbus
        .get_connection_unix_process_id(unique.clone().into())
        .await
    {
        Ok(pid) => ctx.pids.contains(&pid),
        Err(_) => false,
    }
}

/// Recursively build a normalized node from an AT-SPI accessible, bounded by
/// [`WalkBudget`] (node count, nesting depth, and per-level sibling scan) so a
/// pathological tree can't burn the reader's whole ceiling with no tree to show for it.
async fn walk(
    proxy: &AccessibleProxy<'_>,
    conn: &zbus::Connection,
    depth: usize,
    budget: &mut WalkBudget,
) -> Result<AxNode> {
    budget.visit();
    // Issue the seven independent per-node reads concurrently on the shared connection and await
    // the slowest, instead of paying seven sequential D-Bus round-trips (~7x the per-node
    // latency); zbus multiplexes concurrent method calls over one connection. Traversal order and
    // `budget` accounting are unchanged, so node ids stay in lockstep with `find_nth`. On the
    // error path `join!` completes all seven before bailing — the same result, at the cost of a
    // few reads on a snapshot that was already failing.
    let (role_res, raw_role_res, name_res, description_res, state_res, bounds, child_refs_res) = tokio::join!(
        proxy.get_role(),
        proxy.get_role_name(),
        proxy.name(),
        proxy.description(),
        proxy.get_state(),
        extents(proxy, conn),
        proxy.get_children(),
    );
    let role = role_res.map_err(bus_err)?;
    let raw_role = raw_role_res.unwrap_or_default();
    let name = nonempty(name_res.unwrap_or_default());
    let description = normalize_description(&description_res.unwrap_or_default(), name.as_deref());
    let states = map_states(&state_res.map_err(bus_err)?);

    let mut children = Vec::new();
    // Resolved before the gate: a childless node must never be reported truncated for
    // declining to explore a list that was already empty.
    let child_refs = child_refs_res.map_err(bus_err)?;
    if !child_refs.is_empty() && budget.may_explore_children(depth) {
        for (scanned, child_ref) in child_refs.into_iter().enumerate() {
            if !budget.may_visit_sibling(scanned) {
                break;
            }
            // AT-SPI's null ref stands in for content the app has not exposed; Brave 151's
            // window published exactly one while renderer accessibility was off (read
            // 2026-08-24). Building a proxy from it only errors, which read as a lost subtree.
            if child_ref.is_null() {
                budget.note_unexposed();
                continue;
            }
            let Ok(child) = child_ref.as_accessible_proxy(conn).await else {
                budget.note_unreadable();
                continue;
            };
            children.push(Box::pin(walk(&child, conn, depth + 1, budget)).await?);
        }
    }

    let value = read_value(proxy, conn, map_role(role)).await;

    Ok(AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role: map_role(role),
        raw_role,
        name,
        description,
        value,
        states,
        bounds,
        children,
    })
}

/// Parse a boolean target for a toggle widget. Accepts the common textual and
/// numeric spellings; `None` means "not a boolean" (so the caller falls through
/// to another path rather than guessing).
fn parse_bool(text: &str) -> Option<bool> {
    match text.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "on" | "yes" | "checked" | "check" => Some(true),
        "false" | "0" | "off" | "no" | "unchecked" | "uncheck" => Some(false),
        _ => None,
    }
}

/// AT-SPI action names that flip / activate a boolean widget. A GtkSwitch exposes
/// `"toggle"`; buttons/checkboxes expose `"click"`/`"activate"`/`"press"`.
const TOGGLE_ACTION_NAMES: &[&str] = &[
    "toggle", "click", "activate", "press", "check", "uncheck", "switch",
];

/// Outcome of one AT-SPI Action attempt on a node.
enum ActionAttempt {
    /// An action from the ladder was found and fired: `ok` is the bus-reported success,
    /// `action` the (lowercased) action name that ran — the caller uses it to decide
    /// whether a post-fire state verify applies.
    Fired { ok: bool, action: String },
    /// The node exposes no Action interface, or none of `names` is among its actions.
    /// Nothing was dispatched.
    Unavailable,
    /// An AT-SPI call failed, so the outcome is genuinely unknown — in particular a
    /// failed `DoAction` may still have reached the toolkit. Distinct from
    /// `Fired(false)` (the toolkit answered "I did not run it") so a caller can't
    /// report a transport failure as a truthful "did not run".
    Error(String),
}

/// Fire the node's first Action whose name is in `names`.
///
/// A proxy that cannot be *built* is `Unavailable`: construction dispatches no action, and
/// the overwhelmingly common cause is the node not implementing the Action interface at all.
/// From `NActions` onward every RPC failure is `Error`.
async fn try_action(
    conn: &zbus::Connection,
    node: &AccessibleProxy<'_>,
    names: &[&str],
) -> ActionAttempt {
    let dest = node.inner().destination().to_owned();
    let path = node.inner().path().to_owned();
    let Some(action) = atspi::proxy::action::ActionProxy::builder(conn)
        .destination(dest)
        .ok()
        .and_then(|b| b.path(path).ok())
    else {
        return ActionAttempt::Unavailable;
    };
    let Ok(a) = action.build().await else {
        return ActionAttempt::Unavailable;
    };
    let n = match a.n_actions().await {
        Ok(n) => n,
        Err(e) => return ActionAttempt::Error(format!("NActions: {e}")),
    };
    for i in 0..n {
        let name = match a.get_name(i).await {
            Ok(name) => name.to_ascii_lowercase(),
            Err(e) => return ActionAttempt::Error(format!("GetName({i}): {e}")),
        };
        if names.contains(&name.as_str()) {
            return match a.do_action(i).await {
                Ok(ok) => ActionAttempt::Fired { ok, action: name },
                Err(e) => ActionAttempt::Error(format!("DoAction({i}): {e}")),
            };
        }
    }
    ActionAttempt::Unavailable
}

/// The AT-SPI state flag carrying a widget's boolean state: toggle buttons expose it as
/// `Pressed`, checkboxes/switches/radios as `Checked`. Shared by `set_value`'s toggle write
/// and `invoke`'s toggle verify so the two can never read a control's state differently.
fn toggle_state_flag(role: glass_core::AxRole) -> atspi_common::State {
    if role == glass_core::AxRole::ToggleButton {
        atspi_common::State::Pressed
    } else {
        atspi_common::State::Checked
    }
}

/// Set a boolean widget (switch/checkbox/toggle/radio) to `target_on`, as the caller spelled it in
/// `requested`. Idempotent:
/// only invokes the toggle action when the boolean state differs, then confirms the
/// state actually changed (the toolkit applies the action on its next loop) — so a
/// no-op activation (e.g. a radio can't be *un*-selected by clicking it) is reported
/// as `AxValueNotApplied`, never a silent success.
async fn set_toggle(
    conn: &zbus::Connection,
    node: &AccessibleProxy<'_>,
    role: glass_core::AxRole,
    target_on: bool,
    requested: &str,
    id: u32,
) -> Result<()> {
    let flag = toggle_state_flag(role);
    if node.get_state().await.map_err(bus_err)?.contains(flag) == target_on {
        return Ok(()); // already in the desired state
    }
    // `Error` (an AT-SPI call that failed) folds in with "did not fire" here, keeping
    // `set_value`'s behavior unchanged: this path verifies the state below and reports
    // `AxValueNotApplied` on no change, so an ambiguous outcome is caught by that check
    // rather than needing its own classification.
    if !matches!(
        try_action(conn, node, TOGGLE_ACTION_NAMES).await,
        ActionAttempt::Fired { ok: true, .. }
    ) {
        // No toggle action (e.g. a GTK4 GtkCheckButton exposes none) — can't set it
        // through accessibility; the caller should drive it with click_element.
        return Err(GlassError::AxElementNotEditable(id));
    }
    // Poll until the toolkit applies it; a no-op activation never converges.
    let mut last_on = None;
    for _ in 0..VERIFY_POLLS {
        tokio::time::sleep(VERIFY_INTERVAL).await;
        let on = node.get_state().await.map_err(bus_err)?.contains(flag);
        last_on = Some(on);
        if on == target_on {
            return Ok(());
        }
    }
    // Report the state the last poll read, not `!target_on` derived from the request — the two
    // agree only while this equality is the loop's sole exit, and nothing holds that.
    // The radio note only where the site can see one: `set_toggle` also serves checkboxes and
    // switches, and asking to *select* a radio is not being refused an un-selection.
    let why = if role == glass_core::AxRole::RadioButton && !target_on {
        "the control's toggle action fired without moving it, which is how a radio button reports \
         that it cannot be unselected"
    } else {
        "the control's toggle action fired and its state did not change within the poll window"
    };
    Err(GlassError::value_not_applied_because(
        id,
        requested,
        last_on.map(toggle_state_label),
        why,
    ))
}

/// How a boolean control's state is named in a verdict the caller reads — one spelling, where
/// `set_value` accepts `true`/`on`/`1` for the request alike.
fn toggle_state_label(on: bool) -> &'static str {
    if on { "on" } else { "off" }
}

/// The one AT-SPI action whose meaning is "flip this control's boolean state" — and so the
/// one rung whose success can be checked by re-reading that state after firing.
const TOGGLE_ACTION: &str = "toggle";

/// AT-SPI action names that activate a widget for a generic click. Broader than
/// [`TOGGLE_ACTION_NAMES`] on the activation side (push/jump), narrower on the
/// check/uncheck side — those are set_value verbs, not clicks.
///
/// This is a membership set, not a priority order: [`try_action`] fires the node's first
/// action that appears in this list, so the widget's own action-index order decides which
/// one runs when it exposes several.
const ACTIVATE_ACTION_NAMES: &[&str] =
    &["click", "activate", "press", "push", "jump", TOGGLE_ACTION];

/// Confirm a fired `toggle` action actually moved the control: poll its boolean state until
/// it differs from `was_on`. Without this, a toolkit that accepts and acknowledges the action
/// but never applies it (or applies it to nothing) reports a successful click on a control
/// that did not move — a silent failure. The failure is `AxActionFailed`, which propagates
/// rather than falling back to a pointer click: the action was dispatched, so a second
/// actuation could land on top of a late-applying toggle.
async fn verify_toggle_flipped(
    node: &AccessibleProxy<'_>,
    flag: atspi_common::State,
    was_on: bool,
    id: u32,
) -> Result<()> {
    for _ in 0..VERIFY_POLLS {
        tokio::time::sleep(VERIFY_INTERVAL).await;
        if node.get_state().await.map_err(bus_err)?.contains(flag) != was_on {
            return Ok(());
        }
    }
    Err(GlassError::AxActionFailed(
        id,
        "the toggle action was acknowledged but the state did not change".into(),
    ))
}

/// Actuate the element identified by `target` via its native AT-SPI Action — the
/// backend for `Accessibility::invoke`. Re-walks pre-order to `target.id`, verifies
/// the fingerprint (same gate as `set_value_async`, guarding a stale id / mirror
/// drift), then fires the first action in [`ACTIVATE_ACTION_NAMES`].
///
/// When the action that fired is [`TOGGLE_ACTION`], the ack alone is not accepted as
/// success: the control's boolean state must be observed to change (see
/// [`verify_toggle_flipped`]).
async fn invoke_async(ctx: &AxContext, target: &AxTarget) -> Result<()> {
    let (app_ref, conn) = find_app(ctx).await?;
    let app = app_ref.as_accessible_proxy(&conn).await.map_err(bus_err)?;
    let mut budget = WalkBudget::with_limits(ctx.limits);
    let node_ref = Box::pin(find_nth(&app_ref, &app, &conn, 0, target.id.0, &mut budget))
        .await?
        .ok_or(GlassError::AxElementChanged(target.id.0))?;
    let node = node_ref.as_accessible_proxy(&conn).await.map_err(bus_err)?;
    // Verify role + name against the fingerprint (guards a stale id / mirror drift) —
    // same gate as set_value_async.
    let role = map_role(node.get_role().await.map_err(bus_err)?);
    let name = nonempty(node.name().await.unwrap_or_default());
    if !target.matches(role, name.as_deref()) {
        return Err(GlassError::AxElementChanged(target.id.0));
    }
    // Read the control's boolean state BEFORE firing, so a `toggle` action can be verified by
    // an actual flip below — afterwards there is nothing left to compare against. Costs one
    // property read on every native click; the alternative is trusting a toolkit ack that a
    // switch may never honor. Meaningless for a control with no boolean state (a plain
    // button), which is why only the `toggle` rung consults it.
    let flag = toggle_state_flag(role);
    let was_on = node.get_state().await.map_err(bus_err)?.contains(flag);
    match try_action(&conn, &node, ACTIVATE_ACTION_NAMES).await {
        ActionAttempt::Fired { ok: true, action } if action == TOGGLE_ACTION => {
            verify_toggle_flipped(&node, flag, was_on, target.id.0).await
        }
        ActionAttempt::Fired { ok: true, .. } => Ok(()),
        ActionAttempt::Fired { ok: false, .. } => Err(GlassError::AxActionFailed(
            target.id.0,
            "the toolkit reported the action did not run".into(),
        )),
        ActionAttempt::Unavailable => Err(GlassError::AxActionUnavailable(target.id.0)),
        // Not `AxActionUnavailable`: the call may have reached the toolkit, so this must not
        // be fallback-eligible (see `GlassError::invoke_fallback_eligible`) — a pointer click
        // on top of a landed action would actuate the control twice.
        ActionAttempt::Error(msg) => Err(GlassError::AccessibilityUnavailable(format!(
            "AT-SPI action call failed: {msg}"
        ))),
    }
}

/// Window-relative bounds via the Component interface, or `None` if the node has no
/// geometry / doesn't implement Component / reports a zero-area rect.
///
/// These extents are **toolkit-approximate**: AT-SPI geometry is "locate the element"
/// precision, not "trace its border". Widths are usually exact but the reported `x`/`y`
/// can drift per widget (measured ~10-20px under headless GTK4), so consumers (e.g. the
/// Set-of-Mark overlay) must not treat the box as pixel-perfect. Addressing stays
/// reliable because clicks target the bounds *center*, which remains within the element.
async fn extents(proxy: &AccessibleProxy<'_>, conn: &zbus::Connection) -> Option<AxRect> {
    let dest = proxy.inner().destination().to_owned();
    let path = proxy.inner().path().to_owned();
    let comp = ComponentProxy::builder(conn)
        .destination(dest)
        .ok()?
        .path(path)
        .ok()?
        .build()
        .await
        .ok()?;
    let (x, y, w, h) = comp.get_extents(CoordType::Window).await.ok()?;
    if w <= 0 || h <= 0 {
        return None;
    }
    Some(AxRect {
        x,
        y,
        width: w as u32,
        height: h as u32,
    })
}

/// Read the element's current value/text for value-bearing roles, or `None`.
/// Text-editable roles read the `Text` interface; numeric roles read `Value`.
/// Gated by role so the walk adds at most one D-Bus call on relevant nodes.
///
/// An empty text field carries no value here, unlike [`read_text`]'s read-back: the outline
/// renders what an element holds, and `""` is nothing to show.
async fn read_value(
    proxy: &AccessibleProxy<'_>,
    conn: &zbus::Connection,
    role: glass_core::AxRole,
) -> Option<String> {
    use glass_core::AxRole::*;
    match role {
        TextField | TextArea | ComboBox => read_text(proxy, conn).await.and_then(nonempty),
        Slider | SpinButton | ProgressBar => read_number(proxy, conn).await,
        _ => None,
    }
}

/// The element's text over the AT-SPI `Text` interface. `Some("")` is an element holding nothing;
/// `None` is no reading at all — the interface is absent, or the call failed — which
/// [`read_back_confirms`] must never mistake for a value.
async fn read_text(proxy: &AccessibleProxy<'_>, conn: &zbus::Connection) -> Option<String> {
    let text = atspi::proxy::text::TextProxy::builder(conn)
        .destination(proxy.inner().destination().to_owned())
        .ok()?
        .path(proxy.inner().path().to_owned())
        .ok()?
        .build()
        .await
        .ok()?;
    let n = text.character_count().await.ok()?;
    text.get_text(0, n).await.ok()
}

/// The element's number over the AT-SPI `Value` interface, spelled as a caller spells a value.
/// `None` when the interface is absent or the call failed.
async fn read_number(proxy: &AccessibleProxy<'_>, conn: &zbus::Connection) -> Option<String> {
    let val = atspi::proxy::value::ValueProxy::builder(conn)
        .destination(proxy.inner().destination().to_owned())
        .ok()?
        .path(proxy.inner().path().to_owned())
        .ok()?
        .build()
        .await
        .ok()?;
    val.current_value().await.ok().map(|v| v.to_string())
}

fn nonempty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
}

#[cfg(test)]
mod toggle_label_tests {
    use super::toggle_state_label;

    #[test]
    fn a_control_that_stayed_off_is_reported_as_off() {
        // Inverted, a failed `set_value("true")` reports the control as already on — the state the
        // caller asked for, alongside "did not take".
        assert_eq!(toggle_state_label(false), "off");
        assert_eq!(toggle_state_label(true), "on");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn no_matching_app_message_is_developer_framed() {
        let msg = no_app_tree_message(&[4321, 4322]);
        assert!(msg.contains("4321"), "names the PID(s)");
        assert!(msg.contains("enable accessibility") || msg.contains("AccessKit"));
        assert!(
            msg.contains("pixel") || msg.contains("screenshot"),
            "points at the pixel-loop fallback"
        );
        assert!(
            !msg.contains("relaunch with a11y:true"),
            "distinct from the bus/opt-in error"
        );
    }

    /// The remedy is for a write that never arrived. Attached to a value the element transformed,
    /// it would send the caller to type text the element already has in another form.
    #[test]
    fn only_a_value_the_element_still_holds_carries_the_write_never_arrived_remedy() {
        let unchanged = write_verdict(3, "typed", Some("old"), Some("old"));
        assert!(
            matches!(
                unchanged,
                GlassError::AxValueNotApplied { why: Some(_), .. }
            ),
            "got: {unchanged:?}"
        );
        let transformed = write_verdict(3, "50", Some("1"), Some("50.0"));
        assert!(
            matches!(transformed, GlassError::AxValueNotApplied { why: None, .. }),
            "got: {transformed:?}"
        );
        let unread = write_verdict(3, "typed", Some("old"), None);
        assert!(
            matches!(unread, GlassError::AxValueNotApplied { why: None, .. }),
            "a read-back that failed is no evidence about the write; got: {unread:?}"
        );
    }

    #[test]
    fn writes_value_only_for_numeric_range_widgets() {
        use glass_core::AxRole::*;
        assert!(writes_value_only(SpinButton, "4"));
        assert!(writes_value_only(Slider, "50.5"));
        assert!(writes_value_only(ScrollBar, "0"));
    }

    #[test]
    fn writes_value_only_is_false_for_text_or_non_numeric() {
        use glass_core::AxRole::*;
        // A text field uses EditableText even when its content is numeric.
        assert!(!writes_value_only(TextField, "4"));
        // A non-numeric target isn't the value path.
        assert!(!writes_value_only(SpinButton, "abc"));
        assert!(!writes_value_only(Button, "x"));
    }
}
