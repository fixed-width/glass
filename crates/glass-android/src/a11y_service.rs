//! `ServiceA11y` — the on-device-AccessibilityService a11y reader. Talks the `tree`/`action`
//! line-JSON protocol to `glass-a11y.apk` over an `adb forward`ed socket, and maps the live
//! `AccessibilityNodeInfo` tree (sent as JSON) into glass's `AxTree`.

use std::sync::Mutex;

use serde_json::{Value, json};

use glass_core::accessibility::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree,
    TruncationLimit, WalkBudget, WalkLimits,
};
use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result};

use crate::axmap::{LabelInputs, class_to_role, labels};
use crate::conn::Conn;

/// Map one device `tree` JSON node (+descendants) into an `AxNode`, converting screen bounds to
/// window-relative. Ids are left `AxNodeId(0)`; the core's `AxTree::assign_ids` numbers them
/// pre-order (root = 0).
///
/// INVARIANT: `AxNodeId(n)` equals the device's `ref` n. Both sides number the *same* node set in
/// the *same* pre-order: the device assigns `ref` while walking its adapted tree, sends that tree
/// as JSON `children` (in order), and this mapper recurses `children` in order without skipping or
/// reordering — a node with malformed/missing bounds errors the whole snapshot rather than being
/// dropped (which would shift every later id). So `set_value` can send `target.id.0` as the device
/// `ref` and hit the right node. Keep both walks pre-order if either side changes.
fn json_to_node(
    v: &Value,
    win: &WindowGeometry,
    depth: usize,
    budget: &mut WalkBudget,
) -> Result<AxNode> {
    budget.visit();
    let cls = v.get("class").and_then(Value::as_str).unwrap_or("");
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    // The device agent omits an empty text/desc rather than sending `""`, so both arrive as
    // `None` here; `labels` judges a blank one absent either way.
    let text = v.get("text").and_then(Value::as_str);
    let desc = v.get("desc").and_then(Value::as_str);
    // Both keys are absent (not null) on an older companion; `get` returns `None` either way, so
    // no version check is needed to stay compatible with it.
    let resource_id = v.get("resource_id").and_then(Value::as_str);
    let hint = v.get("hint").and_then(Value::as_str);
    let (name, value, description) = labels(LabelInputs {
        text,
        desc,
        resource_id,
        hint,
        editable: flag("editable"),
    });
    let b = v
        .get("bounds")
        .ok_or_else(|| GlassError::AccessibilityUnavailable("node missing bounds".into()))?;
    // Clamp rather than error: a live a11y tree legitimately contains degenerate/off-screen rects
    // (zero or inverted w/h out of `getBoundsInScreen`), so erroring would fail the whole snapshot
    // on one odd node. Negative w/h clamp to 0; values outside the int range clamp to its bounds.
    let bi = |k: &str| -> i32 {
        b.get(k)
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    let bu = |k: &str| -> u32 {
        b.get(k)
            .and_then(Value::as_i64)
            .unwrap_or(0)
            .clamp(0, i64::from(u32::MAX)) as u32
    };
    let (x, y, w, h) = (bi("x"), bi("y"), bu("w"), bu("h"));
    // Recursion is bounded by `budget` (depth, node count, siblings per level), so a
    // pathologically deep or wide device tree cannot blow the stack or the token budget.
    // The child array is resolved before either bound is consulted: a childless node must
    // never be reported truncated for declining to explore a list that was already empty.
    let children = match v.get("children").and_then(Value::as_array) {
        None => vec![],
        Some(arr) if arr.is_empty() => vec![],
        Some(_) if budget.depth_exhausted(depth) => {
            budget.hit(TruncationLimit::Depth);
            vec![]
        }
        Some(_) if budget.nodes_exhausted() => {
            budget.hit(TruncationLimit::Nodes);
            vec![]
        }
        Some(arr) => {
            let mut out = Vec::new();
            for (i, c) in arr.iter().enumerate() {
                // Checked before processing each child (not after) so the child that merely
                // completes the tree doesn't get mistaken for one the walk declined to visit.
                if budget.nodes_exhausted() {
                    budget.hit(TruncationLimit::Nodes);
                    break;
                }
                if i >= budget.max_siblings() {
                    budget.hit(TruncationLimit::Siblings);
                    break;
                }
                out.push(json_to_node(c, win, depth + 1, budget)?);
            }
            out
        }
    };
    Ok(AxNode {
        id: AxNodeId(0),
        role: class_to_role(cls),
        raw_role: cls.to_string(),
        name,
        // State-description stays unread — the device protocol doesn't carry it.
        description,
        value,
        states: AxStates {
            enabled: flag("enabled"),
            editable: flag("editable"),
            // Android "focusable" is keyboard-only; map isClickable -> focusable as the actability proxy.
            focusable: flag("clickable"),
            visible: true,
            // The companion carries isCheckable/isChecked (authoritative, unlike the baseline
            // uiautomator reader), so surface them directly; `AxStates::active()` and the
            // Checked/Unchecked `state_pred`s gate `checked` on `checkable`.
            checkable: flag("checkable"),
            checked: flag("checked"),
            ..Default::default()
        },
        bounds: Some(AxRect {
            x: x - win.x,
            y: y - win.y,
            width: w,
            height: h,
        }),
        children,
    })
}

/// Build the `AxTree` from a device `tree` response value (the `"tree"` object).
pub(crate) fn tree_from_json(
    tree: &Value,
    win: &WindowGeometry,
    limits: WalkLimits,
) -> Result<AxTree> {
    let mut budget = WalkBudget::with_limits(limits);
    let mut root = json_to_node(tree, win, 0, &mut budget)?;
    // The device answers with the root of the ACTIVE WINDOW, so this node is the window —
    // whatever layout class it happens to carry. Say so, for the same reason the
    // uiautomator reader wraps its dump in a Window root: both Android readers have to
    // agree about the root, or a `role:` selector written against one misses on the other.
    // The node itself is untouched — no synthetic wrapper — because `AxNodeId(n)` is the
    // device's `ref` n and an extra node would shift every id `set_value` addresses by.
    // `raw_role` keeps the device's class on the node, so anything that reads the node itself
    // — the role histogram, a structured client — still has it. It no longer reaches the
    // outline, though: the outline names a native token only for an element glass has no role
    // for, and this one is now a Window.
    root.role = AxRole::Window;
    let mut tree = AxTree::new(root);
    tree.truncated = budget.truncation();
    Ok(tree)
}

/// Line-JSON client to the on-device a11y service (mirrors `AgentClient`).
pub struct ServiceClient {
    conn: Mutex<Conn>,
    port: u16,
}

impl ServiceClient {
    pub fn connect(port: u16) -> Result<ServiceClient> {
        let conn = Conn::open(port)?;
        Ok(ServiceClient {
            conn: Mutex::new(conn),
            port,
        })
    }

    /// Run a request, transparently reconnecting once if the socket dropped. The bool is
    /// `Conn::call`'s: true for a transport failure, false for a refusal the device sent back.
    fn call(&self, req: Value) -> std::result::Result<Value, (GlassError, bool)> {
        let mut conn = self.conn.lock().map_err(|_| {
            (
                GlassError::Backend("a11y service client lock poisoned".into()),
                false,
            )
        })?;
        match conn.call(req.clone()) {
            Ok(v) => Ok(v),
            Err((e, false)) => Err((e, false)),
            Err((_, true)) => {
                // The service's accept loop accepts a fresh connection after a drop.
                *conn = Conn::open(self.port).map_err(|e| (e, true))?;
                conn.call(req)
            }
        }
    }

    fn tree(&self, package: &str) -> Result<Value> {
        let r = self
            .call(json!({"op": "tree", "package": package}))
            .map_err(|(e, _)| e)?;
        r.get("tree")
            .cloned()
            .ok_or_else(|| GlassError::AccessibilityUnavailable("no tree in response".into()))
    }

    /// [`Self::action`], keeping `call`'s transport flag for callers that must not retry a
    /// possibly-dispatched action by another route.
    fn action_result(
        &self,
        ref_id: u32,
        action: &str,
        text: Option<&str>,
    ) -> std::result::Result<(), (GlassError, bool)> {
        let mut req = json!({"op": "action", "ref": ref_id, "action": action});
        if let Some(t) = text {
            req["text"] = json!(t);
        }
        self.call(req).map(|_| ())
    }

    fn action(&self, ref_id: u32, action: &str, text: Option<&str>) -> Result<()> {
        self.action_result(ref_id, action, text).map_err(|(e, _)| e)
    }

    pub fn ping(&self) -> Result<()> {
        self.call(json!({"op": "ping"}))
            .map(|_| ())
            .map_err(|(e, _)| e)
    }
}

/// The Accessibility reader backed by the on-device service. `package` is the target app.
pub struct ServiceA11y {
    client: ServiceClient,
    package: String,
}

impl ServiceA11y {
    pub fn new(client: ServiceClient, package: String) -> Self {
        Self { client, package }
    }

    /// Poll until `chosen`'s `checked` leaves `was` — a Compose recompose reaches the tree a
    /// beat after the action, so acknowledgement is not proof; `set_value` polls for the same
    /// reason.
    fn wait_for_flip(
        &mut self,
        ctx: &AxContext,
        target: u32,
        chosen: AxNodeId,
        was: bool,
    ) -> Result<()> {
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut after = self.snapshot(ctx)?;
            after.assign_ids();
            if after.find(chosen).is_some_and(|n| n.states.checked != was) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(flip_timeout(target, chosen, was));
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }
}

impl Accessibility for ServiceA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let tree = self.client.tree(&self.package)?;
        tree_from_json(&tree, &ctx.window, ctx.limits)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        // Guard: re-snapshot and verify the ref still points at the same editable element before
        // acting. Shared with `AndroidA11y::set_value`, so both readers refuse the same drift.
        let tree = {
            let mut t = self.snapshot(ctx)?;
            t.assign_ids();
            t
        };
        crate::a11y::editable_target(&tree, target)?;
        self.client.action(target.id.0, "set_text", Some(text))?;
        // Verify the value actually took. ACTION_SET_TEXT returns success but silently no-ops when
        // *replacing* existing text in a Compose field, so a bare Ok could lie (glass forbids silent
        // fallbacks). The set is async (Compose recompose → a11y update), so poll briefly for the
        // value to land; error honestly only on timeout.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut after = self.snapshot(ctx)?;
            after.assign_ids();
            let node = after.find(target.id);
            let got = node.and_then(|n| n.value.clone());
            // An empty field reports no value (None), not Some(""), so compare against "".
            if got.as_deref().unwrap_or("") == text {
                return Ok(());
            }
            // A field that has stopped reporting `editable` — a submit collapsing it to a display
            // row, focus lost — also reports no value, which reads exactly like a write that never
            // landed. Spending the rest of the budget to then blame the write would send the
            // caller to clear a field that is already correct and to switch backends for nothing.
            // `AndroidA11y`'s `verify_write` re-checks the same flag for the same reason.
            if node.is_some_and(|n| !n.states.editable) {
                return Err(GlassError::AccessibilityUnavailable(format!(
                    "set_value on element {} was sent, but the element no longer reports itself \
                     editable, so its value cannot be read back; re-snapshot to see what it holds \
                     rather than retyping",
                    target.id.0
                )));
            }
            if std::time::Instant::now() >= deadline {
                return Err(GlassError::Backend(format!(
                    "set_value on element {} did not take (field is {got:?}, wanted {text:?}); a \
                     Compose field that already holds text can't be replaced via ACTION_SET_TEXT — \
                     clear it first or unset GLASS_ANDROID_A11Y_APK to use the uiautomator backend",
                    target.id.0
                )));
            }
            std::thread::sleep(std::time::Duration::from_millis(150));
        }
    }

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<()> {
        let tree = {
            let mut t = self.snapshot(ctx)?;
            t.assign_ids();
            t
        };
        let node = actuable_node(&tree, target)?;
        let chosen = node.id;
        if !node.states.enabled {
            return Err(disabled_error(target.id.0, chosen));
        }
        let (checkable, was) = (node.states.checkable, node.states.checked);
        self.client
            .action_result(chosen.0, "click", None)
            .map_err(|(e, transport)| action_error(target.id.0, &e, transport))?;
        if checkable {
            return self.wait_for_flip(ctx, target.id.0, chosen, was);
        }
        Ok(())
    }
}

use crate::adb::Adb;
use std::sync::Arc;

const SERVICE_COMPONENT: &str =
    "com.fixedwidth.glassa11y/com.fixedwidth.glassa11y.GlassA11yService";
const SERVICE_PACKAGE: &str = "com.fixedwidth.glassa11y";
const SOCKET: &str = "glass-a11y";

/// True when an `adb install` failure is the "existing package signed differently" case
/// that only an uninstall can clear (e.g. a release APK over a local debug build).
fn is_signature_mismatch(err: &str) -> bool {
    err.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE") || err.contains("signatures do not match")
}

/// Install the service APK, recovering from a signature mismatch. glass owns this package
/// (install → enable → teardown, no meaningful user state), so when a differently-signed
/// build is already present it removes the stale copy and installs fresh rather than failing.
fn install_service(adb: &Adb, apk: &str) -> Result<()> {
    match adb.run(["install", "-r", apk]) {
        Ok(_) => Ok(()),
        Err(e) if is_signature_mismatch(&e.to_string()) => {
            eprintln!(
                "glass-a11y: replacing a differently-signed existing install of {SERVICE_PACKAGE}"
            );
            adb.run(["uninstall", SERVICE_PACKAGE])?;
            adb.run(["install", "-r", apk]).map(|_| ())
        }
        Err(e) => Err(e),
    }
}

/// `GLASS_ANDROID_A11Y_APK`, else `glass-a11y.apk` dropped in the glass data dir or next
/// to the `glass-mcp` binary; `None` when disabled via `GLASS_ANDROID_A11Y=off`.
pub fn a11y_apk(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if get("GLASS_ANDROID_A11Y")
        .map(|v| v.eq_ignore_ascii_case("off"))
        .unwrap_or(false)
    {
        return None;
    }
    let mut dirs = crate::sdk::artifact_data_dirs(get);
    dirs.extend(crate::sdk::exe_dir());
    crate::sdk::resolve_artifact(
        "GLASS_ANDROID_A11Y_APK",
        "glass-a11y.apk",
        &dirs,
        get,
        &|p| p.is_file(),
    )
}

struct Active {
    serial: Option<String>,
    port: u16,
    prior_enabled: String,
    prior_a11y_enabled: String,
}

/// Owns the installed+enabled state so the shutdown hook can restore it. Cloneable (shared
/// `Arc<Mutex<Option<Active>>>`) like `AgentRegistry`.
#[derive(Clone, Default)]
pub struct A11yServiceRegistry {
    state: Arc<std::sync::Mutex<Option<Active>>>,
}

impl A11yServiceRegistry {
    pub fn new() -> Self {
        Self::default()
    }

    /// Install + enable the service on `adb`'s device, forward a port, connect, ping. Returns a
    /// connected `ServiceClient`. The apk path is resolved from env by the caller.
    pub fn ensure(&self, adb: &Adb, apk: &str) -> Result<ServiceClient> {
        install_service(adb, apk)?;
        let get = |k: &str| {
            adb.run(["shell", "settings", "get", "secure", k])
                .unwrap_or_default()
        };
        let prior = get("enabled_accessibility_services");
        let prior = prior.trim();
        let prior = if prior == "null" { "" } else { prior };
        // Save the global flag too, so teardown restores the device's prior a11y state exactly.
        let prior_a11y = get("accessibility_enabled");
        let prior_a11y = prior_a11y.trim();
        let prior_a11y = if prior_a11y == "null" || prior_a11y.is_empty() {
            "0"
        } else {
            prior_a11y
        };
        let want = if prior.is_empty() {
            SERVICE_COMPONENT.to_string()
        } else if prior.split(':').any(|s| s == SERVICE_COMPONENT) {
            prior.to_string()
        } else {
            format!("{prior}:{SERVICE_COMPONENT}")
        };
        adb.run([
            "shell",
            "settings",
            "put",
            "secure",
            "enabled_accessibility_services",
            &want,
        ])?;
        adb.run([
            "shell",
            "settings",
            "put",
            "secure",
            "accessibility_enabled",
            "1",
        ])?;
        let out = adb.run(["forward", "tcp:0", &format!("localabstract:{SOCKET}")])?;
        let port = crate::agent::parse_forward_port(&out)
            .ok_or_else(|| GlassError::Backend(format!("adb forward gave no port: {out:?}")))?;
        // From here, a failure must roll back the settings + forward, else a failed `ensure` leaks
        // an enabled service and a forward slot.
        let client = match wait_for_service(port) {
            Ok(c) => c,
            Err(e) => {
                restore_a11y(adb, prior, prior_a11y, port);
                return Err(e);
            }
        };
        *self.state.lock().unwrap() = Some(Active {
            serial: adb.serial().map(str::to_string),
            port,
            prior_enabled: prior.to_string(),
            prior_a11y_enabled: prior_a11y.to_string(),
        });
        Ok(client)
    }

    /// Restore the device's prior accessibility state and remove the forward. Best-effort,
    /// idempotent. No process to kill (disabling unbinds the service).
    pub fn shutdown(&self) {
        if let Ok(mut g) = self.state.lock()
            && let Some(a) = g.take()
        {
            let adb = match &a.serial {
                Some(s) => Adb::from_env().with_serial(s.clone()),
                None => Adb::from_env(),
            };
            restore_a11y(&adb, &a.prior_enabled, &a.prior_a11y_enabled, a.port);
        }
    }
}

/// Restore `enabled_accessibility_services` + `accessibility_enabled` to their prior values and
/// remove the forwarded port. Shared by `shutdown` and the failed-`ensure` rollback. Best-effort.
fn restore_a11y(adb: &Adb, prior_enabled: &str, prior_a11y_enabled: &str, port: u16) {
    if prior_enabled.is_empty() {
        // `settings put ... ""` errors ("Bad arguments"); delete to clear the list instead.
        let _ = adb.run([
            "shell",
            "settings",
            "delete",
            "secure",
            "enabled_accessibility_services",
        ]);
    } else {
        let _ = adb.run([
            "shell",
            "settings",
            "put",
            "secure",
            "enabled_accessibility_services",
            prior_enabled,
        ]);
    }
    let _ = adb.run([
        "shell",
        "settings",
        "put",
        "secure",
        "accessibility_enabled",
        prior_a11y_enabled,
    ]);
    let _ = adb.run(["forward", "--remove", &format!("tcp:{port}")]);
}

/// Connect to the forwarded service port, retrying briefly while the service binds + listens.
pub(crate) fn wait_for_service(port: u16) -> Result<ServiceClient> {
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
    loop {
        match ServiceClient::connect(port).and_then(|c| c.ping().map(|_| c)) {
            Ok(c) => return Ok(c),
            Err(e) if std::time::Instant::now() >= deadline => return Err(e),
            Err(_) => std::thread::sleep(std::time::Duration::from_millis(150)),
        }
    }
}

/// The node an `invoke` should actuate on behalf of `target`.
///
/// A Compose button's label and its clickable node are different nodes: the touch-target
/// carries no name and the named child carries no `ACTION_CLICK`. When the target itself
/// advertises no click, climb to the nearest node that does and encloses it — the same node a
/// tap at the target's centre would have been handled by.
fn actuable_node<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    crate::a11y::fingerprinted(tree, target)?;
    let mut path = Vec::new();
    if !path_to(&tree.root, target.id, &mut path) {
        return Err(GlassError::AxElementNotFound(target.id.0));
    }
    let want = path.last().expect("path_to leaves the target last").bounds;
    path.iter()
        .rev()
        .find(|n| n.states.focusable && (n.id == target.id || encloses(n.bounds, want)))
        .copied()
        .ok_or(GlassError::AxActionUnavailable(target.id.0))
}

/// The root-to-`id` path, inclusive of both ends. False when `id` is not in this subtree,
/// leaving `out` as it was found.
fn path_to<'a>(node: &'a AxNode, id: AxNodeId, out: &mut Vec<&'a AxNode>) -> bool {
    out.push(node);
    if node.id == id {
        return true;
    }
    for c in &node.children {
        if path_to(c, id, out) {
            return true;
        }
    }
    out.pop();
    false
}

/// Whether `outer` fully contains `inner`. A node without bounds encloses nothing — the climb
/// must not reach past a node whose geometry it cannot check.
fn encloses(outer: Option<AxRect>, inner: Option<AxRect>) -> bool {
    let (Some(o), Some(i)) = (outer, inner) else {
        return false;
    };
    i64::from(o.x) <= i64::from(i.x)
        && i64::from(o.y) <= i64::from(i.y)
        && i64::from(o.x) + i64::from(o.width) >= i64::from(i.x) + i64::from(i.width)
        && i64::from(o.y) + i64::from(o.height) >= i64::from(i.y) + i64::from(i.height)
}

/// The error for a target the tree says is disabled. Deliberately not `AxActionUnavailable`:
/// a pointer click would tap a control that cannot act and report success.
fn disabled_error(target: u32, chosen: AxNodeId) -> GlassError {
    GlassError::AxActionFailed(
        target,
        format!(
            "element {} is disabled, so its click action was refused; no pointer click was \
             synthesized on top of it",
            chosen.0
        ),
    )
}

/// Map an `action` failure onto the invoke contract. Neither arm is fallback-eligible: a
/// refusal may still have reached the toolkit, and a lost answer says nothing either way.
fn action_error(target: u32, e: &GlassError, transport: bool) -> GlassError {
    if transport {
        GlassError::AccessibilityUnavailable(format!("a11y service action call failed: {e}"))
    } else {
        GlassError::AxActionFailed(target, e.to_string())
    }
}

/// The error for a toggle whose action was acknowledged but whose state never moved.
fn flip_timeout(target: u32, chosen: AxNodeId, was: bool) -> GlassError {
    GlassError::AxActionFailed(
        target,
        format!(
            "the click action on element {} was accepted but its checked state stayed {was}; \
             the control did not toggle",
            chosen.0
        ),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::AxRole;
    use serde_json::json;

    fn win() -> WindowGeometry {
        WindowGeometry {
            x: 0,
            y: 100,
            width: 1080,
            height: 2300,
        }
    }

    #[test]
    fn signature_mismatch_detected() {
        assert!(is_signature_mismatch(
            "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package signatures do not match newer version; ignoring!]"
        ));
        assert!(is_signature_mismatch(
            "signatures do not match newer version"
        ));
        assert!(!is_signature_mismatch(
            "Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"
        ));
        assert!(!is_signature_mismatch("error: device offline"));
    }

    #[test]
    fn maps_json_tree_to_window_relative_axtree() {
        let v = json!({
            "ref": 0, "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false,
            "children": [
                {"ref": 1, "class": "android.widget.EditText", "text": "joe@x.com", "desc": "Email",
                 "bounds": {"x": 40, "y": 200, "w": 600, "h": 120},
                 "editable": true, "clickable": true, "enabled": true, "scrollable": false},
                {"ref": 2, "class": "android.widget.Button", "desc": "Save",
                 "bounds": {"x": 40, "y": 360, "w": 200, "h": 100},
                 "editable": false, "clickable": true, "enabled": true, "scrollable": false}
            ]
        });
        let mut t = tree_from_json(&v, &win(), WalkLimits::DEFAULT).unwrap();
        t.assign_ids();
        assert_eq!(t.count, 3);
        let email = t.find(AxNodeId(1)).unwrap();
        assert_eq!(email.role, AxRole::TextField);
        assert_eq!(email.name.as_deref(), Some("Email")); // editable: name follows desc, not text
        assert_eq!(email.value.as_deref(), Some("joe@x.com"));
        assert!(email.states.editable);
        assert_eq!(email.bounds.unwrap().y, 100); // window-relative: 200 - win.y 100
        let save = t.find(AxNodeId(2)).unwrap();
        assert_eq!(save.role, AxRole::Button);
        assert_eq!(save.name.as_deref(), Some("Save"));
    }

    /// One device `tree` response: a root layout with two children, shaped like the real
    /// protocol (bounds in screen coordinates, flags as bools).
    fn device_tree() -> Value {
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400},
            "children": [
                {"class": "android.widget.TextView", "text": "Settings",
                 "bounds": {"x": 0, "y": 100, "w": 400, "h": 60}, "children": []},
                {"class": "android.widget.Button", "desc": "Save",
                 "bounds": {"x": 0, "y": 200, "w": 400, "h": 60}, "children": []}
            ]
        })
    }

    #[test]
    fn the_root_is_a_window_like_the_uiautomator_reader() {
        let win = WindowGeometry {
            x: 0,
            y: 0,
            width: 1080,
            height: 2400,
        };
        let mut tree = tree_from_json(&device_tree(), &win, WalkLimits::DEFAULT).expect("builds");
        tree.assign_ids();
        // The service asks the device for the active window's root, so that node IS the
        // window — and both Android readers must agree on it, or one `role:` selector
        // cannot address the root on both.
        assert_eq!(tree.root.role, AxRole::Window);
        // The device's own class stays in raw_role, where anything reading the node — the
        // role histogram, a structured client — still finds it. The outline will not print
        // it: that only names a token for an element with no mapped role.
        assert_eq!(tree.root.raw_role, "android.widget.FrameLayout");
        // Rooting must not add, drop or reorder a node: ids are the device's refs, and
        // set_value addresses by them.
        assert_eq!(tree.root.id, AxNodeId(0));
        assert_eq!(tree.count, 3);
        assert_eq!(tree.root.children[0].id, AxNodeId(1));
        assert_eq!(tree.root.children[0].role, AxRole::Label);
        assert_eq!(tree.root.children[1].id, AxNodeId(2));
        assert_eq!(tree.root.children[1].role, AxRole::Button);
    }

    #[test]
    fn reads_checkable_and_checked_from_json() {
        // The companion now carries isCheckable/isChecked; surface them on the node's states.
        let on = json!({
            "class": "android.widget.CheckBox", "bounds": {"x": 0, "y": 100, "w": 10, "h": 10},
            "checkable": true, "checked": true
        });
        let n = json_to_node(&on, &win(), 0, &mut WalkBudget::new()).unwrap();
        assert!(
            n.states.checkable && n.states.checked,
            "on checkbox → checkable + checked"
        );
        let plain = json!({
            "class": "android.widget.TextView", "bounds": {"x": 0, "y": 100, "w": 10, "h": 10}
        });
        let p = json_to_node(&plain, &win(), 0, &mut WalkBudget::new()).unwrap();
        assert!(
            !p.states.checkable && !p.states.checked,
            "a node with no checkable/checked keys stays false"
        );
    }

    /// Device JSON for a root with `n` flat children, each a distinctly-named Button.
    fn wide_device_json(n: usize) -> Value {
        let kids: Vec<Value> = (0..n)
            .map(|i| {
                json!({
                    "class": "android.widget.Button",
                    "text": format!("btn{i}"),
                    "bounds": {"x": 0, "y": i, "w": 10, "h": 10},
                    "children": []
                })
            })
            .collect();
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 0, "w": 100, "h": 100},
            "children": kids
        })
    }

    #[test]
    fn truncation_stops_the_walk_and_never_shifts_surviving_ids() {
        // The device numbers refs in pre-order over the SAME node set. If truncation dropped
        // nodes from the middle instead of stopping at the end, every later id would shift and
        // set_value would write to the wrong element.
        let json = wide_device_json(glass_core::MAX_NODES + 50);
        let mut tree = tree_from_json(&json, &win(), WalkLimits::DEFAULT).expect("tree parses");
        tree.assign_ids();

        assert!(tree.truncated.is_some(), "the node cap must have been hit");
        // `tree_from_json` maps the device root directly (no synthetic wrapper), so the
        // FrameLayout itself is id 0 and child at array index i is id i+1 — every surviving
        // child must still carry the name matching its own id-derived index.
        let third = tree.find(AxNodeId(3)).expect("id 3 survives");
        assert_eq!(third.name.as_deref(), Some("btn2"));
    }

    #[test]
    fn a_complete_tree_of_exactly_max_nodes_reports_no_truncation() {
        // `tree_from_json` walks the device root itself (no synthetic wrapper), so root (1) +
        // MAX_NODES-1 flat children = MAX_NODES nodes walked in total, and the LAST child is
        // what pushes the running count to MAX_NODES. Nothing was declined, so this must NOT
        // be reported truncated (regression for the false-positive-at-the-cap bug).
        let json = wide_device_json(glass_core::MAX_NODES - 1);
        let mut tree = tree_from_json(&json, &win(), WalkLimits::DEFAULT).expect("tree parses");
        tree.assign_ids();
        assert_eq!(tree.count, glass_core::MAX_NODES);
        assert_eq!(tree.truncated, None);
    }

    #[test]
    fn a_tree_of_max_nodes_plus_one_still_reports_nodes_truncation() {
        // One more child than the complete case above: now there really is a node the walk
        // declines to visit, so the cap must still fire — proving the fix didn't just
        // disable it.
        let json = wide_device_json(glass_core::MAX_NODES);
        let tree = tree_from_json(&json, &win(), WalkLimits::DEFAULT).expect("tree parses");
        assert_eq!(
            tree.truncated.map(|t| t.limit),
            Some(TruncationLimit::Nodes)
        );
    }

    #[test]
    fn a_childless_node_at_the_spent_node_budget_records_no_truncation() {
        // A leaf with no "children" key, reached once the node budget is already spent, must
        // not be reported truncated merely for declining to explore an empty list.
        let leaf = json!({
            "class": "android.widget.TextView",
            "bounds": {"x": 0, "y": 0, "w": 10, "h": 10}
        });
        let mut budget = WalkBudget::new();
        for _ in 0..glass_core::MAX_NODES {
            budget.visit();
        }
        let _ = json_to_node(&leaf, &win(), 0, &mut budget).unwrap();
        assert!(budget.truncation().is_none());
    }

    #[test]
    fn degenerate_bounds_clamp_instead_of_erroring() {
        // A live a11y tree legitimately has zero/inverted rects; the mapper must clamp, not fail.
        let v = json!({
            "ref": 0, "class": "android.view.View",
            "bounds": {"x": -5, "y": 10, "w": -3, "h": 0},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false
        });
        let t = tree_from_json(&v, &win(), WalkLimits::DEFAULT)
            .expect("degenerate bounds must not error the snapshot");
        let b = t.root.bounds.unwrap();
        assert_eq!((b.width, b.height), (0, 0)); // negative/zero w/h clamp to 0
        assert_eq!((b.x, b.y), (-5, -90)); // window-relative: x -5-0, y 10-100
    }

    fn node_json(
        text: Option<&str>,
        desc: Option<&str>,
        resource_id: Option<&str>,
        hint: Option<&str>,
    ) -> Value {
        let mut v = json!({
            "class": "android.widget.Button",
            "bounds": {"x": 0, "y": 100, "w": 10, "h": 10},
            "enabled": true,
        });
        if let Some(t) = text {
            v["text"] = json!(t);
        }
        if let Some(d) = desc {
            v["desc"] = json!(d);
        }
        if let Some(r) = resource_id {
            v["resource_id"] = json!(r);
        }
        if let Some(h) = hint {
            v["hint"] = json!(h);
        }
        v
    }

    fn mapped(text: Option<&str>, desc: Option<&str>) -> AxNode {
        let mut budget = WalkBudget::new();
        json_to_node(&node_json(text, desc, None, None), &win(), 0, &mut budget).expect("maps")
    }

    /// [`mapped`], for an editable field; omits `resource_id`/`hint`, the shape an older
    /// companion sends.
    fn mapped_editable(text: Option<&str>, desc: Option<&str>) -> AxNode {
        let mut v = node_json(text, desc, None, None);
        v["class"] = json!("android.widget.EditText");
        v["editable"] = json!(true);
        let mut budget = WalkBudget::new();
        json_to_node(&v, &win(), 0, &mut budget).expect("maps")
    }

    /// [`mapped_editable`], additionally setting `resource_id` and `hint`.
    fn mapped_full(
        text: Option<&str>,
        desc: Option<&str>,
        resource_id: Option<&str>,
        hint: Option<&str>,
        editable: bool,
    ) -> AxNode {
        let mut v = node_json(text, desc, resource_id, hint);
        if editable {
            v["class"] = json!("android.widget.EditText");
            v["editable"] = json!(true);
        }
        let mut budget = WalkBudget::new();
        json_to_node(&v, &win(), 0, &mut budget).expect("maps")
    }

    #[test]
    fn the_content_description_a_text_displaced_becomes_the_description() {
        let node = mapped(Some("Save"), Some("Save changes"));
        // Non-editable: `text` wins the name here too, unchanged by glass#260.
        assert_eq!(node.name.as_deref(), Some("Save"));
        assert_eq!(node.description.as_deref(), Some("Save changes"));
        assert_eq!(
            node.value, None,
            "a Button's text is not user-entered content"
        );
    }

    #[test]
    fn a_content_description_that_became_the_name_is_not_repeated() {
        let node = mapped(None, Some("Bold"));
        assert_eq!(node.name.as_deref(), Some("Bold"));
        assert_eq!(
            node.description, None,
            "the desc IS the name here; printing it again would duplicate the label"
        );
    }

    #[test]
    fn a_desc_identical_to_the_text_is_not_a_description() {
        assert_eq!(mapped(Some("Save"), Some("Save")).description, None);
    }

    #[test]
    fn a_node_with_no_desc_has_no_description() {
        assert_eq!(mapped(Some("Save"), None).description, None);
    }

    #[test]
    fn an_editable_node_is_named_by_its_content_description_not_its_text() {
        // This reader used to name an editable node by `text` too (see `labels`'s doc for why
        // that breaks selectors).
        let node = mapped_editable(Some("joe@x.com"), Some("Email"));
        assert_eq!(node.name.as_deref(), Some("Email"));
        assert_eq!(node.value.as_deref(), Some("joe@x.com"));
        assert_eq!(node.description, None);
    }

    #[test]
    fn an_editable_node_with_no_desc_and_no_id_is_unnamed_not_named_by_its_contents() {
        // The device omits the key entirely for a field with no content description, and
        // `mapped_editable` omits `resource_id` too, so nothing is left to fall back to. Naming it
        // by `text` would move the name — and the fingerprint `set_value` re-walks against — on
        // every keystroke, so unnamed is the honest reading.
        let node = mapped_editable(Some("joe@x.com"), None);
        assert_eq!(node.name, None);
        assert_eq!(node.value.as_deref(), Some("joe@x.com"));
    }

    #[test]
    fn a_non_editable_nodes_text_is_its_name_and_not_also_its_value() {
        // This reader used to copy a label into `value` too, so a Label reported the same
        // string twice.
        let node = mapped(Some("Settings"), None);
        assert_eq!(node.name.as_deref(), Some("Settings"));
        assert_eq!(node.value, None);
    }

    #[test]
    fn an_editable_node_with_no_desc_is_named_by_its_view_id() {
        let node = mapped_full(
            Some("joe@x.com"),
            None,
            Some("com.x:id/email_field"),
            None,
            true,
        );
        assert_eq!(node.name.as_deref(), Some("email_field"));
        assert_eq!(node.value.as_deref(), Some("joe@x.com"));
    }

    #[test]
    fn an_editable_nodes_hint_becomes_its_description() {
        let node = mapped_full(
            None,
            None,
            Some("com.x:id/q"),
            Some("Search settings"),
            true,
        );
        assert_eq!(node.name.as_deref(), Some("q"));
        assert_eq!(node.description.as_deref(), Some("Search settings"));
    }

    #[test]
    fn an_editable_nodes_name_is_the_desc_not_the_raw_resource_id() {
        // The only fixture here with both `desc` and `resource_id` present, so it is what
        // pins the precedence between them. (`LabelInputs`'s named fields, not this test, are
        // what stop the call site transposing the two.) The older-companion path (neither key
        // sent) needs no dedicated test — every fixture above that omits them already
        // exercises it.
        let node = mapped_full(
            Some("joe@x.com"),
            Some("Email"),
            Some("com.x:id/email_field"),
            None,
            true,
        );
        assert_eq!(node.name.as_deref(), Some("Email"));
    }

    /// A device tree shaped like Compose's: the clickable touch-target `View` carries no name,
    /// and the label that names it is a child.
    fn compose_like() -> Value {
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false,
            "children": [
                {"class": "android.view.View",
                 "bounds": {"x": 60, "y": 480, "w": 210, "h": 130},
                 "editable": false, "clickable": true, "enabled": true, "scrollable": false,
                 "children": [
                    {"class": "android.widget.TextView", "text": "Save",
                     "bounds": {"x": 120, "y": 520, "w": 80, "h": 50},
                     "editable": false, "clickable": false, "enabled": true, "scrollable": false}
                 ]}
            ]
        })
    }

    fn built(v: &Value) -> AxTree {
        let mut t = tree_from_json(v, &win(), WalkLimits::DEFAULT).expect("maps");
        t.assign_ids();
        t
    }

    /// The target a caller would hold after selecting `id` from `tree`.
    fn target_for(tree: &AxTree, id: AxNodeId) -> AxTarget {
        let n = tree.find(id).expect("node is in the tree");
        AxTarget {
            id,
            role: n.role,
            name: n.name.clone(),
            bounds: n.bounds,
            value: n.value.clone(),
        }
    }

    #[test]
    fn a_named_label_climbs_to_the_clickable_node_that_encloses_it() {
        let t = built(&compose_like());
        let label = target_for(&t, AxNodeId(2));
        assert_eq!(label.name.as_deref(), Some("Save"));
        assert_eq!(actuable_node(&t, &label).unwrap().id, AxNodeId(1));
    }

    #[test]
    fn a_target_that_advertises_a_click_is_actuated_directly() {
        let t = built(&compose_like());
        let btn = target_for(&t, AxNodeId(1));
        assert_eq!(actuable_node(&t, &btn).unwrap().id, AxNodeId(1));
    }

    #[test]
    fn a_clickable_ancestor_that_does_not_enclose_the_target_is_not_climbed_to() {
        // Same shape, but the clickable node's box sits beside its label rather than around
        // it — the case where a tap at the label's centre would NOT have reached it.
        let mut v = compose_like();
        v["children"][0]["bounds"] = json!({"x": 600, "y": 480, "w": 210, "h": 130});
        let t = built(&v);
        let label = target_for(&t, AxNodeId(2));
        assert!(matches!(
            actuable_node(&t, &label),
            Err(GlassError::AxActionUnavailable(2))
        ));
    }

    #[test]
    fn a_path_with_no_clickable_node_reports_the_action_unavailable() {
        let mut v = compose_like();
        v["children"][0]["clickable"] = json!(false);
        let t = built(&v);
        let label = target_for(&t, AxNodeId(2));
        assert!(matches!(
            actuable_node(&t, &label),
            Err(GlassError::AxActionUnavailable(2))
        ));
    }

    #[test]
    fn action_unavailable_is_the_only_resolution_error_that_may_fall_back() {
        let mut v = compose_like();
        v["children"][0]["clickable"] = json!(false);
        let t = built(&v);
        let e = actuable_node(&t, &target_for(&t, AxNodeId(2))).unwrap_err();
        assert!(e.invoke_fallback_eligible(), "{e}");
    }

    #[test]
    fn a_target_whose_name_drifted_is_rejected_before_any_climb() {
        let t = built(&compose_like());
        let mut label = target_for(&t, AxNodeId(2));
        label.name = Some("Send".into());
        assert!(matches!(
            actuable_node(&t, &label),
            Err(GlassError::AxElementChanged(2))
        ));
    }

    #[test]
    fn a_drifted_target_must_not_fall_back_to_a_pointer_click() {
        let t = built(&compose_like());
        let mut label = target_for(&t, AxNodeId(2));
        label.name = Some("Send".into());
        let e = actuable_node(&t, &label).unwrap_err();
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    /// Two siblings under the root: the first is clickable and its bounds happen to enclose
    /// the second, but it does not contain the target in its subtree; the target is the second
    /// sibling, with no clickable node anywhere in its real ancestry (root nor itself).
    fn target_follows_a_rejected_clickable_sibling() -> Value {
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false,
            "children": [
                {"class": "android.view.View",
                 "bounds": {"x": 0, "y": 200, "w": 1080, "h": 2000},
                 "editable": false, "clickable": true, "enabled": true, "scrollable": false,
                 "children": []},
                {"class": "android.widget.TextView", "text": "Save",
                 "bounds": {"x": 40, "y": 1220, "w": 200, "h": 60},
                 "editable": false, "clickable": false, "enabled": true, "scrollable": false}
            ]
        })
    }

    /// Pins `path_to`'s backtrack: without `out.pop()` restoring the accumulator after the
    /// first (rejected) sibling, the reverse walk in `actuable_node` would still see that
    /// sibling's clickable node — whose bounds do enclose the target — as if it were on the
    /// target's real path, and wrongly return it instead of reporting no actuator exists.
    #[test]
    fn a_rejected_earlier_sibling_is_popped_before_trying_the_target_sibling() {
        let t = built(&target_follows_a_rejected_clickable_sibling());
        let target = target_for(&t, AxNodeId(2));
        assert!(matches!(
            actuable_node(&t, &target),
            Err(GlassError::AxActionUnavailable(2))
        ));
    }

    #[test]
    fn a_disabled_target_errors_instead_of_falling_back_to_a_pointer_click() {
        let e = disabled_error(2, AxNodeId(1));
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert!(
            e.to_string().contains("disabled"),
            "the message must say why: {e}"
        );
    }

    #[test]
    fn a_device_refusal_is_not_fallback_eligible() {
        // The device answered and refused. The call may have reached the toolkit, so a pointer
        // click on top of it could actuate the control twice.
        let inner = GlassError::Backend("agent: ACTION_CLICK refused".into());
        let e = action_error(2, &inner, false);
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert!(e.to_string().contains("ACTION_CLICK refused"), "{e}");
    }

    #[test]
    fn a_transport_failure_is_not_fallback_eligible_either() {
        let inner = GlassError::Backend("agent write: broken pipe".into());
        let e = action_error(2, &inner, true);
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    #[test]
    fn a_toggle_that_never_flipped_is_a_failure_not_a_missing_action() {
        let e = flip_timeout(2, AxNodeId(1), false);
        assert!(!e.invoke_fallback_eligible(), "{e}");
        // The distinction that matters to a caller: the action WAS accepted.
        assert!(e.to_string().contains("accepted"), "{e}");
        assert!(e.to_string().contains("did not toggle"), "{e}");
    }
}
