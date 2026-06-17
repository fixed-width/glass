//! `ServiceA11y` — the on-device-AccessibilityService a11y reader. Talks the `tree`/`action`
//! line-JSON protocol to `glass-a11y.apk` over an `adb forward`ed socket, and maps the live
//! `AccessibilityNodeInfo` tree (sent as JSON) into glass's `AxTree`.

use std::sync::Mutex;

use serde_json::{json, Value};

use glass_core::accessibility::{Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxStates, AxTarget, AxTree};
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
fn json_to_node(v: &Value, win: &WindowGeometry) -> Result<AxNode> {
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
        b.get(k).and_then(Value::as_i64).unwrap_or(0).clamp(i64::from(i32::MIN), i64::from(i32::MAX)) as i32
    };
    let bu = |k: &str| -> u32 {
        b.get(k).and_then(Value::as_i64).unwrap_or(0).clamp(0, i64::from(u32::MAX)) as u32
    };
    let (x, y, w, h) = (bi("x"), bi("y"), bu("w"), bu("h"));
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    // Recursion depth = a11y tree depth (shallow in practice; not hardened against adversarial nesting).
    let children = match v.get("children").and_then(Value::as_array) {
        Some(arr) => arr.iter().map(|c| json_to_node(c, win)).collect::<Result<Vec<_>>>()?,
        None => vec![],
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
            ..Default::default()
        },
        bounds: Some(AxRect { x: x - win.x, y: y - win.y, width: w, height: h }),
        children,
    })
}

/// Build the `AxTree` from a device `tree` response value (the `"tree"` object).
pub(crate) fn tree_from_json(tree: &Value, win: &WindowGeometry) -> Result<AxTree> {
    // count stays 0 until the caller runs AxTree::assign_ids (per the Accessibility trait contract).
    Ok(AxTree { root: json_to_node(tree, win)?, count: 0 })
}

/// Line-JSON client to the on-device a11y service (mirrors `AgentClient`).
pub struct ServiceClient {
    conn: Mutex<Conn>,
    port: u16,
}

impl ServiceClient {
    pub fn connect(port: u16) -> Result<ServiceClient> {
        let conn = Conn::open(port)?;
        Ok(ServiceClient { conn: Mutex::new(conn), port })
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
        tree_from_json(&tree, &ctx.window)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        // Guard: re-snapshot and verify the ref still points at the same editable element
        // (role+name+bounds) before acting — the same drift protection as AndroidA11y::set_value.
        let tree = {
            let mut t = self.snapshot(ctx)?;
            t.assign_ids();
            t
        };
        let node = tree.find(target.id).ok_or(GlassError::AxElementNotFound(target.id.0))?;
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
            if got.as_deref() == Some(text) {
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

use std::sync::Arc;
use crate::adb::Adb;

const SERVICE_COMPONENT: &str = "com.fixedwidth.glassa11y/com.fixedwidth.glassa11y.GlassA11yService";
const SOCKET: &str = "glass-a11y";

/// `GLASS_ANDROID_A11Y_APK` (path to the APK) when configured + not disabled.
pub fn a11y_apk(get: &dyn Fn(&str) -> Option<String>) -> Option<String> {
    if get("GLASS_ANDROID_A11Y").map(|v| v.eq_ignore_ascii_case("off")).unwrap_or(false) {
        return None;
    }
    get("GLASS_ANDROID_A11Y_APK").filter(|s| !s.is_empty())
}

struct Active { serial: Option<String>, port: u16, prior_enabled: String }

/// Owns the installed+enabled state so the shutdown hook can restore it. Cloneable (shared
/// `Arc<Mutex<Option<Active>>>`) like `AgentRegistry`.
#[derive(Clone, Default)]
pub struct A11yServiceRegistry { state: Arc<std::sync::Mutex<Option<Active>>> }

impl A11yServiceRegistry {
    pub fn new() -> Self { Self::default() }

    /// Install + enable the service on `adb`'s device, forward a port, connect, ping. Returns a
    /// connected `ServiceClient`. The apk path is resolved from env by the caller.
    pub fn ensure(&self, adb: &Adb, apk: &str) -> Result<ServiceClient> {
        adb.run(["install", "-r", apk])?;
        let prior = adb.run(["shell", "settings", "get", "secure", "enabled_accessibility_services"])
            .unwrap_or_default();
        let prior = prior.trim();
        let prior = if prior == "null" { "" } else { prior };
        let want = if prior.is_empty() { SERVICE_COMPONENT.to_string() }
                   else if prior.split(':').any(|s| s == SERVICE_COMPONENT) { prior.to_string() }
                   else { format!("{prior}:{SERVICE_COMPONENT}") };
        adb.run(["shell", "settings", "put", "secure", "enabled_accessibility_services", &want])?;
        adb.run(["shell", "settings", "put", "secure", "accessibility_enabled", "1"])?;
        let out = adb.run(["forward", "tcp:0", &format!("localabstract:{SOCKET}")])?;
        let port = crate::agent::parse_forward_port(&out)
            .ok_or_else(|| GlassError::Backend(format!("adb forward gave no port: {out:?}")))?;
        let client = wait_for_service(port)?; // connect + ping, retry briefly
        *self.state.lock().unwrap() = Some(Active {
            serial: adb.serial().map(str::to_string), port, prior_enabled: prior.to_string(),
        });
        Ok(client)
    }

    /// Restore the prior accessibility-services setting and remove the forward. Best-effort,
    /// idempotent. No process to kill (disabling unbinds the service).
    pub fn shutdown(&self) {
        if let Ok(mut g) = self.state.lock() {
            if let Some(a) = g.take() {
                let adb = match &a.serial {
                    Some(s) => Adb::from_env().with_serial(s.clone()),
                    None => Adb::from_env(),
                };
                if a.prior_enabled.is_empty() {
                    // `settings put ... ""` errors ("Bad arguments"); delete to clear the list and
                    // turn a11y off (we enabled it; with no prior service nothing else needs it).
                    let _ = adb.run(["shell", "settings", "delete", "secure", "enabled_accessibility_services"]);
                    let _ = adb.run(["shell", "settings", "put", "secure", "accessibility_enabled", "0"]);
                } else {
                    let _ = adb.run(["shell", "settings", "put", "secure",
                        "enabled_accessibility_services", &a.prior_enabled]);
                }
                let _ = adb.run(["forward", "--remove", &format!("tcp:{}", a.port)]);
            }
        }
    }
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
        WindowGeometry { x: 0, y: 100, width: 1080, height: 2300 }
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
        let mut t = tree_from_json(&v, &win()).unwrap();
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
    fn degenerate_bounds_clamp_instead_of_erroring() {
        // A live a11y tree legitimately has zero/inverted rects; the mapper must clamp, not fail.
        let v = json!({
            "ref": 0, "class": "android.view.View",
            "bounds": {"x": -5, "y": 10, "w": -3, "h": 0},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false
        });
        let t = tree_from_json(&v, &win()).expect("degenerate bounds must not error the snapshot");
        let b = t.root.bounds.unwrap();
        assert_eq!((b.width, b.height), (0, 0)); // negative/zero w/h clamp to 0
        assert_eq!((b.x, b.y), (-5, -90)); // window-relative: x -5-0, y 10-100
    }
}
