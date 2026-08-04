//! `ServiceA11y` — the on-device-AccessibilityService a11y reader. Talks the `tree`/`action`
//! line-JSON protocol to `glass-a11y.apk` over an `adb forward`ed socket, and maps the live
//! `AccessibilityNodeInfo` tree (sent as JSON) into glass's `AxTree`.

use std::sync::Mutex;

use serde_json::{Value, json};

use glass_core::accessibility::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree,
    Subject, TruncationLimit, WalkBudget, WalkLimits,
};
use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result};

use crate::axmap::{LabelInputs, class_to_role, labels};
use crate::conn::{CallFailure, Conn};

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

/// A `tree` reply: the tree, and the window it came from when the companion names one.
struct TreeReply {
    tree: Value,
    /// The package the companion actually answered about, when it names one. `None` either
    /// on an older companion that predates this field, or on a current one that cannot name
    /// the active window on this platform/state — either way, absent is not a mismatch.
    package: Option<String>,
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

    /// Run a request, reconnecting once and re-sending when `resend` accepts the failure.
    /// `rearm` is `None` for a request that IS the re-arm (`tree`); `Some(pkg)` re-serves `tree`
    /// for `pkg` on the fresh connection first and, only once its reply confirms `pkg` is still
    /// foreground, lets `req` follow it — see [`rearm_tree`].
    fn call_with(
        &self,
        req: Value,
        resend: fn(&CallFailure) -> bool,
        rearm: Option<&str>,
    ) -> std::result::Result<Value, CallFailure> {
        let mut conn = self.conn.lock().map_err(|_| {
            CallFailure::NotSent(GlassError::Backend(
                "a11y service client lock poisoned".into(),
            ))
        })?;
        match conn.call(req.clone()) {
            Ok(v) => Ok(v),
            Err(f) if resend(&f) => {
                // The service's accept loop accepts a fresh connection after a drop.
                *conn = Conn::open(self.port).map_err(|e| f.with_error(e))?;
                if let Some(pkg) = rearm {
                    // `f`'s classification, not the re-arm's own: recovering from `f` says no
                    // more about whether `req` was delivered, whichever step trips.
                    rearm_tree(&mut conn, pkg).map_err(|e| f.with_error(e))?;
                }
                conn.call(req)
            }
            Err(f) => Err(f),
        }
    }

    /// Run a request that can be run twice without consequence, transparently reconnecting
    /// once if the socket dropped. A dead socket usually surfaces on the *read*, not the write.
    fn call(&self, req: Value, rearm: Option<&str>) -> std::result::Result<Value, CallFailure> {
        self.call_with(req, CallFailure::is_transport, rearm)
    }

    /// [`Self::call`] for a side-effecting request: re-send only what provably never went out.
    fn call_once_sent(
        &self,
        req: Value,
        rearm: Option<&str>,
    ) -> std::result::Result<Value, CallFailure> {
        self.call_with(req, CallFailure::nothing_sent, rearm)
    }

    fn tree(&self, package: &str) -> Result<TreeReply> {
        let r = self
            .call(json!({"op": "tree", "package": package}), None)
            .map_err(CallFailure::into_error)?;
        let tree = r
            .get("tree")
            .cloned()
            .ok_or_else(|| GlassError::AccessibilityUnavailable("no tree in response".into()))?;
        Ok(TreeReply {
            tree,
            package: r.get("package").and_then(Value::as_str).map(str::to_owned),
        })
    }

    /// Fire `ACTION_CLICK` on device node `ref_id`. Never re-sent once delivered; the failure
    /// keeps its delivery classification. `package` never reaches the wire here — it only
    /// re-arms a reconnected connection's `tree` call — see [`Self::call_with`].
    fn click(&self, ref_id: u32, package: &str) -> std::result::Result<(), CallFailure> {
        self.call_once_sent(
            json!({"op": "action", "ref": ref_id, "action": "click"}),
            Some(package),
        )
        .map(|_| ())
    }

    /// Fire `ACTION_SET_TEXT` on device node `ref_id`. Idempotent, so this takes the
    /// reconnecting path. `package` never reaches the wire here — it only re-arms a
    /// reconnected connection's `tree` call — see [`Self::call_with`].
    fn set_text(&self, ref_id: u32, text: &str, package: &str) -> Result<()> {
        self.call(
            json!({"op": "action", "ref": ref_id, "action": "set_text", "text": text}),
            Some(package),
        )
        .map(|_| ())
        .map_err(CallFailure::into_error)
    }
}

/// Re-serve `tree` for `pkg` on a freshly reconnected connection, and confirm the reply still
/// names `pkg` before letting the pending request that triggered the reconnect follow it.
///
/// `pkg` must be the app the *served* tree actually described (`AxTree::subject`'s `actual`,
/// when set) — the one whose refs the pending request addresses — not the one originally asked
/// about: a permission dialog's tree answers for the dialog, not the app behind it. Re-arming
/// against whatever is foreground *now* would match by construction and authorise the pending
/// request against a window its ref never came from.
///
/// Returns a plain [`GlassError`], never a [`CallFailure`] — only the caller's own `f` (see
/// [`ServiceClient::call_with`]) classifies whether the *pending* request was delivered.
fn rearm_tree(conn: &mut Conn, pkg: &str) -> Result<()> {
    let reply = conn
        .call(json!({"op": "tree", "package": pkg}))
        .map_err(|f| {
            GlassError::Backend(format!(
                "reconnect could not re-arm the connection: {}",
                f.into_error()
            ))
        })?;
    let actual = reply.get("package").and_then(Value::as_str);
    let Some(actual) = actual else {
        return Err(GlassError::Backend(format!(
            "the reconnect could not confirm {pkg} is still the foreground app; the pending \
             request was not re-sent"
        )));
    };
    match subject_of(pkg, Some(actual)) {
        None => Ok(()),
        Some(s) => Err(GlassError::Backend(format!(
            "the foreground app moved from {} to {} while reconnecting; the pending request \
             was not re-sent",
            s.asked, s.actual
        ))),
    }
}

/// How long `invoke` waits for an actuated control's `checked` state to reach the tree.
const CHECK_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);
const CHECK_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// The Accessibility reader backed by the on-device service. `package` is the target app.
pub struct ServiceA11y {
    client: ServiceClient,
    package: String,
    /// How long [`Self::wait_for_check`] polls; [`CHECK_TIMEOUT`] outside tests.
    check_timeout: std::time::Duration,
}

impl ServiceA11y {
    pub fn new(client: ServiceClient, package: String) -> Self {
        Self {
            client,
            package,
            check_timeout: CHECK_TIMEOUT,
        }
    }

    /// Poll until the actuated control reports `checked == want`. The toolkit's UI update
    /// reaches the accessibility tree a beat after the action; `set_value` polls for the same
    /// reason.
    fn wait_for_check(&mut self, ctx: &AxContext, plan: &InvokePlan, want: bool) -> Result<()> {
        let deadline = std::time::Instant::now() + self.check_timeout;
        loop {
            let mut after = self.snapshot(ctx)?;
            after.assign_ids();
            let seen = check_state(&after, &plan.actuated);
            if seen == CheckState::Reached(want) {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                return Err(check_timeout(plan.target.0, &plan.actuated, want, seen));
            }
            std::thread::sleep(CHECK_POLL);
        }
    }

    /// Fire the click for an already-resolved `plan`, then confirm any expected state change.
    /// Split out of `invoke` so a test can force a reconnect strictly between the snapshot/plan
    /// step and this one — a window `invoke`'s own straight-line execution never otherwise
    /// exposes to a caller.
    ///
    /// `subject` (the plan's tree's own `AxTree::subject`) is passed separately from `plan`: it
    /// borrows the tree, not `self`, so the caller can still hand it to this `&mut self` method.
    fn dispatch_click(
        &mut self,
        ctx: &AxContext,
        target: &AxTarget,
        plan: &InvokePlan,
        subject: Option<&Subject>,
    ) -> Result<Option<AxNodeId>> {
        let acting_on = subject.map_or(self.package.as_str(), |s| s.actual.as_str());
        self.client
            .click(plan.actuated.id.0, acting_on)
            .map_err(|f| action_error(target.id.0, f))?;
        if let Some(want) = plan.want_checked {
            self.wait_for_check(ctx, plan, want)?;
        }
        Ok(plan.substituted())
    }
}

/// What the tree describes, when the companion named a window and it is not the one asked about.
/// A companion that names none makes no claim — absent is not a mismatch.
fn subject_of(asked: &str, actual: Option<&str>) -> Option<Subject> {
    let actual = actual?;
    (actual != asked).then(|| Subject {
        asked: asked.to_owned(),
        actual: actual.to_owned(),
    })
}

impl Accessibility for ServiceA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        let reply = self.client.tree(&self.package)?;
        let mut tree = tree_from_json(&reply.tree, &ctx.window, ctx.limits)?;
        tree.subject = subject_of(&self.package, reply.package.as_deref());
        Ok(tree)
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
        // Re-arm against the app the served tree actually described, not the one asked about —
        // that's the app `target`'s ref came from (see `rearm_tree`).
        let acting_on = tree
            .subject
            .as_ref()
            .map_or(self.package.as_str(), |s| s.actual.as_str());
        self.client.set_text(target.id.0, text, acting_on)?;
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

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let tree = {
            let mut t = self.snapshot(ctx)?;
            t.assign_ids();
            t
        };
        let plan = invoke_plan(&tree, target)?;
        self.dispatch_click(ctx, target, &plan, tree.subject.as_ref())
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

    /// Install + enable the service on `adb`'s device, forward a port, and connect. Returns a
    /// `ServiceClient` only once the service has served a window. The apk path is resolved from
    /// env by the caller.
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
        put_enabled_services(adb, &want)?;
        put_secure(adb, "accessibility_enabled", "1")?;
        let out = adb.run(["forward", "tcp:0", &format!("localabstract:{SOCKET}")])?;
        let port = crate::agent::parse_forward_port(&out)
            .ok_or_else(|| GlassError::Backend(format!("adb forward gave no port: {out:?}")))?;
        // From here, a failure must roll back the settings + forward, else a failed `ensure` leaks
        // an enabled service and a forward slot.
        let client = ready_or_restore(
            port,
            READY_ATTEMPT,
            READY_ATTEMPTS,
            &|step| {
                // Last resort: the reinstall is what SIGKILLs the service and starts this
                // race, and also the only step observed to bring a stuck binding back.
                if step + 1 == READY_ATTEMPTS {
                    install_service(adb, apk)?;
                }
                force_rebind(adb, &want)
            },
            &|| restore_a11y(adb, prior, prior_a11y, port),
        )?;
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

fn put_secure(adb: &Adb, key: &str, value: &str) -> Result<()> {
    adb.run(["shell", "settings", "put", "secure", key, value])
        .map(|_| ())
}

/// Write `enabled_accessibility_services`. An empty list is a `delete`: `settings put ... ""`
/// errors ("Bad arguments").
fn put_enabled_services(adb: &Adb, list: &str) -> Result<()> {
    if list.is_empty() {
        adb.run([
            "shell",
            "settings",
            "delete",
            "secure",
            "enabled_accessibility_services",
        ])
        .map(|_| ())
    } else {
        put_secure(adb, "enabled_accessibility_services", list)
    }
}

/// `list` with this service's component removed — the colon-separated list the platform reads.
fn without_service(list: &str) -> String {
    list.split(':')
        .filter(|s| !s.is_empty() && *s != SERVICE_COMPONENT)
        .collect::<Vec<_>>()
        .join(":")
}

/// Make the platform tear this service's accessibility binding down and build a fresh one, by
/// disabling and re-enabling it. The process is left alone: a restart is what glass#324 recovers
/// from, not a remedy for it.
fn force_rebind(adb: &Adb, enabled: &str) -> Result<()> {
    put_secure(adb, "accessibility_enabled", "0")?;
    put_enabled_services(adb, &without_service(enabled))?;
    put_enabled_services(adb, enabled)?;
    put_secure(adb, "accessibility_enabled", "1")
}

/// Restore `enabled_accessibility_services` + `accessibility_enabled` to their prior values and
/// remove the forwarded port. Shared by `shutdown` and the failed-`ensure` rollback. Best-effort.
fn restore_a11y(adb: &Adb, prior_enabled: &str, prior_a11y_enabled: &str, port: u16) {
    let _ = put_enabled_services(adb, prior_enabled);
    let _ = put_secure(adb, "accessibility_enabled", prior_a11y_enabled);
    let _ = adb.run(["forward", "--remove", &format!("tcp:{port}")]);
}

/// How long ONE readiness attempt polls for a served window. On a cold-booted device readiness
/// that succeeds at all has done so in 0.7-2.5s, and one still refused at 6s ran out a 20s budget
/// too (glass#324).
const READY_ATTEMPT: std::time::Duration = std::time::Duration::from_secs(6);
const READY_POLL: std::time::Duration = std::time::Duration::from_millis(150);

/// Readiness attempts [`A11yServiceRegistry::ensure`] makes, the first included: poll, force a
/// rebind, poll, reinstall + force a rebind, poll. Three of these is 18s of polling.
const READY_ATTEMPTS: u32 = 3;

/// Wait for readiness on `port`, and when an attempt is refused for its whole budget, force the
/// service's binding to be rebuilt and try again — `rebind(step)` for the `step`th recovery,
/// escalating with the number. `attempts` counts the first, so it makes `attempts - 1` rebinds.
///
/// `restore` runs on exactly one path, giving up — here rather than at the caller so retrying
/// cannot grow a second way out that forgets it.
fn ready_or_restore(
    port: u16,
    attempt: std::time::Duration,
    attempts: u32,
    rebind: &dyn Fn(u32) -> Result<()>,
    restore: &dyn Fn(),
) -> Result<ServiceClient> {
    let started = std::time::Instant::now();
    let mut refusal = match wait_for_ready(port, attempt) {
        Ok(client) => return Ok(client),
        Err(e) => e,
    };
    let mut rebinds = 0u32;
    for step in 1..attempts {
        if let Err(e) = rebind(step) {
            restore();
            return Err(GlassError::Backend(format!(
                "could not force the on-device a11y service on :{port} to rebind after {step} \
                 readiness attempts, {}ms total: {e} — last attempt: {refusal}",
                started.elapsed().as_millis()
            )));
        }
        rebinds += 1;
        match wait_for_ready(port, attempt) {
            Ok(client) => return Ok(client),
            Err(e) => refusal = e,
        }
    }
    restore();
    Err(GlassError::AccessibilityUnavailable(format!(
        "the on-device a11y service on :{port} never served a window: {} readiness attempts of \
         {attempt:?}, {rebinds} forced rebinds between them, {}ms total — last attempt: {refusal}",
        rebinds + 1,
        started.elapsed().as_millis()
    )))
}

/// One readiness attempt against the binding currently on `port`: connect, then poll `tree`
/// until one is served. Both gates share the one budget. The error is a message, not a
/// `GlassError`: [`ready_or_restore`] classifies the sequence, and a variant here would nest a
/// second "accessibility unavailable:" inside the first.
fn wait_for_ready(
    port: u16,
    timeout: std::time::Duration,
) -> std::result::Result<ServiceClient, String> {
    let deadline = std::time::Instant::now() + timeout;
    let client = loop {
        match ServiceClient::connect(port) {
            Ok(c) => break c,
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(format!("never accepted a connection in {timeout:?}: {e}"));
            }
            Err(_) => std::thread::sleep(READY_POLL),
        }
    };
    loop {
        // `ping` proves only that the process is up; a served tree proves the capability. The
        // empty package asks for the active window, exactly what the reader asks for. One
        // connection throughout — `ServiceClient` reopens its own socket when a call finds the
        // service gone.
        match client.tree("") {
            Ok(_) => return Ok(client),
            Err(e) if std::time::Instant::now() >= deadline => {
                return Err(format!("answered but served no window in {timeout:?}: {e}"));
            }
            Err(_) => std::thread::sleep(READY_POLL),
        }
    }
}

/// One node's identity, captured at actuation so a state read back after the click can be
/// confirmed to be that same control. An `AxNodeId` is a positional pre-order index re-derived
/// per snapshot, so an id alone can land on a different node in the next tree.
#[derive(Debug)]
struct Actuated {
    id: AxNodeId,
    role: AxRole,
    name: Option<String>,
}

/// What an `invoke` must do, decided from one tree before any device I/O.
#[derive(Debug)]
struct InvokePlan {
    /// The element the caller named.
    target: AxNodeId,
    /// The node the click is sent to — the target, or the control the climb resolved to.
    actuated: Actuated,
    /// `Some(want)` when the actuated control must be read back reporting `checked == want`.
    want_checked: Option<bool>,
}

impl InvokePlan {
    /// The actuated node, when it is not the one the caller named.
    fn substituted(&self) -> Option<AxNodeId> {
        (self.actuated.id != self.target).then_some(self.actuated.id)
    }
}

/// Whether clicking a control of this role must move its `checked` state. A checkbox or a
/// switch toggles; a radio button or a tab selects, and clicking the one already selected
/// leaves it selected.
fn toggles(role: AxRole) -> bool {
    matches!(role, AxRole::CheckBox | AxRole::ToggleButton)
}

/// Decide what an `invoke` does with `target`: which node to actuate, and what the tree must
/// show afterwards. Pure, so the decision is testable without a device.
///
/// The order is load-bearing: a Compose control omits `ACTION_CLICK` while disabled, so a climb
/// run before the target's own `enabled` check walks past it and actuates whatever encloses it.
fn invoke_plan(tree: &AxTree, target: &AxTarget) -> Result<InvokePlan> {
    let node = crate::a11y::fingerprinted(tree, target)?;
    if !node.states.enabled {
        return Err(disabled_error(target.id.0, target.id));
    }
    let chosen = actuable_node(tree, target)?;
    if !chosen.states.enabled {
        return Err(disabled_error(target.id.0, chosen.id));
    }
    // Under these caps the kept nodes are no longer a pre-order prefix — the walk drops a
    // subtree and carries on with later siblings — so a host id no longer equals the device
    // `ref` it would be sent as. `Nodes` stops the walk outright and keeps the prefix.
    // Fallback-eligible on purpose: nothing was dispatched, and the pointer path aims at this
    // tree's own bounds, which the id/`ref` skew does not touch. It must therefore stay below
    // both `enabled` refusals — reached first, it hands a disabled control to the pointer
    // path.
    if let Some(t) = &tree.truncated
        && matches!(t.limit, TruncationLimit::Depth | TruncationLimit::Siblings)
    {
        return Err(GlassError::AxActionUnavailable(target.id.0));
    }
    let want = if toggles(chosen.role) {
        !chosen.states.checked
    } else {
        true
    };
    Ok(InvokePlan {
        target: target.id,
        actuated: Actuated {
            id: chosen.id,
            role: chosen.role,
            name: chosen.name.clone(),
        },
        want_checked: (chosen.states.checkable && want != chosen.states.checked).then_some(want),
    })
}

/// What a tree read after the click says about the actuated control.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum CheckState {
    /// The control is still there, reporting this `checked`.
    Reached(bool),
    /// The id no longer resolves to the checkable control that was actuated.
    Gone,
}

/// Read the actuated control's `checked` out of a tree taken after the click.
///
/// The id alone cannot accept a state change: it is positional, so a node the click added above
/// the control shifts every later id, and `checked` reads `false` on any node with no checked
/// state — between them, an inert label sliding into the id reads as a flip. Role and name are
/// what stand in its place, so a same-named sibling that inherits the id is accepted as the
/// control; `fingerprinted` compares bounds and value too, for the same job before the click.
fn check_state(after: &AxTree, act: &Actuated) -> CheckState {
    match after.find(act.id) {
        Some(n) if n.states.checkable && n.role == act.role && n.name == act.name => {
            CheckState::Reached(n.states.checked)
        }
        _ => CheckState::Gone,
    }
}

/// The node an `invoke` should actuate on behalf of `target`.
///
/// A Compose button's label and its clickable node are different nodes: the touch-target
/// carries no name and the named child carries no `ACTION_CLICK`. When the target itself
/// advertises no click, climb to the nearest node that does and encloses it. Only the ancestor
/// chain is walked, so an overlapping sibling drawn above the target could still take a real tap.
fn actuable_node<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
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

/// Whether `outer` fully contains `inner`, edges inclusive. Either side missing bounds is false —
/// the climb must not reach past geometry it cannot check — though neither arm is reachable
/// through this reader, since `json_to_node` errors a snapshot on a node with no bounds.
fn encloses(outer: Option<AxRect>, inner: Option<AxRect>) -> bool {
    let (Some(o), Some(i)) = (outer, inner) else {
        return false;
    };
    i64::from(o.x) <= i64::from(i.x)
        && i64::from(o.y) <= i64::from(i.y)
        && i64::from(o.x) + i64::from(o.width) >= i64::from(i.x) + i64::from(i.width)
        && i64::from(o.y) + i64::from(o.height) >= i64::from(i.y) + i64::from(i.height)
}

/// The error for a control the tree says is disabled. Deliberately not `AxActionUnavailable`:
/// a pointer click would tap a control that cannot act and report success.
fn disabled_error(target: u32, disabled: AxNodeId) -> GlassError {
    GlassError::AxActionFailed(
        target,
        format!(
            "element {} is disabled, so its click action was refused; no pointer click was \
             synthesized on top of it",
            disabled.0
        ),
    )
}

/// Map a click failure onto the invoke contract. No arm is fallback-eligible: a refusal may
/// still have reached the toolkit, and a lost answer says nothing either way.
fn action_error(target: u32, f: CallFailure) -> GlassError {
    match f {
        CallFailure::NotSent(e) => GlassError::AccessibilityUnavailable(format!(
            "the click on element {target} could not be sent: {e}"
        )),
        // Non-fallback-eligible only stops glass re-clicking; the message has to stop a human
        // or an agent doing it by hand.
        CallFailure::AnswerLost(e) => GlassError::AccessibilityUnavailable(format!(
            "the click on element {target} was sent but its result was lost ({e}); it may or \
             may not have actuated — re-snapshot to see the app's state rather than clicking \
             again"
        )),
        CallFailure::Refused(e) => GlassError::AxActionFailed(target, e.to_string()),
    }
}

/// The error for a control whose click was accepted but whose state never reached `want`.
fn check_timeout(target: u32, act: &Actuated, want: bool, seen: CheckState) -> GlassError {
    let id = act.id.0;
    let detail = match seen {
        CheckState::Reached(state) => format!(
            "its checked state stayed {state} instead of becoming {want}; the control did not \
             take the click"
        ),
        CheckState::Gone => "it is no longer the control that was clicked, so the click could \
             not be confirmed; a control that disappears or is replaced after an accepted click \
             has usually acted — re-snapshot to see the app's state rather than clicking again"
            .to_string(),
    };
    GlassError::AxActionFailed(
        target,
        format!("the click action on element {id} was accepted but {detail}"),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::accessibility::AxRole;
    use serde_json::json;
    use std::io::{BufRead, Write};
    use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

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
    fn a_reply_naming_another_app_discloses_it() {
        let subject = subject_of(
            "com.example.app",
            Some("com.google.android.permissioncontroller"),
        );
        assert_eq!(
            subject,
            Some(Subject {
                asked: "com.example.app".into(),
                actual: "com.google.android.permissioncontroller".into(),
            })
        );
    }

    #[test]
    fn a_reply_naming_the_app_asked_for_discloses_nothing() {
        assert_eq!(subject_of("com.example.app", Some("com.example.app")), None);
    }

    #[test]
    fn an_older_companion_that_names_no_window_makes_no_claim() {
        // The key is absent, not null: nothing is known, which is not the same as a mismatch.
        assert_eq!(subject_of("com.example.app", None), None);
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

    /// Bounds equal on every edge. `actuable_node` climbs to the nearest enclosing clickable
    /// ancestor, and a Compose touch target's rect routinely equals its label's — a strict
    /// comparison here sends every such click to the pointer path instead.
    #[test]
    fn a_rect_encloses_one_that_shares_all_four_edges() {
        let same = AxRect {
            x: 40,
            y: 200,
            width: 200,
            height: 100,
        };
        assert!(encloses(Some(same), Some(same)));
    }

    /// Defensive default, pinned on purpose: the signature admits `None`, though no production
    /// caller supplies it — `encloses`'s own doc explains why.
    #[test]
    fn a_node_without_bounds_neither_encloses_nor_is_enclosed() {
        let rect = AxRect {
            x: 40,
            y: 200,
            width: 200,
            height: 100,
        };
        assert!(!encloses(None, Some(rect)), "an outer with no bounds");
        assert!(!encloses(Some(rect), None), "an inner with no bounds");
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
        // Inside a clickable card, so "the target itself" is distinguishable from "something
        // enclosing it that also advertises a click".
        let t = built(&nested_clickables());
        let btn = target_for(&t, AxNodeId(2));
        assert_eq!(actuable_node(&t, &btn).unwrap().id, AxNodeId(2));
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

    /// The clickable node's box shares all four edges with its label's, the coincident-edge case
    /// `encloses` itself is unit-tested against — this proves the climb actually takes that arm.
    #[test]
    fn a_climb_reaches_a_clickable_ancestor_whose_bounds_exactly_match_the_target() {
        let mut v = compose_like();
        v["children"][0]["bounds"] = json!({"x": 120, "y": 520, "w": 80, "h": 50});
        let t = built(&v);
        let label = target_for(&t, AxNodeId(2));
        assert_eq!(actuable_node(&t, &label).unwrap().id, AxNodeId(1));
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
            invoke_plan(&t, &label),
            Err(GlassError::AxElementChanged(2))
        ));
    }

    #[test]
    fn a_drifted_target_must_not_fall_back_to_a_pointer_click() {
        let t = built(&compose_like());
        let mut label = target_for(&t, AxNodeId(2));
        label.name = Some("Send".into());
        let e = invoke_plan(&t, &label).unwrap_err();
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
    fn a_device_refusal_is_a_failed_action_not_a_lost_answer() {
        // The device answered and refused. The call may have reached the toolkit, so a pointer
        // click on top of it could actuate the control twice.
        let inner = GlassError::Backend("agent: ACTION_CLICK refused".into());
        let e = action_error(2, CallFailure::Refused(inner));
        assert!(matches!(e, GlassError::AxActionFailed(2, _)), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert!(e.to_string().contains("ACTION_CLICK refused"), "{e}");
    }

    #[test]
    fn a_click_that_never_went_out_is_a_transport_error_not_a_refusal() {
        let inner = GlassError::Backend("agent write: broken pipe".into());
        let e = action_error(2, CallFailure::NotSent(inner));
        assert!(matches!(e, GlassError::AccessibilityUnavailable(_)), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    #[test]
    fn a_click_whose_answer_was_lost_says_it_may_already_have_actuated() {
        let inner = GlassError::Backend("agent closed the connection".into());
        let e = action_error(2, CallFailure::AnswerLost(inner));
        assert!(matches!(e, GlassError::AccessibilityUnavailable(_)), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        // Refusing to fall back is only half of it — the message has to stop a hand-retry.
        let m = e.to_string();
        assert!(m.contains("was sent"), "{m}");
        assert!(m.contains("may or may not have actuated"), "{m}");
        assert!(m.contains("re-snapshot"), "{m}");
    }

    fn actuated(id: u32, role: AxRole, name: &str) -> Actuated {
        Actuated {
            id: AxNodeId(id),
            role,
            name: Some(name.into()),
        }
    }

    #[test]
    fn a_toggle_that_never_flipped_is_a_failure_not_a_missing_action() {
        let act = actuated(1, AxRole::CheckBox, "Agree");
        let e = check_timeout(2, &act, true, CheckState::Reached(false));
        assert!(matches!(e, GlassError::AxActionFailed(2, _)), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        // The distinction that matters to a caller: the action WAS accepted.
        assert!(e.to_string().contains("accepted"), "{e}");
        assert!(e.to_string().contains("stayed false"), "{e}");
    }

    #[test]
    fn a_control_that_vanished_after_the_click_is_not_reported_as_never_moving() {
        // A checkable row that navigates, a toggle in a dismissing dialog: "it stayed <was>"
        // would be a wrong diagnosis of a click that worked, inviting the retry that actuates
        // twice.
        let act = actuated(1, AxRole::CheckBox, "Agree");
        let e = check_timeout(2, &act, true, CheckState::Gone);
        let m = e.to_string();
        assert!(!m.contains("stayed"), "no state was read to report: {m}");
        assert!(m.contains("has usually acted"), "{m}");
        assert!(m.contains("re-snapshot"), "{m}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    /// A tree taken after the click, holding `nodes` under the root.
    fn after_tree(nodes: Vec<Value>) -> AxTree {
        built(&json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "enabled": true,
            "children": nodes
        }))
    }

    fn checkable_json(class: &str, name: &str, checked: bool) -> Value {
        json!({
            "class": class, "text": name,
            "bounds": {"x": 40, "y": 200, "w": 200, "h": 100},
            "clickable": true, "enabled": true, "checkable": true, "checked": checked
        })
    }

    #[test]
    fn the_actuated_control_reports_the_state_it_reads_back() {
        let after = after_tree(vec![checkable_json(
            "android.widget.CheckBox",
            "Agree",
            true,
        )]);
        let act = actuated(1, AxRole::CheckBox, "Agree");
        assert_eq!(check_state(&after, &act), CheckState::Reached(true));
    }

    #[test]
    fn a_node_that_inherited_the_id_does_not_count_as_the_flip() {
        // The app rejected the change and showed an error line above the control, so every
        // later id shifted. Id 1 is now an inert label, whose `checked` reads false — which a
        // bare `checked != was` reads as an unchecking.
        let after = after_tree(vec![
            json!({
                "class": "android.widget.TextView", "text": "You must agree first",
                "bounds": {"x": 40, "y": 150, "w": 400, "h": 40}, "enabled": true
            }),
            checkable_json("android.widget.CheckBox", "Agree", true),
        ]);
        let act = actuated(1, AxRole::CheckBox, "Agree");
        assert_eq!(check_state(&after, &act), CheckState::Gone);
    }

    #[test]
    fn a_different_control_at_the_same_id_does_not_count_as_the_flip() {
        // Same id, still checkable, still a CheckBox — only the name says it is a different
        // control, which is exactly the case a bare id match cannot see.
        let after = after_tree(vec![checkable_json(
            "android.widget.CheckBox",
            "Subscribe",
            false,
        )]);
        let act = actuated(1, AxRole::CheckBox, "Agree");
        assert_eq!(check_state(&after, &act), CheckState::Gone);
    }

    /// The known weakness, pinned rather than fixed: the fingerprint is `checkable` + role +
    /// name, so a same-named sibling that inherits the id is accepted as the control that was
    /// clicked, and its state is reported as the flip. `Actuated` records no rect, so nothing
    /// here can tell the two apart. Strengthening this is glass#319's job — when it lands, this
    /// test is the expectation it must consciously change.
    #[test]
    fn a_same_named_sibling_that_inherited_the_id_is_accepted_as_the_control() {
        // `plan.actuated` is recorded from this tree, where id 1 really is the control clicked.
        let mut clicked = checkable_json("android.widget.CheckBox", "Agree", false);
        clicked["bounds"] = json!({"x": 40, "y": 400, "w": 200, "h": 100});
        let before = after_tree(vec![clicked.clone()]);
        let plan = invoke_plan(&before, &target_for(&before, AxNodeId(1))).unwrap();

        // Ids shifted between the click and this read-back: an already-checked impostor now
        // occupies id 1, and the control actually clicked is id 2, still unchecked.
        let impostor = checkable_json("android.widget.CheckBox", "Agree", true);
        let after = after_tree(vec![impostor, clicked]);
        assert_eq!(
            check_state(&after, &plan.actuated),
            CheckState::Reached(true),
            "a same-named sibling is indistinguishable to a role+name fingerprint"
        );
    }

    /// `CheckedTextView` also maps to `AxRole::CheckBox` (see `axmap.rs`), so a node that lost
    /// its checkable state between the click and the read-back is still reachable under the
    /// same role and name — it must not be read as `checked == false`.
    #[test]
    fn a_same_named_node_no_longer_checkable_is_not_read_as_the_control() {
        let mut node = checkable_json("android.widget.CheckedTextView", "Agree", false);
        node["checkable"] = json!(false);
        let after = after_tree(vec![node]);
        let act = actuated(1, AxRole::CheckBox, "Agree");
        assert_eq!(check_state(&after, &act), CheckState::Gone);
    }

    /// A `Switch` maps to `AxRole::ToggleButton`, not `AxRole::CheckBox` — role is as much a
    /// stand-in for the id as name, and this is the case that isolates it from name.
    #[test]
    fn a_same_named_node_of_a_different_role_is_not_read_as_the_control() {
        let after = after_tree(vec![checkable_json("android.widget.Switch", "Agree", true)]);
        let act = actuated(1, AxRole::CheckBox, "Agree");
        assert_eq!(check_state(&after, &act), CheckState::Gone);
    }

    /// A clickable card wrapping a clickable button wrapping its label — nested clickables are
    /// the Android norm (a row that opens a detail view, holding a button that deletes).
    fn nested_clickables() -> Value {
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "clickable": false, "enabled": true,
            "children": [
                {"class": "androidx.cardview.widget.CardView",
                 "bounds": {"x": 40, "y": 400, "w": 900, "h": 300},
                 "clickable": true, "enabled": true,
                 "children": [
                    {"class": "android.widget.Button",
                     "bounds": {"x": 60, "y": 440, "w": 300, "h": 120},
                     "clickable": true, "enabled": true,
                     "children": [
                        {"class": "android.widget.TextView", "text": "Delete",
                         "bounds": {"x": 80, "y": 470, "w": 100, "h": 50},
                         "clickable": false, "enabled": true}
                     ]}
                 ]}
            ]
        })
    }

    #[test]
    fn a_label_climbs_to_the_nearest_clickable_ancestor_not_the_outermost() {
        // Climbing to the card actuates "open detail" where the caller asked for "delete".
        let t = built(&nested_clickables());
        let label = target_for(&t, AxNodeId(3));
        assert_eq!(label.name.as_deref(), Some("Delete"));
        assert_eq!(
            actuable_node(&t, &label).unwrap().id,
            AxNodeId(2),
            "the enclosing Button, not the Card around it"
        );
    }

    /// A disabled control that advertises no click of its own, inside a clickable card — the
    /// Compose shape, where the delegate omits `ACTION_CLICK` while a control is disabled, so
    /// nothing but the node's own `enabled` flag says to refuse.
    fn disabled_inside_a_clickable_card() -> Value {
        let mut v = nested_clickables();
        v["children"][0]["children"][0]["clickable"] = json!(false);
        v["children"][0]["children"][0]["enabled"] = json!(false);
        v
    }

    #[test]
    fn a_disabled_target_is_refused_even_when_something_around_it_is_clickable() {
        let t = built(&disabled_inside_a_clickable_card());
        let e = invoke_plan(&t, &target_for(&t, AxNodeId(2))).unwrap_err();
        assert!(matches!(e, GlassError::AxActionFailed(2, _)), "{e}");
        assert!(e.to_string().contains("disabled"), "{e}");
        assert!(
            !e.invoke_fallback_eligible(),
            "a pointer click would tap a control that cannot act: {e}"
        );
    }

    #[test]
    fn a_disabled_control_the_climb_resolved_to_is_refused_as_well() {
        // The target itself is fine; what would actually be actuated is not.
        let mut v = nested_clickables();
        v["children"][0]["children"][0]["clickable"] = json!(false);
        v["children"][0]["enabled"] = json!(false);
        let t = built(&v);
        let e = invoke_plan(&t, &target_for(&t, AxNodeId(3))).unwrap_err();
        assert!(e.to_string().contains("element 1 is disabled"), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    #[test]
    fn a_disabled_control_is_refused_before_a_truncated_tree_can_fall_back() {
        // Same shape as the test above, in a truncated tree. `AxActionUnavailable` is
        // fallback-eligible, so a truncation guard reached before the climb's `enabled` check
        // taps the disabled control's centre and reports ok.
        let mut v = nested_clickables();
        v["children"][0]["children"][0]["clickable"] = json!(false);
        v["children"][0]["enabled"] = json!(false);
        v["children"].as_array_mut().expect("root children").push(
            json!({"class": "android.widget.TextView", "text": "Cancel",
                         "bounds": {"x": 40, "y": 800, "w": 200, "h": 60},
                         "clickable": false, "enabled": true}),
        );
        let mut t = tree_from_json(
            &v,
            &win(),
            WalkLimits {
                siblings: 1,
                ..WalkLimits::DEFAULT
            },
        )
        .expect("maps");
        t.assign_ids();
        assert_eq!(
            t.truncated.map(|x| x.limit),
            Some(TruncationLimit::Siblings)
        );
        let e = invoke_plan(&t, &target_for(&t, AxNodeId(3))).unwrap_err();
        assert!(e.to_string().contains("element 1 is disabled"), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    #[test]
    fn a_disabled_target_with_nothing_clickable_around_it_is_refused_too() {
        // With no clickable ancestor the climb ends in `AxActionUnavailable`, which IS
        // fallback-eligible — so reaching it taps the disabled control's centre and reports ok.
        let mut v = disabled_inside_a_clickable_card();
        v["children"][0]["clickable"] = json!(false);
        let t = built(&v);
        let e = invoke_plan(&t, &target_for(&t, AxNodeId(2))).unwrap_err();
        assert!(e.to_string().contains("disabled"), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
    }

    /// A device tree whose only child is a checkable control of `class`, in `checked` state.
    fn one_checkable(class: &str, checked: bool) -> Value {
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "enabled": true,
            "children": [checkable_json(class, "Agree", checked)]
        })
    }

    fn selection_tree(class: &str, checked: bool) -> AxTree {
        built(&one_checkable(class, checked))
    }

    #[test]
    fn re_clicking_a_selected_radio_button_asks_for_no_state_change() {
        // A radio button already selected stays selected, so waiting for a flip fails a click
        // on a UI already in the requested state.
        let t = selection_tree("android.widget.RadioButton", true);
        let plan = invoke_plan(&t, &target_for(&t, AxNodeId(1))).unwrap();
        assert_eq!(plan.want_checked, None);
    }

    #[test]
    fn clicking_an_unselected_radio_button_asks_for_it_to_end_selected() {
        let t = selection_tree("android.widget.RadioButton", false);
        let plan = invoke_plan(&t, &target_for(&t, AxNodeId(1))).unwrap();
        assert_eq!(plan.want_checked, Some(true));
    }

    #[test]
    fn clicking_a_checkbox_asks_for_its_state_to_move_either_way() {
        for was in [false, true] {
            let t = selection_tree("android.widget.CheckBox", was);
            let plan = invoke_plan(&t, &target_for(&t, AxNodeId(1))).unwrap();
            assert_eq!(plan.want_checked, Some(!was), "checked was {was}");
        }
    }

    #[test]
    fn clicking_a_switch_asks_for_its_state_to_move_either_way() {
        for was in [false, true] {
            let t = selection_tree("android.widget.Switch", was);
            let plan = invoke_plan(&t, &target_for(&t, AxNodeId(1))).unwrap();
            assert_eq!(plan.want_checked, Some(!was), "checked was {was}");
        }
    }

    #[test]
    fn a_plain_button_has_no_state_to_wait_for() {
        let t = built(&compose_like());
        let plan = invoke_plan(&t, &target_for(&t, AxNodeId(2))).unwrap();
        assert_eq!(plan.want_checked, None);
    }

    #[test]
    fn the_plan_actuates_the_climbed_ancestor_and_reports_the_substitution() {
        let t = built(&compose_like());
        let plan = invoke_plan(&t, &target_for(&t, AxNodeId(2))).unwrap();
        assert_eq!(plan.actuated.id, AxNodeId(1));
        assert_eq!(plan.substituted(), Some(AxNodeId(1)));
    }

    #[test]
    fn a_target_actuated_in_its_own_right_substitutes_nothing() {
        let t = built(&nested_clickables());
        let plan = invoke_plan(&t, &target_for(&t, AxNodeId(2))).unwrap();
        assert_eq!(plan.actuated.id, AxNodeId(2));
        assert_eq!(plan.substituted(), None);
    }

    /// The compose fixture read back under `limits` — small caps make the walk report the
    /// truncation a huge device tree would.
    fn truncated_compose(limits: WalkLimits) -> AxTree {
        let mut t = tree_from_json(&compose_like(), &win(), limits).expect("maps");
        t.assign_ids();
        t
    }

    #[test]
    fn a_depth_truncated_tree_does_not_actuate_natively() {
        // The depth cap drops a node's children and carries on with later siblings, so the kept
        // set is no longer a pre-order prefix.
        let t = truncated_compose(WalkLimits {
            depth: 1,
            ..WalkLimits::DEFAULT
        });
        assert_eq!(t.truncated.map(|x| x.limit), Some(TruncationLimit::Depth));
        let e = invoke_plan(&t, &target_for(&t, AxNodeId(1))).unwrap_err();
        assert!(matches!(e, GlassError::AxActionUnavailable(1)), "{e}");
        // Nothing was dispatched and the pointer path aims at this tree's own bounds, so the
        // click still lands, disclosed as a pointer click.
        assert!(e.invoke_fallback_eligible(), "{e}");
    }

    #[test]
    fn a_sibling_truncated_tree_does_not_actuate_natively() {
        let mut v = compose_like();
        v["children"].as_array_mut().unwrap().push(
            json!({"class": "android.widget.TextView", "text": "Cancel",
                         "bounds": {"x": 60, "y": 700, "w": 210, "h": 130},
                         "clickable": false, "enabled": true}),
        );
        let mut t = tree_from_json(
            &v,
            &win(),
            WalkLimits {
                siblings: 1,
                ..WalkLimits::DEFAULT
            },
        )
        .expect("maps");
        t.assign_ids();
        assert_eq!(
            t.truncated.map(|x| x.limit),
            Some(TruncationLimit::Siblings)
        );
        let e = invoke_plan(&t, &target_for(&t, AxNodeId(1))).unwrap_err();
        assert!(matches!(e, GlassError::AxActionUnavailable(1)), "{e}");
    }

    #[test]
    fn a_node_capped_tree_still_actuates_natively() {
        // The node cap stops the walk outright, so every surviving id still equals its device
        // `ref`.
        let t = truncated_compose(WalkLimits {
            nodes: 2,
            ..WalkLimits::DEFAULT
        });
        assert_eq!(t.truncated.map(|x| x.limit), Some(TruncationLimit::Nodes));
        assert_eq!(
            invoke_plan(&t, &target_for(&t, AxNodeId(1)))
                .unwrap()
                .actuated
                .id,
            AxNodeId(1)
        );
    }

    /// How the fake device answers an `action` request.
    #[derive(Clone, Copy)]
    enum OnAction {
        Ok,
        /// Read the request, then drop the socket without answering — the shape of an answer
        /// lost to a read timeout, a rebinding service or a reset `adb forward` tunnel.
        DropWithoutAnswering,
    }

    /// What a fake `tree` reply's `package` field says, relative to what was asked.
    enum TreePackage {
        /// Whatever the request asked for.
        Echo,
        /// A different package than asked — the foreground changed since the last `tree`.
        Other(String),
        /// The key omitted, as an older companion sends it, or one that cannot name the active
        /// window.
        Unnamed,
    }

    /// Whether a fake `tree` request is served normally (per `packages`) or refused outright —
    /// mirrors the shipped companion's `Server.kt`, which answers `Response.error(id, "no
    /// active window")` when nothing is in the foreground.
    #[derive(Clone, Copy)]
    enum OnTree {
        Serve,
        Refused(&'static str),
        /// Refused "no active window" until the flag is set, then served: a binding that starts
        /// serving only once something off the request path makes the platform rebuild it.
        RefusedUntil(&'static AtomicBool),
    }

    /// A fake on-device service on a local TCP listener: serves `tree` from a script (the last
    /// entry repeating) and records every request, tagged with the connection it arrived on so
    /// a re-send after a reconnect is visible in the log.
    fn fake_service(trees: Vec<Value>, on_action: OnAction) -> (u16, Arc<Mutex<Vec<String>>>) {
        fake_service_ex(trees, vec![on_action], vec![TreePackage::Echo])
    }

    /// [`fake_service`], plus two knobs, each varying by its own count (not each other's):
    /// - `on_action` — indexed **by connection** (index 0 = the first connection, clamped to
    ///   the last entry): a later connection's action can succeed after an earlier one failed,
    ///   needed to observe a reconnect's resend land.
    /// - `packages` — indexed by how many `tree`s have been served *in total*, across every
    ///   connection (same convention as `trees`): a later entry can name a different package
    ///   than asked, or omit the field, modelling a foreground that changed by the time a
    ///   reconnect re-arms.
    ///
    /// The real companion re-checks its last-served package against the active window at the
    /// moment of each `action`, refusing if the window changed since; this fake still models
    /// only the coarser gate glass#286 needed (some prior `tree`, ever, on THIS connection) via
    /// a one-way flip that never re-closes — `packages` does not make the gate itself re-check
    /// anything.
    fn fake_service_ex(
        trees: Vec<Value>,
        on_action: Vec<OnAction>,
        packages: Vec<TreePackage>,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        fake_service_full(trees, on_action, packages, vec![OnTree::Serve])
    }

    /// [`fake_service_ex`], plus `on_tree` — indexed by how many `tree` requests have *arrived*
    /// across every connection (last entry repeating), not by how many were served the way
    /// `trees`/`packages` are: a refusal serves nothing, so a served-index would trap it on its
    /// first entry and could never model a service that refuses and then starts serving. An
    /// `OnTree::Refused` entry answers `ok:false` without consulting `trees`/`packages`, and
    /// advances neither `served` nor the connection's gate.
    fn fake_service_full(
        trees: Vec<Value>,
        on_action: Vec<OnAction>,
        packages: Vec<TreePackage>,
        on_tree: Vec<OnTree>,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        let ops: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let log = Arc::clone(&ops);
        std::thread::spawn(move || {
            let mut conn = 0usize;
            let mut served = 0usize;
            let mut requested = 0usize;
            while let Ok((sock, _)) = listener.accept() {
                conn += 1;
                let this_action = on_action[(conn - 1).min(on_action.len() - 1)];
                let Ok(mut w) = sock.try_clone() else { break };
                let mut r = std::io::BufReader::new(sock);
                if writeln!(w, r#"{{"hello":{{"proto":1}}}}"#)
                    .and_then(|()| w.flush())
                    .is_err()
                {
                    break;
                }
                let mut tree_confirmed = false;
                loop {
                    let mut line = String::new();
                    if !matches!(r.read_line(&mut line), Ok(n) if n > 0) {
                        break;
                    }
                    let req: Value = serde_json::from_str(&line).expect("request is json");
                    let id = req["id"].clone();
                    let reply = match req["op"].as_str().unwrap_or_default() {
                        "tree" => {
                            let this_tree = on_tree[requested.min(on_tree.len() - 1)];
                            requested += 1;
                            let refusal = match this_tree {
                                OnTree::Refused(msg) => Some(msg),
                                OnTree::RefusedUntil(gate)
                                    if !gate.load(std::sync::atomic::Ordering::Relaxed) =>
                                {
                                    Some("no active window")
                                }
                                _ => None,
                            };
                            match refusal {
                                Some(msg) => {
                                    log.lock().unwrap().push(format!("conn{conn}:tree-refused"));
                                    json!({"id": id, "ok": false, "error": msg})
                                }
                                None => {
                                    log.lock().unwrap().push(format!("conn{conn}:tree"));
                                    let t = trees[served.min(trees.len() - 1)].clone();
                                    let pkg = &packages[served.min(packages.len() - 1)];
                                    served += 1;
                                    tree_confirmed = true;
                                    let mut reply = json!({"id": id, "ok": true, "tree": t});
                                    match pkg {
                                        TreePackage::Echo => {
                                            reply["package"] = req["package"].clone();
                                        }
                                        TreePackage::Other(p) => reply["package"] = json!(p),
                                        TreePackage::Unnamed => {}
                                    }
                                    reply
                                }
                            }
                        }
                        "action" if !tree_confirmed => {
                            log.lock()
                                .unwrap()
                                .push(format!("conn{conn}:action-refused-no-tree"));
                            json!({"id": id, "ok": false,
                                   "error": "no tree has been served on this connection"})
                        }
                        "action" => {
                            log.lock().unwrap().push(format!(
                                "conn{conn}:{} ref={}",
                                req["action"].as_str().unwrap_or_default(),
                                req["ref"]
                            ));
                            match this_action {
                                OnAction::Ok => json!({"id": id, "ok": true}),
                                OnAction::DropWithoutAnswering => break,
                            }
                        }
                        _ => json!({"id": id, "ok": true}),
                    };
                    if writeln!(w, "{reply}").and_then(|()| w.flush()).is_err() {
                        break;
                    }
                }
            }
        });
        (port, ops)
    }

    /// A reader wired to a fake device. `check_timeout` is passed explicitly so a test watching
    /// a state check time out does not wait out the production budget; `Duration::ZERO` makes it
    /// read the tree back exactly once.
    fn reader(port: u16, check_timeout: std::time::Duration) -> ServiceA11y {
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, String::new());
        a.check_timeout = check_timeout;
        a
    }

    fn ctx() -> AxContext {
        AxContext {
            pids: vec![],
            window: win(),
            window_handle: None,
            a11y_bus_addr: None,
            limits: WalkLimits::DEFAULT,
        }
    }

    fn ops_of(ops: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        ops.lock().unwrap().clone()
    }

    #[test]
    fn an_action_before_any_tree_on_this_connection_is_refused() {
        // A fresh connection — including one opened by a client reconnect — has served no
        // tree yet, so an action on it must be refused rather than answered `ok:true`.
        let (port, ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let e = client
            .click(1, "com.example.app")
            .expect_err("no tree has been served on this connection yet");
        assert!(matches!(e, CallFailure::Refused(_)), "{}", e.into_error());
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:action-refused-no-tree".to_string()]
        );
    }

    #[test]
    fn a_reconnect_re_arms_the_fresh_connection_before_resending() {
        // conn1's tree primes its gate; the set_text is then dropped in flight (a transport
        // failure, not a refusal), so the idempotent path reconnects.
        let (port, ops) = fake_service_ex(
            vec![json!({})],
            vec![OnAction::DropWithoutAnswering, OnAction::Ok],
            vec![TreePackage::Echo],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        client.tree("com.example.app").expect("primes conn1");
        client
            .set_text(1, "hi", "com.example.app")
            .expect("re-arm confirms the same package, so the resend goes through");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:set_text ref=1".to_string(),
                "conn2:tree".to_string(),
                "conn2:set_text ref=1".to_string(),
            ],
            "conn2:tree (the re-arm) must precede conn2:set_text (the resend)"
        );
    }

    #[test]
    fn a_reconnect_whose_rearm_names_a_different_package_is_refused_without_resending() {
        // The foreground moved between the dropped attempt and the reconnect: re-sending
        // anyway would authorise the action against a window the caller's ref never came from.
        let (port, ops) = fake_service_ex(
            vec![json!({})],
            vec![OnAction::DropWithoutAnswering],
            vec![
                TreePackage::Echo,
                TreePackage::Other("com.other.app".into()),
            ],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        client.tree("com.example.app").expect("primes conn1");
        let e = client
            .set_text(1, "hi", "com.example.app")
            .expect_err("a different foreground must refuse the resend");
        let msg = e.to_string();
        assert!(msg.contains("com.example.app"), "{msg}");
        assert!(msg.contains("com.other.app"), "{msg}");
        assert!(msg.contains("moved"), "{msg}");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:set_text ref=1".to_string(),
                "conn2:tree".to_string(),
            ],
            "no set_text on conn2 — the mismatch must stop the resend, not just be reported"
        );
    }

    #[test]
    fn a_reconnect_whose_rearm_names_no_package_cannot_confirm_and_is_refused() {
        // An older companion, or one that cannot name the active window, on the re-arm: absent
        // is not a match, so it cannot authorise the resend either.
        let (port, ops) = fake_service_ex(
            vec![json!({})],
            vec![OnAction::DropWithoutAnswering],
            vec![TreePackage::Echo, TreePackage::Unnamed],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        client.tree("com.example.app").expect("primes conn1");
        let e = client
            .set_text(1, "hi", "com.example.app")
            .expect_err("an unconfirmed foreground must refuse the resend");
        let msg = e.to_string();
        assert!(msg.contains("com.example.app"), "{msg}");
        assert!(msg.contains("could not confirm"), "{msg}");
        assert!(!msg.contains("moved"), "{msg}");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:set_text ref=1".to_string(),
                "conn2:tree".to_string(),
            ],
            "no set_text on conn2 — an unconfirmed foreground must stop the resend"
        );
    }

    #[test]
    fn a_rearm_refused_by_the_companion_keeps_the_original_failures_own_classification() {
        // conn1's action is dropped (AnswerLost); the re-arm on conn2 is then refused outright
        // (`Server.kt`'s `Response.error(id, "no active window")`, `CallFailure::Refused`).
        // Read via `call` directly, not `set_text`: `set_text`'s public `Result<()>` erases
        // which `CallFailure` variant it was, and that variant is exactly what this pins.
        let (port, _ops) = fake_service_full(
            vec![json!({})],
            vec![OnAction::DropWithoutAnswering],
            vec![TreePackage::Echo],
            vec![OnTree::Serve, OnTree::Refused("no active window")],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        client.tree("com.example.app").expect("primes conn1");
        let e = client
            .call(
                json!({"op": "action", "ref": 1, "action": "set_text", "text": "hi"}),
                Some("com.example.app"),
            )
            .expect_err("a refused re-arm must still fail — just not overwrite the original's classification");
        assert!(
            matches!(e, CallFailure::AnswerLost(_)),
            "conn1's own failure was AnswerLost; a re-arm the companion refuses must not \
             relabel it Refused (which a click would then report as the action itself having \
             failed, or worse — Refused's own AnswerLost sibling would read as a lost result \
             for an action that never left the host): {}",
            e.into_error()
        );
    }

    #[test]
    fn readiness_waits_for_a_served_window_not_just_a_live_socket() {
        // glass#324. Reinstalling the APK makes Android SIGKILL the process hosting the
        // AccessibilityService; the restart binds its socket — and answers `ping` — before the
        // window manager has reattached a root window, so every `tree` until then is refused
        // "no active window".
        let (port, ops) = fake_service_full(
            vec![compose_like()],
            vec![OnAction::Ok],
            vec![TreePackage::Echo],
            vec![
                OnTree::Refused("no active window"),
                OnTree::Refused("no active window"),
                OnTree::Serve,
            ],
        );
        let client =
            wait_for_ready(port, READY_ATTEMPT).expect("the third tree serves, inside the budget");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree-refused".to_string(),
                "conn1:tree-refused".to_string(),
                "conn1:tree".to_string(),
            ],
            "readiness must poll `tree` until one is served, on the one connection"
        );
        client
            .tree("com.example.app")
            .expect("the client handed back is the live, serving one");
    }

    #[test]
    fn readiness_gives_up_within_its_budget_naming_the_refusal_not_just_the_wait() {
        // The refusal never clears — a genuinely stuck service, not one mid-restart. The budget
        // is what stops the retrying, and the error must carry what the device said: a bare
        // "timed out" sends the reader hunting the host's socket, not the device's missing
        // window.
        let timeout = std::time::Duration::from_millis(400);
        let (port, ops) = fake_service_full(
            vec![compose_like()],
            vec![OnAction::Ok],
            vec![TreePackage::Echo],
            vec![OnTree::Refused("no active window")],
        );
        let started = std::time::Instant::now();
        let Err(e) = wait_for_ready(port, timeout) else {
            panic!("a service that never serves a window is not ready");
        };
        let waited = started.elapsed();
        assert!(
            waited >= timeout,
            "gave up after {waited:?}, before the budget"
        );
        assert!(
            waited < timeout * 10,
            "waited {waited:?} — the budget passed in must be the one that bounds it"
        );
        let msg = e.to_string();
        assert!(msg.contains("no active window"), "{msg}");
        let ops = ops_of(&ops);
        assert!(
            ops.len() > 1,
            "one refusal is transient, not fatal — it must be retried: {ops:?}"
        );
        assert!(ops.iter().all(|o| o == "conn1:tree-refused"), "{ops:?}");
    }

    #[test]
    fn the_disable_half_of_a_rebind_drops_our_service_and_keeps_the_devices_own() {
        // The platform unbinds a service when it leaves `enabled_accessibility_services`. Leave
        // ours in and the rebind degrades to a global `accessibility_enabled` toggle, which was
        // seen recovering the platform's own reader without recovering our binding.
        let other = "com.example.reader/.TalkBack";
        assert_eq!(
            without_service(&format!("{other}:{SERVICE_COMPONENT}")),
            other,
            "ours goes, the device's own stays"
        );
        assert_eq!(without_service(SERVICE_COMPONENT), "");
        assert_eq!(without_service(""), "");
    }

    /// A fake service that refuses every `tree` until `gate` opens.
    fn refusing_service(gate: &'static AtomicBool) -> (u16, Arc<Mutex<Vec<String>>>) {
        fake_service_full(
            vec![compose_like()],
            vec![OnAction::Ok],
            vec![TreePackage::Echo],
            vec![OnTree::RefusedUntil(gate)],
        )
    }

    #[test]
    fn a_refused_readiness_forces_a_rebind_and_the_attempt_after_it_serves() {
        // glass#324. The refusal is bimodal on a cold-booted device: readiness either succeeds
        // in a couple of seconds or is still refused 20s later, and the only thing observed to
        // clear the late case is re-establishing the service's binding.
        static SERVING: AtomicBool = AtomicBool::new(false);
        let (port, ops) = refusing_service(&SERVING);
        let steps = Mutex::new(Vec::new());
        let restores = AtomicUsize::new(0);
        let client = ready_or_restore(
            port,
            std::time::Duration::from_millis(100),
            3,
            &|step| {
                steps.lock().unwrap().push(step);
                SERVING.store(true, Ordering::Relaxed);
                Ok(())
            },
            &|| {
                restores.fetch_add(1, Ordering::Relaxed);
            },
        )
        .expect("the attempt after the rebind serves");
        assert_eq!(
            *steps.lock().unwrap(),
            vec![1],
            "one rebind, and no second one once readiness came back"
        );
        assert_eq!(
            restores.load(Ordering::Relaxed),
            0,
            "readiness succeeded — rolling the device back would tear down what it just got"
        );
        let ops = ops_of(&ops);
        assert_eq!(
            ops.last().map(String::as_str),
            Some("conn2:tree"),
            "the serve must land on the attempt after the rebind: {ops:?}"
        );
        assert!(
            ops[..ops.len() - 1]
                .iter()
                .all(|o| o == "conn1:tree-refused"),
            "{ops:?}"
        );
        client
            .tree("com.example.app")
            .expect("the client handed back is the serving one");
    }

    #[test]
    fn readiness_stops_rebinding_at_its_budget_and_says_what_it_tried() {
        // A CI log is all anyone gets: the give-up has to name what the device said, how hard
        // it was pushed to rebind, and over how long.
        static NEVER: AtomicBool = AtomicBool::new(false);
        let attempt = std::time::Duration::from_millis(200);
        let (port, ops) = refusing_service(&NEVER);
        let steps = Mutex::new(Vec::new());
        let started = std::time::Instant::now();
        let Err(e) = ready_or_restore(
            port,
            attempt,
            3,
            &|step| {
                steps.lock().unwrap().push(step);
                Ok(())
            },
            &|| {},
        ) else {
            panic!("a device that never serves a window is not ready");
        };
        let waited = started.elapsed();
        assert_eq!(
            *steps.lock().unwrap(),
            vec![1, 2],
            "a rebind between each pair of attempts, escalating, and none after the last"
        );
        assert!(
            waited >= attempt * 3,
            "gave up after {waited:?} — each attempt gets the budget it was given"
        );
        assert!(
            waited < attempt * 3 * 3,
            "waited {waited:?} — retrying must spend a bounded total, not a fresh budget each time"
        );
        let msg = e.to_string();
        assert!(msg.contains("3 readiness attempts"), "{msg}");
        assert!(msg.contains("2 forced rebinds"), "{msg}");
        assert!(msg.contains("ms total"), "{msg}");
        assert!(msg.contains("no active window"), "{msg}");
        assert!(
            ops_of(&ops).iter().all(|o| o.ends_with(":tree-refused")),
            "{ops:?}"
        );
    }

    #[test]
    fn readiness_that_gives_up_restores_the_device_exactly_once() {
        // The rollback is what keeps a failed `ensure` from leaving the service enabled and the
        // port forwarded. Retrying is where that goes wrong: once per attempt tears down the
        // settings the next attempt needs, and none at all leaks both.
        static NEVER: AtomicBool = AtomicBool::new(false);
        let (port, _ops) = refusing_service(&NEVER);
        let steps = Mutex::new(Vec::new());
        let restores = AtomicUsize::new(0);
        let Err(_) = ready_or_restore(
            port,
            std::time::Duration::from_millis(100),
            3,
            &|step| {
                steps.lock().unwrap().push(step);
                assert_eq!(
                    restores.load(Ordering::Relaxed),
                    0,
                    "a rebind after the restore would re-enable the service it just tore down"
                );
                Ok(())
            },
            &|| {
                restores.fetch_add(1, Ordering::Relaxed);
            },
        ) else {
            panic!("a device that never serves a window is not ready");
        };
        assert_eq!(*steps.lock().unwrap(), vec![1, 2], "every retry ran");
        assert_eq!(
            restores.load(Ordering::Relaxed),
            1,
            "the device is restored once, after the last attempt — not once per attempt"
        );
    }

    #[test]
    fn a_rebind_that_fails_restores_the_device_and_stops_retrying() {
        // The other give-up path: adb itself failed. Its early return is the easiest place to
        // leak an enabled service and a forwarded port.
        static NEVER: AtomicBool = AtomicBool::new(false);
        let (port, _ops) = refusing_service(&NEVER);
        let steps = Mutex::new(Vec::new());
        let restores = AtomicUsize::new(0);
        let Err(e) = ready_or_restore(
            port,
            std::time::Duration::from_millis(100),
            3,
            &|step| {
                steps.lock().unwrap().push(step);
                Err(GlassError::Backend("device offline".into()))
            },
            &|| {
                restores.fetch_add(1, Ordering::Relaxed);
            },
        ) else {
            panic!("a rebind that cannot reach the device is not readiness");
        };
        assert_eq!(
            *steps.lock().unwrap(),
            vec![1],
            "a rebind adb could not deliver is not worth repeating"
        );
        assert_eq!(
            restores.load(Ordering::Relaxed),
            1,
            "the device must still be restored when the recovery step is what failed"
        );
        let msg = e.to_string();
        assert!(msg.contains("device offline"), "{msg}");
        assert!(msg.contains("no active window"), "{msg}");
    }

    #[test]
    fn set_values_call_site_sends_its_own_configured_package_to_the_rearm() {
        // Pins `set_value`'s fallback to `self.package` when the served tree carries no subject
        // (`TreePackage::Echo`, so asked == actual): the re-arm reply's package is fixed
        // regardless of what was asked, so this only passes if the fallback is `ServiceA11y`'s
        // own configured package.
        let field = |value: &str| {
            json!({
                "class": "android.widget.FrameLayout",
                "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400},
                "children": [
                    {"class": "android.widget.EditText", "text": value, "desc": "Email",
                     "bounds": {"x": 0, "y": 100, "w": 600, "h": 120},
                     "editable": true, "clickable": true, "enabled": true}
                ]
            })
        };
        let (port, _ops) = fake_service_ex(
            vec![field("old"), field("old"), field("new")],
            vec![OnAction::DropWithoutAnswering, OnAction::Ok],
            vec![
                TreePackage::Echo,
                TreePackage::Other("com.example.app".into()),
            ],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        let t = built(&field("old"));
        let target = target_for(&t, AxNodeId(1));
        a.set_value(&ctx(), &target, "new")
            .expect("the caller's own configured package must match the rearm's fixed reply");
    }

    #[test]
    fn invokes_call_site_sends_its_own_configured_package_to_the_rearm() {
        // Mirrors the `set_value` test above, for `invoke`'s fallback to `self.package` when the
        // plan's tree carries no subject. `click`'s resend fires only on a real
        // `CallFailure::NotSent`, never `AnswerLost`, and that failure only arises strictly
        // BETWEEN `invoke`'s snapshot/plan step and its click — a window `invoke`'s own single
        // call never exposes to a caller. So this drives `invoke`'s own two halves directly —
        // snapshot/plan, then `dispatch_click` (split out for exactly this seam) — with a real
        // `shutdown(Write)` between them.
        let clickable = json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400},
            "children": [
                {"class": "android.widget.Button", "desc": "Save",
                 "bounds": {"x": 0, "y": 100, "w": 200, "h": 100},
                 "clickable": true, "enabled": true}
            ]
        });
        let (port, _ops) = fake_service_ex(
            vec![clickable.clone()],
            vec![OnAction::Ok],
            vec![
                TreePackage::Echo,
                TreePackage::Other("com.example.app".into()),
            ],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        let t = built(&clickable);
        let target = target_for(&t, AxNodeId(1));

        // `invoke`'s own first half, called directly: snapshot + resolve the plan.
        let mut tree = a.snapshot(&ctx()).expect("snapshot");
        tree.assign_ids();
        let plan = invoke_plan(&tree, &target).expect("plan resolves");

        // Break the connection's write half — deterministic, not raced: this line has already
        // returned before `dispatch_click` is called below.
        a.client
            .conn
            .lock()
            .expect("lock")
            .writer
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");

        a.dispatch_click(&ctx(), &target, &plan, tree.subject.as_ref())
            .expect("the caller's own configured package must match the rearm's fixed reply");
    }

    #[test]
    fn dispatch_clicks_rearm_asks_about_the_acting_app_not_the_asked_about_one() {
        // Sibling of the test above, on the same `shutdown(Write)` seam, but with a subject:
        // the served tree answered for `com.other.app`, not the configured
        // `com.example.app` — `target`'s ref is numbered from THAT tree. The rearm's reply names
        // the ASKED app instead. Comparing against the asked app (the bug) would match by
        // construction and replay the click against whatever unrelated node ref 1 resolves to in
        // the real app; comparing against the acting app (`com.other.app`) sees the mismatch and
        // refuses.
        let clickable = json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400},
            "children": [
                {"class": "android.widget.Button", "desc": "Save",
                 "bounds": {"x": 0, "y": 100, "w": 200, "h": 100},
                 "clickable": true, "enabled": true}
            ]
        });
        let (port, ops) = fake_service_ex(
            vec![clickable.clone()],
            vec![OnAction::Ok],
            vec![
                TreePackage::Other("com.other.app".into()),
                TreePackage::Other("com.example.app".into()),
            ],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        let t = built(&clickable);
        let target = target_for(&t, AxNodeId(1));

        let mut tree = a.snapshot(&ctx()).expect("snapshot");
        tree.assign_ids();
        let plan = invoke_plan(&tree, &target).expect("plan resolves");

        a.client
            .conn
            .lock()
            .expect("lock")
            .writer
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");

        let e = a
            .dispatch_click(&ctx(), &target, &plan, tree.subject.as_ref())
            .expect_err("the rearm names the asked-about app, not the app the ref came from");
        let msg = e.to_string();
        assert!(msg.contains("com.other.app"), "{msg}");
        assert!(msg.contains("com.example.app"), "{msg}");
        assert!(msg.contains("moved"), "{msg}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string(), "conn2:tree".to_string()],
            "no click on conn2 — comparing against the acting app must stop the resend"
        );
    }

    /// A field whose editable EditText reports `value`, wrapped exactly like the fixture used by
    /// [`set_values_call_site_sends_its_own_configured_package_to_the_rearm`].
    fn editable_field(value: &str) -> Value {
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400},
            "children": [
                {"class": "android.widget.EditText", "text": value, "desc": "Email",
                 "bounds": {"x": 0, "y": 100, "w": 600, "h": 120},
                 "editable": true, "clickable": true, "enabled": true}
            ]
        })
    }

    #[test]
    fn a_rearm_matching_the_asked_about_app_does_not_authorise_a_stale_acting_apps_ref() {
        // The served tree answered for `com.other.app` (a dialog), not the configured
        // `com.example.app` — `target`'s ref is numbered from the DIALOG's nodes. `set_text` is
        // dropped in flight; while reconnecting the dialog dismisses and `com.example.app` (the
        // asked-about app) comes forward, so the rearm's reply names it. Comparing against the
        // ASKED app (the bug) would match by construction here and replay the write against
        // whatever unrelated node ref 1 resolves to in the real app — it must instead compare
        // against the ACTING app (`com.other.app`), see the reply naming something else, and
        // refuse.
        let (port, ops) = fake_service_ex(
            vec![
                editable_field("old"),
                editable_field("old"),
                editable_field("new"),
            ],
            vec![OnAction::DropWithoutAnswering, OnAction::Ok],
            vec![
                TreePackage::Other("com.other.app".into()),
                TreePackage::Other("com.example.app".into()),
            ],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        let t = built(&editable_field("old"));
        let target = target_for(&t, AxNodeId(1));

        // `OnAction::Ok` is wired for conn2 so a wrongly-authorised resend would go on to
        // SUCCEED — the false success this pins, not just a differently-worded refusal.
        let e = a
            .set_value(&ctx(), &target, "new")
            .expect_err("the rearm names the asked-about app, not the dialog the ref came from");
        let msg = e.to_string();
        assert!(msg.contains("com.other.app"), "{msg}");
        assert!(msg.contains("com.example.app"), "{msg}");
        assert!(msg.contains("moved"), "{msg}");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:set_text ref=1".to_string(),
                "conn2:tree".to_string(),
            ],
            "no set_text on conn2 — comparing against the acting app must stop the resend"
        );
    }

    #[test]
    fn a_rearm_matching_the_acting_app_resends_even_though_it_differs_from_the_asked_about_app() {
        // Mirrors the test above with the dialog still foreground at the rearm: the reply again
        // names `com.other.app`, the same app the served tree described, so this is safe to
        // resend even though it still differs from the configured `com.example.app`. Comparing
        // against the asked app (the bug) would see that mismatch and spuriously refuse a resend
        // that was actually safe.
        let (port, ops) = fake_service_ex(
            vec![
                editable_field("old"),
                editable_field("old"),
                editable_field("new"),
            ],
            vec![OnAction::DropWithoutAnswering, OnAction::Ok],
            vec![
                TreePackage::Other("com.other.app".into()),
                TreePackage::Other("com.other.app".into()),
            ],
        );
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        let t = built(&editable_field("old"));
        let target = target_for(&t, AxNodeId(1));

        a.set_value(&ctx(), &target, "new")
            .expect("the acting app did not change, so the resend must go through");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:set_text ref=1".to_string(),
                "conn2:tree".to_string(),
                "conn2:set_text ref=1".to_string(),
                "conn2:tree".to_string(),
            ],
            "conn2:set_text (the resend) must follow conn2:tree (the rearm); the last conn2:tree \
             is set_value's own post-write verification read"
        );
    }

    #[test]
    fn snapshot_discloses_a_reply_naming_a_different_app() {
        // Exercises `ServiceA11y::snapshot`'s wiring end-to-end (asked vs. the reply's
        // `package`), not just the pure `subject_of` comparison tested above in isolation.
        let listener = std::net::TcpListener::bind(("127.0.0.1", 0)).expect("bind");
        let port = listener.local_addr().expect("addr").port();
        std::thread::spawn(move || {
            let (sock, _) = listener.accept().expect("accept");
            let mut w = sock.try_clone().expect("clone");
            let mut r = std::io::BufReader::new(sock);
            writeln!(w, r#"{{"hello":{{"proto":1}}}}"#)
                .and_then(|()| w.flush())
                .expect("hello");
            let mut line = String::new();
            r.read_line(&mut line).expect("read request");
            let req: Value = serde_json::from_str(&line).expect("request is json");
            let reply = json!({
                "id": req["id"], "ok": true, "tree": compose_like(),
                "package": "com.google.android.permissioncontroller"
            });
            writeln!(w, "{reply}")
                .and_then(|()| w.flush())
                .expect("write reply");
        });
        let client = ServiceClient::connect(port).expect("connect");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        let tree = a.snapshot(&ctx()).expect("snapshot");
        assert_eq!(
            tree.subject,
            Some(Subject {
                asked: "com.example.app".into(),
                actual: "com.google.android.permissioncontroller".into(),
            })
        );
    }

    #[test]
    fn a_click_whose_answer_was_lost_is_not_re_sent() {
        // The write landed and the response did not come back. Re-sending on a fresh socket,
        // as a transparent reconnect does, dispatches a second ACTION_CLICK and reports one
        // successful native click.
        let (port, ops) = fake_service(vec![compose_like()], OnAction::DropWithoutAnswering);
        let mut a = reader(port, std::time::Duration::ZERO);
        let t = built(&compose_like());
        let e = a
            .invoke(&ctx(), &target_for(&t, AxNodeId(2)))
            .expect_err("a lost answer is not a success");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert!(
            e.to_string().contains("may or may not have actuated"),
            "{e}"
        );
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string(), "conn1:click ref=1".to_string()],
            "exactly one click frame, on the one connection"
        );
    }

    #[test]
    fn the_click_is_sent_to_the_node_the_climb_resolved_to() {
        // Sending the caller's own id would click the label, which handles nothing — every
        // Compose click a no-op reported as success.
        let (port, ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let t = built(&compose_like());
        let substituted = a
            .invoke(&ctx(), &target_for(&t, AxNodeId(2)))
            .expect("clicks");
        assert_eq!(substituted, Some(AxNodeId(1)), "the climb is disclosed");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string(), "conn1:click ref=1".to_string()]
        );
    }

    #[test]
    fn a_disabled_target_sends_no_click_at_all() {
        let json = disabled_inside_a_clickable_card();
        let (port, ops) = fake_service(vec![json.clone()], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let t = built(&json);
        let e = a
            .invoke(&ctx(), &target_for(&t, AxNodeId(2)))
            .expect_err("a disabled control must not report success");
        assert!(e.to_string().contains("disabled"), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string()],
            "the refusal must happen before anything is dispatched"
        );
    }

    #[test]
    fn a_toggle_whose_state_never_moves_is_reported_failed() {
        let stuck = one_checkable("android.widget.CheckBox", false);
        let (port, ops) = fake_service(vec![stuck.clone()], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let t = built(&stuck);
        let e = a
            .invoke(&ctx(), &target_for(&t, AxNodeId(1)))
            .expect_err("an acknowledged action that changed nothing is not a click");
        assert!(e.to_string().contains("accepted"), "{e}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:click ref=1".to_string(),
                "conn1:tree".to_string()
            ],
            "the click went out and the state was read back"
        );
    }

    #[test]
    fn a_toggle_that_flips_is_reported_clicked() {
        let (port, _) = fake_service(
            vec![
                one_checkable("android.widget.CheckBox", false),
                one_checkable("android.widget.CheckBox", true),
            ],
            OnAction::Ok,
        );
        let mut a = reader(port, std::time::Duration::ZERO);
        let t = built(&one_checkable("android.widget.CheckBox", false));
        assert_eq!(
            a.invoke(&ctx(), &target_for(&t, AxNodeId(1)))
                .expect("clicks"),
            None,
            "the target actuated itself, so nothing was substituted"
        );
    }
}
