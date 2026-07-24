//! `ServiceA11y` — the on-device-AccessibilityService a11y reader. Talks the `tree`/`action`
//! line-JSON protocol to `glass-a11y.apk` over an `adb forward`ed socket, and maps the live
//! `AccessibilityNodeInfo` tree (sent as JSON) into glass's `AxTree`.

use std::sync::Mutex;

use serde_json::{json, Value};

use glass_core::accessibility::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxStates, AxTarget, AxTree,
    TruncationLimit, WalkBudget, WalkLimits,
};
use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result};

use crate::axmap::class_to_role;
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
    let text = v.get("text").and_then(Value::as_str);
    let desc = v.get("desc").and_then(Value::as_str);
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
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
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
        // name: the element's own text label, falling back to content-description.
        // value: editable text content only (content-description is not user-entered text).
        name: text.or(desc).map(str::to_string),
        value: text.map(str::to_string),
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
    let root = json_to_node(tree, win, 0, &mut budget)?;
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

    /// Run a request, transparently reconnecting once if the socket dropped.
    /// Mirrors `AgentClient::call`: lock, try, reconnect on `(e, true)`, retry.
    fn call(&self, req: Value) -> Result<Value> {
        let mut conn = self
            .conn
            .lock()
            .map_err(|_| GlassError::Backend("a11y service client lock poisoned".into()))?;
        match conn.call(req.clone()) {
            Ok(v) => Ok(v),
            Err((e, false)) => Err(e),
            Err((_, true)) => {
                // The service's accept loop accepts a fresh connection after a drop.
                *conn = Conn::open(self.port)?;
                conn.call(req).map_err(|(e, _)| e)
            }
        }
    }

    fn tree(&self, package: &str) -> Result<Value> {
        let r = self.call(json!({"op": "tree", "package": package}))?;
        r.get("tree")
            .cloned()
            .ok_or_else(|| GlassError::AccessibilityUnavailable("no tree in response".into()))
    }

    fn action(&self, ref_id: u32, action: &str, text: Option<&str>) -> Result<()> {
        let mut req = json!({"op": "action", "ref": ref_id, "action": action});
        if let Some(t) = text {
            req["text"] = json!(t);
        }
        self.call(req).map(|_| ())
    }

    pub fn ping(&self) -> Result<()> {
        self.call(json!({"op": "ping"})).map(|_| ())
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
}

impl Accessibility for ServiceA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let tree = self.client.tree(&self.package)?;
        tree_from_json(&tree, &ctx.window, ctx.limits)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        // Guard: re-snapshot and verify the ref still points at the same editable element
        // (role+name+bounds) before acting — the same drift protection as AndroidA11y::set_value.
        let tree = {
            let mut t = self.snapshot(ctx)?;
            t.assign_ids();
            t
        };
        let node = tree
            .find(target.id)
            .ok_or(GlassError::AxElementNotFound(target.id.0))?;
        if !target.matches(node.role, node.name.as_deref())
            || !target.bounds_consistent(node.bounds, 8)
        {
            return Err(GlassError::AxElementChanged(target.id.0));
        }
        if !node.states.editable {
            return Err(GlassError::AxElementNotEditable(target.id.0));
        }
        self.client.action(target.id.0, "set_text", Some(text))?;
        // Verify the value actually took. ACTION_SET_TEXT returns success but silently no-ops when
        // *replacing* existing text in a Compose field, so a bare Ok could lie (glass forbids silent
        // fallbacks). The set is async (Compose recompose → a11y update), so poll briefly for the
        // value to land; error honestly only on timeout.
        let deadline = std::time::Instant::now() + std::time::Duration::from_secs(2);
        loop {
            let mut after = self.snapshot(ctx)?;
            after.assign_ids();
            let got = after.find(target.id).and_then(|n| n.value.clone());
            // An empty field reports no value (None), not Some(""), so compare against "".
            if got.as_deref().unwrap_or("") == text {
                return Ok(());
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
        if let Ok(mut g) = self.state.lock() {
            if let Some(a) = g.take() {
                let adb = match &a.serial {
                    Some(s) => Adb::from_env().with_serial(s.clone()),
                    None => Adb::from_env(),
                };
                restore_a11y(&adb, &a.prior_enabled, &a.prior_a11y_enabled, a.port);
            }
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
                {"ref": 1, "class": "android.widget.EditText", "text": "Email",
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
        assert_eq!(email.name.as_deref(), Some("Email"));
        assert!(email.states.editable);
        assert_eq!(email.bounds.unwrap().y, 100); // window-relative: 200 - win.y 100
        let save = t.find(AxNodeId(2)).unwrap();
        assert_eq!(save.role, AxRole::Button);
        assert_eq!(save.name.as_deref(), Some("Save"));
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
}
