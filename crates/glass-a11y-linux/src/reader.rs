//! `LinuxA11y`: the AT-SPI `Accessibility` reader. Connects on a private thread +
//! current-thread runtime (so it never `block_on`s inside the caller's tokio
//! runtime), finds the launched app by PID, and walks its subtree into an `AxTree`.

use std::sync::mpsc;
use std::time::Duration;

use atspi::connection::AccessibilityConnection;
use atspi::proxy::accessible::{AccessibleProxy, ObjectRefExt};
use atspi::proxy::component::ComponentProxy;
use atspi_common::{CoordType, ObjectRefOwned};
use glass_core::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxTarget, AxTree, GlassError, Result,
};

use crate::mapping::{map_role, map_states};

/// Hard cap so a wedged a11y bus can't hang the calling tool forever.
const SNAPSHOT_TIMEOUT: Duration = Duration::from_secs(10);

#[derive(Default)]
pub struct LinuxA11y;

impl LinuxA11y {
    pub fn new() -> Self {
        Self
    }
}

impl Accessibility for LinuxA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let ctx = ctx.clone();
        let (tx, rx) = mpsc::channel();
        // atspi is async and may be invoked from within a tokio runtime, where
        // `block_on` panics. Run on a fresh OS thread with its own current-thread
        // runtime, fully decoupled from the caller's runtime.
        std::thread::spawn(move || {
            let _ = tx.send(run_snapshot(&ctx));
        });
        match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(GlassError::AccessibilityUnavailable(
                "accessibility snapshot timed out (a11y bus not responding)".into(),
            )),
        }
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        let ctx = ctx.clone();
        let target = target.clone();
        let text = text.to_string();
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let _ = tx.send(run_set_value(&ctx, &target, &text));
        });
        match rx.recv_timeout(SNAPSHOT_TIMEOUT) {
            Ok(r) => r,
            Err(_) => Err(GlassError::AccessibilityUnavailable(
                "accessibility set_value timed out (a11y bus not responding)".into(),
            )),
        }
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

fn bus_err(e: impl std::fmt::Display) -> GlassError {
    GlassError::AccessibilityUnavailable(format!("accessibility bus error: {e}"))
}

/// Error shown when glass reached the a11y bus but the launched app publishes no accessible
/// tree — framed for the developer (it's their app's choice), distinct from a glass/bus problem.
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
    let app_ref = chosen
        .ok_or_else(|| GlassError::AccessibilityUnavailable(no_app_tree_message(&ctx.pids)))?;
    Ok((app_ref, zbus_conn))
}

async fn snapshot_async(ctx: &AxContext) -> Result<AxTree> {
    let (app_ref, zbus_conn) = find_app(ctx).await?;
    let app = app_ref
        .as_accessible_proxy(&zbus_conn)
        .await
        .map_err(bus_err)?;

    let root_node = Box::pin(walk(&app, &zbus_conn)).await?;
    let mut tree = AxTree::new(root_node);
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

async fn set_value_async(ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
    let (app_ref, conn) = find_app(ctx).await?;
    let app = app_ref.as_accessible_proxy(&conn).await.map_err(bus_err)?;
    let mut counter = 0u32;
    let node_ref = Box::pin(find_nth(&app_ref, &app, &conn, target.id.0, &mut counter))
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
        if matches!(role, CheckBox | ToggleButton | RadioButton) {
            if let Some(on) = parse_bool(text) {
                return set_toggle(&conn, &node, role, on, target.id.0).await;
            }
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
        if let Some(b) = editable {
            if let Ok(et) = b.build().await {
                match et.set_text_contents(text).await {
                    Ok(true) => return Ok(()),
                    // EditableText is present but rejected the write — don't try Value.
                    Ok(false) => return Err(GlassError::AxElementNotEditable(target.id.0)),
                    Err(_) => {} // interface absent / call failed — fall through to Value
                }
            }
        }
    }
    if let Ok(v) = text.parse::<f64>() {
        let value_proxy = atspi::proxy::value::ValueProxy::builder(&conn)
            .destination(dest)
            .ok()
            .and_then(|b| b.path(path).ok());
        if let Some(b) = value_proxy {
            if let Ok(vp) = b.build().await {
                if vp.set_current_value(v).await.is_ok() {
                    return Ok(());
                }
            }
        }
    }
    Err(GlassError::AxElementNotEditable(target.id.0))
}

/// Pre-order DFS to the node at index `target`, mirroring `walk` exactly: visit
/// the node (its id is the arrival counter), then recurse each child in
/// `get_children` order, **skipping children whose proxy fails to build** (just
/// like `walk`'s `let Ok(child) = … else continue`, so ids stay aligned). The
/// proxy is passed by reference and never returned; the owned `ObjectRefOwned`
/// (cloned on match) is returned, so nothing borrows the connection.
async fn find_nth(
    node_ref: &ObjectRefOwned,
    proxy: &AccessibleProxy<'_>,
    conn: &zbus::Connection,
    target: u32,
    counter: &mut u32,
) -> Result<Option<ObjectRefOwned>> {
    if *counter == target {
        return Ok(Some(node_ref.clone()));
    }
    *counter += 1;
    for child_ref in proxy.get_children().await.map_err(bus_err)? {
        let Ok(child) = child_ref.as_accessible_proxy(conn).await else {
            continue;
        };
        if let Some(found) = Box::pin(find_nth(&child_ref, &child, conn, target, counter)).await? {
            return Ok(Some(found));
        }
    }
    Ok(None)
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

/// Recursively build a normalized node from an AT-SPI accessible.
async fn walk(proxy: &AccessibleProxy<'_>, conn: &zbus::Connection) -> Result<AxNode> {
    let role = proxy.get_role().await.map_err(bus_err)?;
    let raw_role = proxy.get_role_name().await.unwrap_or_default();
    let name = nonempty(proxy.name().await.unwrap_or_default());
    let states = map_states(&proxy.get_state().await.map_err(bus_err)?);
    let bounds = extents(proxy, conn).await;

    let mut children = Vec::new();
    for child_ref in proxy.get_children().await.map_err(bus_err)? {
        let Ok(child) = child_ref.as_accessible_proxy(conn).await else {
            continue;
        };
        children.push(Box::pin(walk(&child, conn)).await?);
    }

    let value = read_value(proxy, conn, map_role(role)).await;

    Ok(AxNode {
        id: AxNodeId(0), // assigned by glass_core::AxTree::assign_ids
        role: map_role(role),
        raw_role,
        name,
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

/// Invoke the node's first activating Action (see [`TOGGLE_ACTION_NAMES`]).
/// Returns whether an action was found and reported success.
async fn invoke_activating_action(conn: &zbus::Connection, node: &AccessibleProxy<'_>) -> bool {
    let dest = node.inner().destination().to_owned();
    let path = node.inner().path().to_owned();
    let Some(action) = atspi::proxy::action::ActionProxy::builder(conn)
        .destination(dest)
        .ok()
        .and_then(|b| b.path(path).ok())
    else {
        return false;
    };
    let Ok(a) = action.build().await else {
        return false;
    };
    let n = a.n_actions().await.unwrap_or(0);
    for i in 0..n {
        let name = a.get_name(i).await.unwrap_or_default().to_ascii_lowercase();
        if TOGGLE_ACTION_NAMES.contains(&name.as_str()) {
            return a.do_action(i).await.unwrap_or(false);
        }
    }
    false
}

/// Set a boolean widget (switch/checkbox/toggle/radio) to `target_on`. Idempotent:
/// only invokes the toggle action when the boolean state differs, then confirms the
/// state actually changed (the toolkit applies the action on its next loop) — so a
/// no-op activation (e.g. a radio can't be *un*-selected by clicking it) is reported
/// as `AxValueNotApplied`, never a silent success. Toggle buttons expose their state
/// via `Pressed`; checkboxes/switches/radios via `Checked`.
async fn set_toggle(
    conn: &zbus::Connection,
    node: &AccessibleProxy<'_>,
    role: glass_core::AxRole,
    target_on: bool,
    id: u32,
) -> Result<()> {
    let flag = if role == glass_core::AxRole::ToggleButton {
        atspi_common::State::Pressed
    } else {
        atspi_common::State::Checked
    };
    if node.get_state().await.map_err(bus_err)?.contains(flag) == target_on {
        return Ok(()); // already in the desired state
    }
    if !invoke_activating_action(conn, node).await {
        // No toggle action (e.g. a GTK4 GtkCheckButton exposes none) — can't set it
        // through accessibility; the caller should drive it with click_element.
        return Err(GlassError::AxElementNotEditable(id));
    }
    // Poll until the toolkit applies it; a no-op activation never converges.
    for _ in 0..6 {
        tokio::time::sleep(Duration::from_millis(120)).await;
        if node.get_state().await.map_err(bus_err)?.contains(flag) == target_on {
            return Ok(());
        }
    }
    Err(GlassError::AxValueNotApplied(id))
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
async fn read_value(
    proxy: &AccessibleProxy<'_>,
    conn: &zbus::Connection,
    role: glass_core::AxRole,
) -> Option<String> {
    use glass_core::AxRole::*;
    let dest = proxy.inner().destination().to_owned();
    let path = proxy.inner().path().to_owned();
    match role {
        TextField | TextArea | ComboBox => {
            let text = atspi::proxy::text::TextProxy::builder(conn)
                .destination(dest)
                .ok()?
                .path(path)
                .ok()?
                .build()
                .await
                .ok()?;
            let n = text.character_count().await.ok()?;
            text.get_text(0, n).await.ok().and_then(nonempty)
        }
        Slider | SpinButton | ProgressBar => {
            let val = atspi::proxy::value::ValueProxy::builder(conn)
                .destination(dest)
                .ok()?
                .path(path)
                .ok()?
                .build()
                .await
                .ok()?;
            val.current_value().await.ok().map(|v| v.to_string())
        }
        _ => None,
    }
}

fn nonempty(s: String) -> Option<String> {
    (!s.is_empty()).then_some(s)
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
