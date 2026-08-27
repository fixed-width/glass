//! `ServiceA11y` — the on-device-AccessibilityService a11y reader. Talks the `tree`/`action`
//! line-JSON protocol to `glass-a11y.apk` over an `adb forward`ed socket, and maps the live
//! `AccessibilityNodeInfo` tree (sent as JSON) into glass's `AxTree`.

use std::sync::Mutex;

use serde_json::{Value, json};

use glass_core::Deadline;
use glass_core::accessibility::{
    Accessibility, AxContext, AxNode, AxNodeId, AxRect, AxRole, AxStates, AxTarget, AxTree,
    Subject, WalkBudget, WalkLimits,
};
use glass_core::platform::WindowGeometry;
use glass_core::{GlassError, Result, read_back_failed, write_took_no_effect};

use crate::axmap::{LabelInputs, class_to_role, demote_web_hosts, labels};
use crate::conn::{CallFailure, Conn};

/// One walk of a device `tree` reply: the bounds it runs under, and the device `ref` of every
/// node it keeps, in the pre-order those nodes are about to be numbered in.
struct Walk {
    budget: WalkBudget,
    refs: Vec<u32>,
}

impl Walk {
    fn new(limits: WalkLimits) -> Walk {
        Walk {
            budget: WalkBudget::with_limits(limits),
            refs: Vec::new(),
        }
    }
}

/// Map one device `tree` JSON node (+descendants) into an `AxNode`, converting screen bounds to
/// window-relative, and record its device `ref` in `walk`. Ids are left `AxNodeId(0)`;
/// `tree_from_json` numbers them pre-order (root = 0).
///
/// The `ref` is read, never inferred from walk order. The two numberings agree only while the
/// host keeps every node the device sent: the `Depth` and `Siblings` caps drop a subtree and
/// carry on with later siblings, so past the first pruned subtree a host id is *lower* than the
/// device's own `ref` for that node (glass#288).
///
/// The push happens on entry, before the children, so `walk.refs` ends up in the same pre-order
/// [`AxTree::assign_ids`] numbers by — `refs[n]` is the node numbered `AxNodeId(n)`.
fn json_to_node(v: &Value, win: &WindowGeometry, depth: usize, walk: &mut Walk) -> Result<AxNode> {
    walk.budget.visit();
    // Dropping the node would shift every later id, so a missing ref fails the whole snapshot
    // like malformed bounds below. Every released companion numbers every node.
    let device_ref = v
        .get("ref")
        .and_then(Value::as_u64)
        .and_then(|n| u32::try_from(n).ok())
        .ok_or_else(|| {
            GlassError::AccessibilityUnavailable("tree node carries no usable ref".into())
        })?;
    walk.refs.push(device_ref);
    let cls = v.get("class").and_then(Value::as_str).unwrap_or("");
    let flag = |k: &str| v.get(k).and_then(Value::as_bool).unwrap_or(false);
    // The device omits empty text; preserve it as empty for editable nodes.
    let text = v
        .get("text")
        .and_then(Value::as_str)
        .or_else(|| flag("editable").then_some(""));
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
    // The child array is resolved before any bound is consulted: a childless node must
    // never be reported truncated for declining to explore a list that was already empty.
    let children = match v.get("children").and_then(Value::as_array) {
        None => vec![],
        Some(arr) if arr.is_empty() => vec![],
        Some(_) if !walk.budget.may_explore_children(depth) => vec![],
        Some(arr) => {
            let mut out = Vec::new();
            for (i, c) in arr.iter().enumerate() {
                if !walk.budget.may_visit_sibling(i) {
                    break;
                }
                out.push(json_to_node(c, win, depth + 1, walk)?);
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
            secure: flag("password"),
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

/// A tree read off the device, paired with the device `ref` of each of its nodes.
///
/// The halves are only meaningful together: `refs[n]` is the `ref` the device gave the node this
/// host numbered `AxNodeId(n)`, so an action a caller addresses by host id can be dispatched to
/// the node the *device* knows by that name.
#[derive(Debug)]
pub(crate) struct RefTree {
    pub(crate) tree: AxTree,
    refs: Vec<u32>,
}

impl RefTree {
    /// The device `ref` for a node of this tree.
    fn device_ref(&self, id: AxNodeId) -> Result<u32> {
        self.refs
            .get(id.0 as usize)
            .copied()
            .ok_or(GlassError::AxElementNotFound(id.0))
    }

    /// The app whose nodes these refs number: what the tree actually described, falling back to
    /// what was asked about when the companion made no claim. A ref is only meaningful against
    /// that app — see [`rearm_tree`].
    fn acting_on<'a>(&'a self, asked: &'a str) -> &'a str {
        self.tree.subject.as_ref().map_or(asked, |s| &s.actual)
    }
}

/// Build the tree from a device `tree` response value (the `"tree"` object), keeping each node's
/// device `ref`.
pub(crate) fn tree_from_json(
    tree: &Value,
    win: &WindowGeometry,
    limits: WalkLimits,
) -> Result<RefTree> {
    let mut walk = Walk::new(limits);
    let mut root = json_to_node(tree, win, 0, &mut walk)?;
    demote_web_hosts(&mut root);
    // The device answers with the root of the ACTIVE WINDOW, so this node is the window
    // whatever layout class it carries. Both Android readers have to agree about the root, or
    // a `role:` selector written against one misses on the other.
    //
    // `raw_role` keeps the device's class on the node, though it no longer reaches the outline,
    // which names a native token only for an element glass has no role for.
    root.role = AxRole::Window;
    let mut tree = AxTree::new(root);
    tree.truncated = walk.budget.truncation();
    // Numbered here rather than by the caller: `refs` is indexed by the id `assign_ids` hands a
    // node. The session re-runs it on every snapshot, which is a no-op.
    tree.assign_ids();
    debug_assert_eq!(
        tree.count,
        walk.refs.len(),
        "one ref per kept node, or the index is meaningless"
    );
    Ok(RefTree {
        tree,
        refs: walk.refs,
    })
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

    /// Bound the connection's reads by `deadline` for as long as the returned guard lives, and
    /// restore the standing timeout when it drops.
    ///
    /// Scoped rather than set-and-leave: the connection outlives any one request, so a bound left
    /// behind would apply to a later call that never agreed to it — and to the *reconnect* path,
    /// where a fresh `Conn` starts from the standing timeout again.
    fn read_within(&self, deadline: Deadline) -> ReadBound<'_> {
        if let Ok(mut conn) = self.conn.lock() {
            conn.read_within(deadline.remaining());
        }
        ReadBound { client: self }
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

    /// Serve the tree for `package` on the connection's standing timeout — the readiness probe and
    /// the tests, neither of which has a caller waiting on a clock.
    fn tree(&self, package: &str) -> Result<TreeReply> {
        self.tree_within(package, Deadline::UNBOUNDED)
    }

    /// Serve the tree for `package`, blocking no longer than `deadline` allows.
    ///
    /// The bound covers the reconnect-and-re-send too, so one call cannot cost two socket
    /// timeouts against a caller who asked for less than one (glass#338).
    fn tree_within(&self, package: &str, deadline: Deadline) -> Result<TreeReply> {
        let _bound = self.read_within(deadline);
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
    fn set_text(
        &self,
        ref_id: u32,
        text: &str,
        package: &str,
    ) -> std::result::Result<(), CallFailure> {
        self.call(
            json!({"op": "action", "ref": ref_id, "action": "set_text", "text": text}),
            Some(package),
        )
        .map(|_| ())
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
    /// How long [`Accessibility::set_value`] polls for the write; [`WRITE_VERIFY_BUDGET`] outside
    /// tests.
    write_verify_budget: std::time::Duration,
}

impl ServiceA11y {
    pub fn new(client: ServiceClient, package: String) -> Self {
        Self {
            client,
            package,
            check_timeout: CHECK_TIMEOUT,
            write_verify_budget: WRITE_VERIFY_BUDGET,
        }
    }

    /// Poll until the actuated control reports `checked == want`. The toolkit's UI update
    /// reaches the accessibility tree a beat after the action; `set_value` polls for the same
    /// reason.
    fn wait_for_check(&mut self, ctx: &AxContext, plan: &InvokePlan, want: bool) -> Result<()> {
        let deadline = std::time::Instant::now() + self.check_timeout;
        loop {
            let after = self.snapshot(ctx)?;
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
    /// `rt` is the tree the plan was resolved against, passed separately from `plan`: it borrows
    /// the tree, not `self`, so the caller can still hand it to this `&mut self` method. It is
    /// also the only thing that can say which device `ref` the actuated node has.
    fn dispatch_click(
        &mut self,
        ctx: &AxContext,
        target: &AxTarget,
        plan: &InvokePlan,
        rt: &RefTree,
    ) -> Result<Option<AxNodeId>> {
        let device_ref = rt.device_ref(plan.actuated.id)?;
        self.client
            .click(device_ref, rt.acting_on(&self.package))
            .map_err(|f| action_error(target.id.0, f))?;
        if let Some(want) = plan.want_checked {
            self.wait_for_check(ctx, plan, want)?;
        }
        Ok(plan.substituted())
    }

    /// [`Accessibility::snapshot`], keeping each node's device `ref` — what an action addressed
    /// by a host id has to be dispatched by.
    fn snapshot_with_refs(&mut self, ctx: &AxContext) -> Result<RefTree> {
        // Checked before the round-trip: a companion that answers after the caller has gone costs
        // a socket timeout nobody is waiting through.
        if ctx.deadline.has_passed() {
            return Err(GlassError::AccessibilityNotReady(
                "no accessibility tree within the time this call allowed".into(),
            ));
        }
        let reply = self.client.tree_within(&self.package, ctx.deadline)?;
        let mut rt = tree_from_json(&reply.tree, &ctx.window, ctx.limits)?;
        rt.tree.subject = subject_of(&self.package, reply.package.as_deref());
        Ok(rt)
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

/// Restores [`ServiceClient`]'s standing read timeout when it drops, so a bound one call set
/// cannot be inherited by the next.
struct ReadBound<'a> {
    client: &'a ServiceClient,
}

impl Drop for ReadBound<'_> {
    fn drop(&mut self) {
        if let Ok(mut conn) = self.client.conn.lock() {
            conn.read_within(None);
        }
    }
}

/// What a write that took no effect looks like on this backend: `ACTION_SET_TEXT` reports success
/// and silently does nothing when it would *replace* text already in a Compose field.
///
/// `GLASS_ANDROID_A11Y=off`, not unsetting `GLASS_ANDROID_A11Y_APK`: the apk is also picked up from
/// the data dirs and from beside the binary, so unsetting the path leaves this reader selected.
const COMPOSE_SET_TEXT_NO_OP: &str = "a Compose field that already holds text cannot be replaced via ACTION_SET_TEXT — clear it \
     first, or set GLASS_ANDROID_A11Y=off to use the uiautomator backend";

/// How long [`ServiceA11y::set_value`] polls for a write to reach the tree, and the gap between
/// those reads. Separate from [`CHECK_TIMEOUT`]: a recompose reaching accessibility is not the same
/// wait as a control's state settling, and a test bounding one must not silently bound the other.
const WRITE_VERIFY_BUDGET: std::time::Duration = std::time::Duration::from_secs(2);
/// See [`WRITE_VERIFY_BUDGET`].
const WRITE_VERIFY_POLL: std::time::Duration = std::time::Duration::from_millis(150);

impl Accessibility for ServiceA11y {
    fn snapshot(&mut self, ctx: &AxContext) -> Result<AxTree> {
        Ok(self.snapshot_with_refs(ctx)?.tree)
    }

    fn set_value(&mut self, ctx: &AxContext, target: &AxTarget, text: &str) -> Result<()> {
        // Guard: re-snapshot and verify the id still points at the same editable element before
        // acting — `invoke`'s guard plus the editable check.
        let rt = self.snapshot_with_refs(ctx)?;
        // Addressed by the node the guard approved, not by the id the caller named, so a guard
        // that ever relaxes into a search cannot dispatch to a node it never approved.
        let device_ref = rt.device_ref(resolved_editable_target(&rt.tree, target)?.id)?;
        self.client
            .set_text(device_ref, text, rt.acting_on(&self.package))
            .map_err(|f| write_error(target.id.0, f))?;
        // Verify the value actually took. ACTION_SET_TEXT returns success but silently no-ops when
        // *replacing* existing text in a Compose field, so a bare Ok could lie (glass forbids silent
        // fallbacks). The set is async (Compose recompose → a11y update), so poll briefly for the
        // value to land; error honestly only on timeout.
        let deadline = std::time::Instant::now() + self.write_verify_budget;
        loop {
            // Every exit below is post-dispatch, so each must be a verdict
            // `set_value_failed_after_writing` accepts, or the session keeps the value it cached
            // for a write that went out and refuses the retry as drift (glass#405).
            let after = self
                .snapshot(ctx)
                .map_err(|e| read_back_failed(target, &e))?;
            // `find` answers `None` for a node that is gone and for one holding nothing, and
            // folding them together confirms a clear on the evidence that the element could not be
            // found. Keep polling — a tree mid-update loses it briefly.
            let Some(node) = after.find(target.id) else {
                if std::time::Instant::now() >= deadline {
                    return Err(GlassError::AxWriteUnconfirmed(
                        target.id.0,
                        "the element is no longer in the tree, so its value could not be read back"
                            .into(),
                    ));
                }
                std::thread::sleep(WRITE_VERIFY_POLL);
                continue;
            };
            // Before the value check, not after: a collapsed row maps its text to `name` and
            // reports no value, so `"" == ""` would confirm a *clear* on the evidence that the
            // field stopped being a field. `AndroidA11y` re-checks it too, through the shared
            // `glass_core::verify_typed_write`.
            if !node.states.editable {
                return Err(GlassError::AxWriteUnconfirmed(
                    target.id.0,
                    "the element no longer reports itself editable, so its value cannot be read \
                     back"
                        .into(),
                ));
            }
            let got = node.value.clone();
            // An empty field reports no value (None), not Some(""), so compare against "".
            if got.as_deref().unwrap_or("") == text {
                return Ok(());
            }
            if std::time::Instant::now() >= deadline {
                // The mapper drops an empty value, so an emptied field arrives as `None` — which
                // the verdict reserves for a reading nobody took.
                let observed = got.as_deref().unwrap_or("");
                // The clause explains a field that never moved; on one holding something else the
                // write did reach it, and "clear it first" would name a field already empty.
                return Err(if write_took_no_effect(observed, target.value.as_deref()) {
                    GlassError::value_not_applied_because(
                        target.id.0,
                        text,
                        Some(observed),
                        COMPOSE_SET_TEXT_NO_OP,
                    )
                } else {
                    GlassError::value_not_applied(target.id.0, text, Some(observed))
                });
            }
            std::thread::sleep(WRITE_VERIFY_POLL);
        }
    }

    fn invoke(&mut self, ctx: &AxContext, target: &AxTarget) -> Result<Option<AxNodeId>> {
        let rt = self.snapshot_with_refs(ctx)?;
        let plan = invoke_plan(&rt.tree, target)?;
        self.dispatch_click(ctx, target, &plan, &rt)
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
///
/// Read from what the tool said, never from the rendered message: that also names the argv, so an
/// APK path quoting either phrase sent every install failure down the uninstall branch — the
/// hazard `AdbOp::for_args` documents for argv (glass#348).
fn is_signature_mismatch(e: &GlassError) -> bool {
    e.tool_said().is_some_and(|said| {
        said.contains("INSTALL_FAILED_UPDATE_INCOMPATIBLE")
            || said.contains("signatures do not match")
    })
}

/// Install the service APK, recovering from a signature mismatch. glass owns this package
/// (install → enable → teardown, no meaningful user state), so when a differently-signed
/// build is already present it removes the stale copy and installs fresh rather than failing.
fn install_service(adb: &Adb, apk: &str) -> Result<()> {
    match adb.run(["install", "-r", apk]) {
        Ok(_) => Ok(()),
        Err(e) if is_signature_mismatch(&e) => {
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

pub(crate) struct Active {
    /// The client the service was enabled through, kept rather than resolved again at teardown:
    /// `Adb::from_env` reads the environment as it is *then*, which need not be what set this up.
    adb: Adb,
    port: u16,
    prior_enabled: String,
    prior_a11y_enabled: String,
}

// Unix-gated with its only consumer, the `FakeAdb`-backed teardown test — that fake is a
// `/bin/sh` script.
#[cfg(all(test, unix))]
impl Active {
    /// State a shutdown has something to undo.
    pub(crate) fn for_test(adb: Adb, port: u16) -> Self {
        Self {
            adb,
            port,
            prior_enabled: String::new(),
            prior_a11y_enabled: "0".to_string(),
        }
    }
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
            &|step| escalate(adb, apk, &want, step, READY_ATTEMPTS),
            // Unbounded: this is the rollback for a failed `ensure` during `glass_start`, not
            // teardown, so each call keeps its own budget.
            &|| restore_a11y(adb, prior, prior_a11y, port, Deadline::UNBOUNDED),
        )?;
        *self.state.lock().unwrap() = Some(Active {
            adb: adb.clone(),
            port,
            prior_enabled: prior.to_string(),
            prior_a11y_enabled: prior_a11y.to_string(),
        });
        Ok(client)
    }

    /// Plant an active service, for a test that needs a registry with something to restore.
    #[cfg(all(test, unix))]
    pub(crate) fn set_active_for_test(&self, active: Active) {
        *self.state.lock().unwrap() = Some(active);
    }

    /// Restore the device's prior accessibility state and remove the forward by `deadline`, which
    /// the rest of teardown shares (glass#422).
    ///
    /// Best-effort and idempotent. No process to kill: disabling unbinds the service.
    pub fn shutdown(&self, deadline: Deadline) {
        if let Ok(mut g) = self.state.lock()
            && let Some(a) = g.take()
        {
            restore_a11y(
                &a.adb,
                &a.prior_enabled,
                &a.prior_a11y_enabled,
                a.port,
                deadline,
            );
        }
    }
}

/// The recovery for the `step`th readiness attempt refused for its whole budget.
///
/// The reinstall is last, not first: it is what SIGKILLs the service and starts this race, and
/// also the only step observed to bring a stuck binding back.
fn escalate(adb: &Adb, apk: &str, want: &str, step: u32, attempts: u32) -> Result<()> {
    if step + 1 == attempts {
        install_service(adb, apk)?;
    }
    force_rebind(adb, want)
}

fn put_secure(adb: &Adb, key: &str, value: &str) -> Result<()> {
    put_secure_until(adb, key, value, Deadline::UNBOUNDED).map(|_| ())
}

/// [`put_secure`] under `deadline`.
fn put_secure_until(adb: &Adb, key: &str, value: &str, deadline: Deadline) -> Result<String> {
    adb.run_until(["shell", "settings", "put", "secure", key, value], deadline)
}

/// Write `enabled_accessibility_services`. An empty list is a `delete`: `settings put ... ""`
/// errors ("Bad arguments").
fn put_enabled_services(adb: &Adb, list: &str) -> Result<()> {
    put_enabled_services_until(adb, list, Deadline::UNBOUNDED)
}

/// [`put_enabled_services`] under `deadline`.
fn put_enabled_services_until(adb: &Adb, list: &str, deadline: Deadline) -> Result<()> {
    if list.is_empty() {
        adb.run_until(
            [
                "shell",
                "settings",
                "delete",
                "secure",
                "enabled_accessibility_services",
            ],
            deadline,
        )
        .map(|_| ())
    } else {
        put_secure_until(adb, "enabled_accessibility_services", list, deadline).map(|_| ())
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
fn restore_a11y(
    adb: &Adb,
    prior_enabled: &str,
    prior_a11y_enabled: &str,
    port: u16,
    deadline: Deadline,
) {
    let services = put_enabled_services_until(adb, prior_enabled, deadline);
    glass_core::note_if_skipped("restoring enabled_accessibility_services", &services);
    let enabled = put_secure_until(adb, "accessibility_enabled", prior_a11y_enabled, deadline);
    glass_core::note_if_skipped("restoring accessibility_enabled", &enabled);
    let forward = adb.run_until(["forward", "--remove", &format!("tcp:{port}")], deadline);
    glass_core::note_if_skipped("removing the a11y service's adb forward", &forward);
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

/// Whether `node` is the element `target` names, wherever it now sits: same role, name and value,
/// and exactly the same size.
fn same_element_moved(target: &AxTarget, node: &AxNode) -> bool {
    target.matches(node.role, node.name.as_deref())
        && target.value_consistent(node.value.as_deref())
        && matches!(
            (target.bounds, node.bounds),
            (Some(a), Some(b)) if a.width == b.width && a.height == b.height
        )
}

/// Every node [`same_element_moved`] accepts for `target`, in pre-order. Searched tree-wide, not
/// among siblings, so a second identical control anywhere refuses the whole relaxation — a list of
/// same-size items sharing one label keeps the strict bounds guard.
fn movement_candidates<'a>(tree: &'a AxTree, target: &AxTarget) -> Vec<&'a AxNode> {
    fn walk<'a>(node: &'a AxNode, target: &AxTarget, out: &mut Vec<&'a AxNode>) {
        if same_element_moved(target, node) {
            out.push(node);
        }
        for c in &node.children {
            walk(c, target, out);
        }
    }
    let mut out = Vec::new();
    walk(&tree.root, target, &mut out);
    out
}

/// A rectangle as `(x,y wxh)`.
fn rect(b: Option<AxRect>) -> String {
    b.map_or_else(|| "(no bounds)".to_string(), |r| r.to_string())
}

/// The refusal for a target more than one node now matches on everything but position, so its id
/// no longer picks one of them out.
fn indistinguishable(target: &AxTarget, candidates: &[&AxNode]) -> GlassError {
    let at: Vec<String> = candidates.iter().map(|n| rect(n.bounds)).collect();
    GlassError::AxActionFailed(
        target.id.0,
        format!(
            "{} elements now match its role, name, value and size — at {} — and it is no longer \
             at {}, so which one it is cannot be told and nothing was dispatched; re-snapshot",
            candidates.len(),
            at.join(", "),
            rect(target.bounds),
        ),
    )
}

/// [`crate::a11y::fingerprinted`], with a control's *position* taken out of its identity.
///
/// glass#326: an activity-open transition slides a control tens of px while the caller's snapshot
/// still holds where it was, and the strict guard refuses it although role, name and value all
/// match and only x differs.
///
/// Do not widen further. The node is still the one on `target.id` — nothing is searched for — its
/// size must match exactly, and it is accepted only while no other node in the tree carries the
/// same role, name, value and size. That uniqueness stands in for the bounds comparison: what
/// `AxTarget::bounds` guards against is a *different* same-role+name element inheriting the id
/// after drift, and there is no different one here to inherit it. Two candidates and this refuses.
///
/// `AndroidA11y`'s `set_value` keeps the strict core helper; only this reader relaxes.
fn resolved_target<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    let strict = match crate::a11y::fingerprinted(tree, target) {
        Ok(node) => return Ok(node),
        Err(e) => e,
    };
    // Only the bounds verdict is revisited; an absent target and every other refusal pass through.
    if !matches!(strict, GlassError::AxElementChanged(_)) {
        return Err(strict);
    }
    let Some(node) = tree.find(target.id) else {
        return Err(strict);
    };
    if !same_element_moved(target, node) {
        return Err(strict);
    }
    let candidates = movement_candidates(tree, target);
    if candidates.len() == 1 {
        return Ok(node);
    }
    Err(indistinguishable(target, &candidates))
}

/// [`resolved_target`], plus [`crate::a11y::editable_target`]'s editable check. `set_value` shares
/// `invoke`'s guard, so it shares the relaxation.
fn resolved_editable_target<'a>(tree: &'a AxTree, target: &AxTarget) -> Result<&'a AxNode> {
    let node = resolved_target(tree, target)?;
    if !node.states.editable {
        return Err(GlassError::AxElementNotEditable(target.id.0));
    }
    Ok(node)
}

/// Decide what an `invoke` does with `target`: which node to actuate, and what the tree must
/// show afterwards. Pure, so the decision is testable without a device.
///
/// The order is load-bearing: a Compose control omits `ACTION_CLICK` while disabled, so a climb
/// run before the target's own `enabled` check walks past it and actuates whatever encloses it.
fn invoke_plan(tree: &AxTree, target: &AxTarget) -> Result<InvokePlan> {
    let node = resolved_target(tree, target)?;
    if !node.states.enabled {
        return Err(disabled_error(target.id.0, target.id));
    }
    let chosen = actuable_node(tree, target)?;
    if !chosen.states.enabled {
        return Err(disabled_error(target.id.0, chosen.id));
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

/// Map a `set_text` failure onto the `set_value` contract. Only `NotSent` proves nothing was
/// written — the other two may already have set the field, so they answer in a verdict
/// `set_value_failed_after_writing` accepts (glass#405).
fn write_error(target: u32, f: CallFailure) -> GlassError {
    match f {
        CallFailure::NotSent(e) => GlassError::AccessibilityUnavailable(format!(
            "the write to element {target} could not be sent: {e}"
        )),
        CallFailure::AnswerLost(e) => GlassError::AxWriteUnconfirmed(
            target,
            format!("the write was sent and its result was lost ({e})"),
        ),
        CallFailure::Refused(e) => GlassError::AxWriteUnconfirmed(
            target,
            format!("the device answered the write with something unreadable ({e})"),
        ),
    }
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
    use glass_core::accessibility::{AxRole, TruncationLimit};
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

    /// Number every node that carries no `ref` the way the companion's `treeJson` does —
    /// pre-order, root = 0 — so a fixture reads like a real device reply without restating what
    /// every companion release already sends. A fixture that states its own refs keeps them,
    /// which is how a tree whose refs and host ids diverge is written.
    fn with_refs(v: &Value) -> Value {
        fn walk(v: &mut Value, next: &mut u32) {
            let r = *next;
            *next += 1;
            if let Some(o) = v.as_object_mut() {
                o.entry("ref").or_insert(json!(r));
            }
            if let Some(kids) = v.get_mut("children").and_then(Value::as_array_mut) {
                for c in kids {
                    walk(c, next);
                }
            }
        }
        let mut out = v.clone();
        walk(&mut out, &mut 0);
        out
    }

    /// [`tree_from_json`] over a fixture numbered like a device reply.
    fn read_json(v: &Value, limits: WalkLimits) -> Result<RefTree> {
        tree_from_json(&with_refs(v), &win(), limits)
    }

    /// [`json_to_node`] over a single fixture node numbered like a device reply.
    fn mapped_node(v: &Value) -> AxNode {
        json_to_node(
            &with_refs(v),
            &win(),
            0,
            &mut Walk::new(WalkLimits::DEFAULT),
        )
        .expect("maps")
    }

    /// The restore hands the device back — service disabled, forward removed — and runs after the
    /// app was stopped, so a wedge earlier in teardown is what would starve it (glass#422).
    #[test]
    #[cfg(unix)]
    fn a_restore_that_never_answers_gives_up_at_the_shared_deadline() {
        use crate::adb::{Answer, FakeAdb};
        use std::time::{Duration, Instant};

        let fake = FakeAdb::new(&[("*", Answer::Lingers)]);
        let reg = A11yServiceRegistry::new();
        *reg.state.lock().unwrap() = Some(Active {
            adb: fake.adb().clone(),
            port: 1234,
            prior_enabled: String::new(),
            prior_a11y_enabled: "0".to_string(),
        });

        let started = Instant::now();
        reg.shutdown(Deadline::at(Instant::now() + Duration::from_millis(300)));

        assert!(
            started.elapsed() < Duration::from_secs(2),
            "waited {:?} on a device that never answers — three unbounded calls at 10s each is \
             what the deadline exists to prevent",
            started.elapsed()
        );
        assert!(
            fake.called("settings"),
            "the first step still has to be attempted: {:?}",
            fake.calls()
        );
    }

    #[test]
    fn signature_mismatch_detected() {
        let failed = |said: &str| {
            crate::adb::a_failed_call(&["install", "-r", "/opt/glass/glass-a11y.apk"], said)
        };
        assert!(is_signature_mismatch(&failed(
            "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package signatures do not match newer version; ignoring!]"
        )));
        assert!(is_signature_mismatch(&failed(
            "signatures do not match newer version"
        )));
        assert!(!is_signature_mismatch(&failed(
            "Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"
        )));
        assert!(!is_signature_mismatch(&failed("error: device offline")));
    }

    #[test]
    #[cfg(unix)]
    fn an_install_recovers_from_a_differently_signed_build_and_reports_anything_else() {
        use crate::adb::{Answer, FakeAdb};

        // A clean install: one call, nothing removed.
        let clean = FakeAdb::new(&[("*", Answer::Silent)]);
        install_service(clean.adb(), "/opt/glass/glass-a11y.apk").expect("a clean install");
        assert_eq!(clean.calls(), ["install -r /opt/glass/glass-a11y.apk"]);

        // A differently-signed build already present: glass owns this package, so the stale copy
        // goes and a fresh one is installed rather than the launch failing.
        let stale_ok = Answer::fails(
            "Failure [INSTALL_FAILED_UPDATE_INCOMPATIBLE: Existing package signatures do not \
             match newer version; ignoring!]",
        );
        let fresh = Answer::Silent;
        let mismatched = FakeAdb::scripted(&[
            ("install *", vec![&stale_ok, &fresh]),
            ("*", vec![&Answer::Silent]),
        ]);
        install_service(mismatched.adb(), "/opt/glass/glass-a11y.apk")
            .expect("a signature mismatch is recovered from");
        assert!(
            mismatched.called(&format!("uninstall {SERVICE_PACKAGE}")),
            "{:?}",
            mismatched.calls()
        );

        // Any other failure is reported, and nothing is uninstalled on the strength of it.
        let other = FakeAdb::new(&[(
            "install *",
            Answer::fails("Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]"),
        )]);
        let err = install_service(other.adb(), "/opt/glass/glass-a11y.apk")
            .expect_err("a full device is not a signature mismatch");
        assert!(err.to_string().contains("INSUFFICIENT_STORAGE"), "{err}");
        assert!(!other.called("uninstall"), "{:?}", other.calls());
    }

    #[test]
    fn the_service_apk_is_the_configured_one_unless_accessibility_is_turned_off() {
        let configured = |k: &str| match k {
            "GLASS_ANDROID_A11Y_APK" => Some("/opt/glass/glass-a11y.apk".to_string()),
            _ => None,
        };
        assert_eq!(
            a11y_apk(&configured).as_deref(),
            Some("/opt/glass/glass-a11y.apk")
        );

        let turned_off = |k: &str| match k {
            "GLASS_ANDROID_A11Y" => Some("off".to_string()),
            "GLASS_ANDROID_A11Y_APK" => Some("/opt/glass/glass-a11y.apk".to_string()),
            _ => None,
        };
        assert_eq!(
            a11y_apk(&turned_off),
            None,
            "off must win over a configured path, or the reader installs what it was told not to"
        );
    }

    #[test]
    fn a_node_is_placed_relative_to_the_window_and_reported_visible() {
        // The device sends screen-absolute bounds, so an offset applied the wrong way puts
        // every node twice the window's inset down the screen, and a click lands there.
        let node = mapped(Some("Save"), None);
        let bounds = node.bounds.expect("the device always sends bounds");
        assert_eq!(
            (bounds.x, bounds.y),
            (0, 0),
            "screen y=100 in a window at y=100"
        );
        // The companion only ever describes what is on screen, and a node reported invisible is
        // one `wait_for_element` will never settle on.
        assert!(node.states.visible);
    }

    #[test]
    fn a_child_list_stops_at_the_node_budget_and_says_so() {
        let wide = json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 100, "h": 100},
            "enabled": true,
            "children": (0..8).map(|i| json!({
                "class": "android.widget.TextView",
                "text": format!("row {i}"),
                "bounds": {"x": 0, "y": 100, "w": 10, "h": 10},
                "enabled": true,
            })).collect::<Vec<_>>(),
        });
        let mut walk = Walk::new(WalkLimits {
            nodes: 4,
            ..WalkLimits::DEFAULT
        });
        let root = json_to_node(&with_refs(&wide), &win(), 0, &mut walk).expect("maps");

        assert!(
            (1..8).contains(&root.children.len()),
            "the cap must stop the walk part-way, not before it starts: {}",
            root.children.len()
        );
        assert_eq!(
            walk.budget.truncation().map(|t| t.limit),
            Some(TruncationLimit::Nodes),
            "a walk that declined children must say which cap did it"
        );
    }

    #[test]
    fn a_childless_node_at_the_depth_cap_records_no_truncation() {
        // The device sends `"children": []` for a leaf. Reaching one with the depth budget
        // already spent declines nothing, so reporting the tree truncated is a false positive —
        // and the caller's remedy for it, raising `max_depth`, returns the identical tree.
        let leaf_at_the_cap = json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 100, "h": 100},
            "enabled": true,
            "children": [{
                "class": "android.widget.TextView",
                "text": "leaf",
                "bounds": {"x": 0, "y": 100, "w": 10, "h": 10},
                "enabled": true,
                "children": [],
            }],
        });
        let mut walk = Walk::new(WalkLimits {
            depth: 1,
            ..WalkLimits::DEFAULT
        });
        let root = json_to_node(&with_refs(&leaf_at_the_cap), &win(), 0, &mut walk).expect("maps");

        assert_eq!(root.children.len(), 1, "the leaf is still mapped");
        assert!(
            walk.budget.truncation().is_none(),
            "nothing was declined: {:?}",
            walk.budget.truncation()
        );
    }

    #[test]
    fn a_node_whose_children_are_past_the_depth_cap_does_report_truncation() {
        // Paired with the case above, which would otherwise pass if the cap never fired.
        let child_at_the_cap = json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 100, "h": 100},
            "enabled": true,
            "children": [{
                "class": "android.widget.FrameLayout",
                "bounds": {"x": 0, "y": 100, "w": 50, "h": 50},
                "enabled": true,
                "children": [{
                    "class": "android.widget.TextView",
                    "text": "too deep",
                    "bounds": {"x": 0, "y": 100, "w": 10, "h": 10},
                    "enabled": true,
                }],
            }],
        });
        let mut walk = Walk::new(WalkLimits {
            depth: 1,
            ..WalkLimits::DEFAULT
        });
        let _ = json_to_node(&with_refs(&child_at_the_cap), &win(), 0, &mut walk).expect("maps");

        assert_eq!(
            walk.budget.truncation().map(|t| t.limit),
            Some(TruncationLimit::Depth)
        );
    }

    #[test]
    fn enclosure_is_measured_from_each_rectangle_far_edge() {
        let rect = |x, y, width, height| {
            Some(AxRect {
                x,
                y,
                width,
                height,
            })
        };
        let outer = rect(0, 0, 100, 100);

        assert!(
            encloses(outer, rect(10, 10, 50, 50)),
            "a box wholly inside is enclosed"
        );
        assert!(
            !encloses(outer, rect(60, 10, 60, 50)),
            "one whose right edge is past the outer's is not"
        );
        assert!(
            !encloses(outer, rect(10, 60, 50, 60)),
            "nor one whose bottom edge is"
        );
        assert!(
            encloses(outer, rect(0, 0, 100, 100)),
            "edges are inclusive, so an exact fit encloses"
        );
        assert!(!encloses(outer, None), "geometry that cannot be checked");
        assert!(!encloses(None, rect(10, 10, 10, 10)));
    }

    /// The far-edge arithmetic promotes to `i64` (glass#322): an outer at (0, 0) with a u32::MAX
    /// width makes `x + width` = 2^32 - 1, which overflows an `i32`, so a node that genuinely
    /// encloses its descendant would read as not enclosing it. Pin the result so a simplification
    /// back to i32 arithmetic fails here instead of silently mis-actuating a control.
    #[test]
    fn encloses_does_not_overflow_on_max_edges() {
        let rect = |x, y, width, height| {
            Some(AxRect {
                x,
                y,
                width,
                height,
            })
        };
        let outer = rect(0, 0, u32::MAX, u32::MAX);
        let inner = rect(0, 0, 100, 100);
        // outer.x + outer.width = 2^32 - 1 (overflowing as i32) >= the inner's right edge 100,
        // and every start/far edge is within i32 range, so this genuinely encloses.
        assert!(encloses(outer, inner));
    }

    /// `GLASS_ANDROID_A11Y_APK` is caller-supplied and lands in the argv the error names, so a
    /// path quoting the phrase used to make every install failure take the uninstall branch.
    #[test]
    fn a_phrase_in_the_apk_path_does_not_make_an_unrelated_failure_a_signature_mismatch() {
        let poisoned = "/home/u/signatures do not match/glass-a11y.apk";
        for said in [
            "Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]",
            "error: device offline",
        ] {
            let e = crate::adb::a_failed_call(&["install", "-r", poisoned], said);
            assert!(!is_signature_mismatch(&e), "{e}");
        }
    }

    /// A timed-out install says nothing about signatures: it names no exit status, and the message
    /// it does carry quotes whatever the child printed first.
    #[test]
    fn an_install_that_timed_out_is_not_a_signature_mismatch() {
        let e = crate::adb::a_real_timeout_hinted();
        assert!(!is_signature_mismatch(&e), "{e}");
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
        let t = read_json(&v, WalkLimits::DEFAULT).unwrap().tree;
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
        let tree = tree_from_json(&with_refs(&device_tree()), &win, WalkLimits::DEFAULT)
            .expect("builds")
            .tree;
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
    fn the_host_view_of_a_published_page_is_a_group_here_too() {
        // Both readers have to agree about a web page's shape, or one `role:` selector cannot
        // address it on both (glass#521).
        let page = json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400},
            "children": [{
                "class": "android.webkit.WebView", "desc": "the web view",
                "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2148},
                "children": [{
                    "class": "android.webkit.WebView",
                    "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2148},
                    "children": [{
                        "class": "android.view.View", "text": "Role fixture",
                        "bounds": {"x": 0, "y": 0, "w": 1080, "h": 120}, "children": []
                    }]
                }]
            }]
        });
        let tree = read_json(&page, WalkLimits::DEFAULT).expect("builds").tree;
        let host = &tree.root.children[0];
        assert_eq!(host.role, AxRole::Group, "the host view");
        assert_eq!(host.children[0].role, AxRole::Document, "the page root");
    }

    #[test]
    fn reads_checkable_and_checked_from_json() {
        // The companion now carries isCheckable/isChecked; surface them on the node's states.
        let on = json!({
            "class": "android.widget.CheckBox", "bounds": {"x": 0, "y": 100, "w": 10, "h": 10},
            "checkable": true, "checked": true
        });
        let n = mapped_node(&on);
        assert!(
            n.states.checkable && n.states.checked,
            "on checkbox → checkable + checked"
        );
        let plain = json!({
            "class": "android.widget.TextView", "bounds": {"x": 0, "y": 100, "w": 10, "h": 10}
        });
        let p = mapped_node(&plain);
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
    fn the_node_cap_stops_the_walk_rather_than_dropping_nodes_from_the_middle() {
        // Unlike Depth and Siblings, this cap ends the walk outright, so what survives is a
        // genuine prefix of the device's tree.
        let json = wide_device_json(glass_core::MAX_NODES + 50);
        let tree = read_json(&json, WalkLimits::DEFAULT)
            .expect("tree parses")
            .tree;

        assert!(tree.truncated.is_some(), "the node cap must have been hit");
        // The device root maps directly (no synthetic wrapper), so the FrameLayout itself is
        // id 0 and the child at array index i is id i+1.
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
        let tree = read_json(&json, WalkLimits::DEFAULT)
            .expect("tree parses")
            .tree;
        assert_eq!(tree.count, glass_core::MAX_NODES);
        assert_eq!(tree.truncated, None);
    }

    #[test]
    fn a_tree_of_max_nodes_plus_one_still_reports_nodes_truncation() {
        // One more child than the complete case above: now there really is a node the walk
        // declines to visit, so the cap must still fire — proving the fix didn't just
        // disable it.
        let json = wide_device_json(glass_core::MAX_NODES);
        let tree = read_json(&json, WalkLimits::DEFAULT)
            .expect("tree parses")
            .tree;
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
        let mut walk = Walk::new(WalkLimits::DEFAULT);
        for _ in 0..glass_core::MAX_NODES {
            walk.budget.visit();
        }
        let _ = json_to_node(&with_refs(&leaf), &win(), 0, &mut walk).unwrap();
        assert!(walk.budget.truncation().is_none());
    }

    #[test]
    fn degenerate_bounds_clamp_instead_of_erroring() {
        // A live a11y tree legitimately has zero/inverted rects; the mapper must clamp, not fail.
        let v = json!({
            "ref": 0, "class": "android.view.View",
            "bounds": {"x": -5, "y": 10, "w": -3, "h": 0},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false
        });
        let t = read_json(&v, WalkLimits::DEFAULT)
            .expect("degenerate bounds must not error the snapshot")
            .tree;
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
        mapped_node(&node_json(text, desc, None, None))
    }

    /// [`mapped`], for an editable field; omits `resource_id`/`hint`, the shape an older
    /// companion sends.
    fn mapped_editable(text: Option<&str>, desc: Option<&str>) -> AxNode {
        let mut v = node_json(text, desc, None, None);
        v["class"] = json!("android.widget.EditText");
        v["editable"] = json!(true);
        mapped_node(&v)
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
        mapped_node(&v)
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
        // what stop the call site transposing the two.)
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

    /// [`compose_like`] with its content slid `dx` px along x and nothing resized — the
    /// activity-open transition glass#326 measured, where the window stays put and its content
    /// moves.
    fn compose_like_slid(dx: i64) -> Value {
        let mut v = compose_like();
        v["children"][0]["bounds"] = json!({"x": 60 + dx, "y": 480, "w": 210, "h": 130});
        v["children"][0]["children"][0]["bounds"] =
            json!({"x": 120 + dx, "y": 520, "w": 80, "h": 50});
        v
    }

    /// Two clickable cards holding a "Save" label each, identical in role, name, value and size.
    fn two_identical_saves() -> Value {
        let card = |x: i64| {
            json!({
                "class": "android.view.View",
                "bounds": {"x": x, "y": 480, "w": 210, "h": 130},
                "editable": false, "clickable": true, "enabled": true, "scrollable": false,
                "children": [
                    {"class": "android.widget.TextView", "text": "Save",
                     "bounds": {"x": x + 60, "y": 520, "w": 80, "h": 50},
                     "editable": false, "clickable": false, "enabled": true, "scrollable": false}
                ]
            })
        };
        json!({
            "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "editable": false, "clickable": false, "enabled": true, "scrollable": false,
            "children": [card(60), card(600)]
        })
    }

    fn built(v: &Value) -> AxTree {
        read_json(v, WalkLimits::DEFAULT).expect("maps").tree
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
        // No node in this tree is called "Send", so the refusal is `fingerprinted`'s gone
        // verdict rather than its changed one.
        let t = built(&compose_like());
        let mut label = target_for(&t, AxNodeId(2));
        label.name = Some("Send".into());
        assert!(matches!(
            invoke_plan(&t, &label),
            Err(GlassError::AxElementGone(2))
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
        read_json(&compose_like(), limits).expect("maps").tree
    }

    #[test]
    fn a_truncated_tree_still_actuates_natively() {
        // Truncation of any kind leaves the surviving nodes addressable — each carries the
        // device's own `ref` — so a plan over one still dispatches natively rather than
        // falling back to a pointer click.
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

    /// A device tree with a trailing sibling *after* a subtree the host will prune, so the host's
    /// pre-order id and the device's own `ref` come apart under a Depth or a Siblings cap
    /// (glass#288): `root > [Card > (c1, c2, c3), trailing]`.
    ///
    /// - `depth: 1` drops Card's children — kept ids `0,1,2`, so the trailing node is id 2.
    /// - `siblings: 2` drops `c3` — kept ids `0..=4`, so the trailing node is id 4.
    ///
    /// Either way the device numbered that node `ref` 5, and its own `ref` 2 and 4 are `c1` and
    /// `c3`. An action addressed by the host id lands on one of those instead.
    fn trailing_after_a_pruned_subtree(trailing: Value) -> Value {
        let leaf = |r: u32, y: i64| {
            json!({
                "ref": r, "class": "android.widget.TextView", "text": format!("c{r}"),
                "bounds": {"x": 60, "y": y, "w": 80, "h": 50},
                "clickable": false, "enabled": true
            })
        };
        json!({
            "ref": 0, "class": "android.widget.FrameLayout",
            "bounds": {"x": 0, "y": 100, "w": 1080, "h": 2300},
            "clickable": false, "enabled": true,
            "children": [
                {"ref": 1, "class": "android.view.View",
                 "bounds": {"x": 60, "y": 480, "w": 210, "h": 130},
                 "clickable": false, "enabled": true,
                 "children": [leaf(2, 500), leaf(3, 540), leaf(4, 580)]},
                trailing
            ]
        })
    }

    /// [`trailing_after_a_pruned_subtree`] trailing a clickable "Save" button (device `ref` 5).
    fn save_after_a_pruned_subtree() -> Value {
        trailing_after_a_pruned_subtree(json!({
            "ref": 5, "class": "android.widget.Button", "desc": "Save",
            "bounds": {"x": 60, "y": 700, "w": 210, "h": 130},
            "clickable": true, "enabled": true
        }))
    }

    /// [`trailing_after_a_pruned_subtree`] trailing an editable "Email" field (device `ref` 5)
    /// holding `value`.
    fn field_after_a_pruned_subtree(value: &str) -> Value {
        trailing_after_a_pruned_subtree(json!({
            "ref": 5, "class": "android.widget.EditText", "text": value, "desc": "Email",
            "bounds": {"x": 60, "y": 700, "w": 600, "h": 120},
            "editable": true, "clickable": true, "enabled": true
        }))
    }

    /// The target a caller would hold after picking the node named `name` out of `tree`.
    fn target_named(tree: &AxTree, name: &str) -> AxTarget {
        fn walk<'a>(node: &'a AxNode, name: &str) -> Option<&'a AxNode> {
            if node.name.as_deref() == Some(name) {
                return Some(node);
            }
            node.children.iter().find_map(|c| walk(c, name))
        }
        let n = walk(&tree.root, name).expect("the tree holds a node with that name");
        AxTarget {
            id: n.id,
            role: n.role,
            name: n.name.clone(),
            bounds: n.bounds,
            value: n.value.clone(),
        }
    }

    /// Read a tree under `limits` from a fake device serving `trees`, and return the reader, the
    /// context, the tree as a caller receives it, and the request log.
    fn read_under(
        trees: Vec<Value>,
        limits: WalkLimits,
    ) -> (ServiceA11y, AxContext, AxTree, Arc<Mutex<Vec<String>>>) {
        let (port, ops) = fake_service(trees, OnAction::Ok);
        let mut a11y = reader(port, std::time::Duration::ZERO);
        let mut ctx = ctx();
        ctx.limits = limits;
        let mut tree = a11y.snapshot(&ctx).expect("snapshot");
        // What the session does to every snapshot before a caller sees an id.
        tree.assign_ids();
        (a11y, ctx, tree, ops)
    }

    #[test]
    fn a_depth_truncated_tree_clicks_the_ref_the_device_gave_the_node() {
        let (mut a11y, ctx, tree, ops) = read_under(
            vec![save_after_a_pruned_subtree()],
            WalkLimits {
                depth: 1,
                ..WalkLimits::DEFAULT
            },
        );
        assert_eq!(
            tree.truncated.map(|t| t.limit),
            Some(TruncationLimit::Depth)
        );
        let save = target_named(&tree, "Save");
        assert_eq!(
            save.id,
            AxNodeId(2),
            "the host numbered Save 2, the device 5"
        );

        a11y.invoke(&ctx, &save).expect("the click is dispatched");

        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:tree".to_string(),
                "conn1:click ref=5".to_string()
            ],
            "sending the host id would have clicked c3, the device's own ref 2; the second tree \
             is invoke's own guard read"
        );
    }

    #[test]
    fn a_sibling_truncated_tree_writes_to_the_ref_the_device_gave_the_node() {
        let (mut a11y, ctx, tree, ops) = read_under(
            vec![
                field_after_a_pruned_subtree("old"),
                field_after_a_pruned_subtree("old"),
                field_after_a_pruned_subtree("new"),
            ],
            WalkLimits {
                siblings: 2,
                ..WalkLimits::DEFAULT
            },
        );
        assert_eq!(
            tree.truncated.map(|t| t.limit),
            Some(TruncationLimit::Siblings)
        );
        let email = target_named(&tree, "Email");
        assert_eq!(
            email.id,
            AxNodeId(4),
            "the host numbered Email 4, the device 5"
        );

        a11y.set_value(&ctx, &email, "new")
            .expect("the write lands");

        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:tree".to_string(),
                "conn1:set_text ref=5".to_string(),
                "conn1:tree".to_string()
            ],
            "sending the host id would have written to c3, the device's own ref 4; the trees are \
             the caller's read, set_value's guard read and its verification read"
        );
    }

    #[test]
    fn a_node_without_a_ref_is_refused_rather_than_numbered_by_inference() {
        // The companion has numbered every node since its first release. Inferring the missing
        // one from walk order is the glass#288 bug in miniature, and dropping the node would
        // shift every later one — so this is a protocol violation, like missing bounds.
        let mut v = save_after_a_pruned_subtree();
        v["children"][1]
            .as_object_mut()
            .expect("node")
            .remove("ref");
        let e = tree_from_json(&v, &win(), WalkLimits::DEFAULT)
            .expect_err("a node with no ref cannot be addressed");
        assert!(matches!(e, GlassError::AccessibilityUnavailable(_)), "{e}");
        assert!(e.to_string().contains("ref"), "{e}");
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
        fake_service_stumbling(trees, on_action, packages, on_tree, 0)
    }

    /// [`fake_service_full`], dropping the first `stumbles` connections before the hello — a
    /// service whose socket is up but which is not answering yet.
    fn fake_service_stumbling(
        trees: Vec<Value>,
        on_action: Vec<OnAction>,
        packages: Vec<TreePackage>,
        on_tree: Vec<OnTree>,
        stumbles: usize,
    ) -> (u16, Arc<Mutex<Vec<String>>>) {
        // Numbered on the way out, like the real companion's `treeJson`, so a fixture never has
        // to restate what every device reply carries.
        let trees: Vec<Value> = trees.iter().map(with_refs).collect();
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
                if conn <= stumbles {
                    log.lock().unwrap().push(format!("conn{conn}:dropped"));
                    continue;
                }
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
            deadline: Deadline::UNBOUNDED,
        }
    }

    fn ops_of(ops: &Arc<Mutex<Vec<String>>>) -> Vec<String> {
        ops.lock().unwrap().clone()
    }

    /// glass#338: this reader is the one the platform *prefers* once the companion is installed,
    /// so a deadline it ignores is a deadline the dogfood configuration ignores.
    ///
    /// A caller that has stopped waiting is answered without a round-trip at all — the socket's
    /// standing timeout is 30s and a failed call reconnects and re-sends, so one snapshot can
    /// outlast a whole `glass_wait_for_element` twice over.
    #[test]
    fn a_snapshot_the_caller_stopped_waiting_for_never_reaches_the_device() {
        let (port, ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let mut a11y = reader(port, std::time::Duration::ZERO);
        let mut ctx = ctx();
        ctx.deadline = Deadline::from_millis(0);

        let e = a11y
            .snapshot(&ctx)
            .expect_err("the caller had stopped waiting");

        assert!(matches!(e, GlassError::AccessibilityNotReady(_)), "{e}");
        assert!(
            ops_of(&ops).is_empty(),
            "the device was asked for a tree nobody was waiting for: {:?}",
            ops_of(&ops)
        );
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
        assert!(
            client.set_text(1, "hi", "com.example.app").is_ok(),
            "re-arm confirms the same package, so the resend goes through"
        );
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
        let Err(f) = client.set_text(1, "hi", "com.example.app") else {
            panic!("a different foreground must refuse the resend");
        };
        let msg = f.into_error().to_string();
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
        let Err(f) = client.set_text(1, "hi", "com.example.app") else {
            panic!("an unconfirmed foreground must refuse the resend");
        };
        let msg = f.into_error().to_string();
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

    /// A device that answers everything `ensure` asks, with `enabled_accessibility_services`
    /// already reading `prior` and the forward landing on `port`.
    #[cfg(unix)]
    fn a_device_ready_to_enable(
        prior: &str,
        prior_enabled: &str,
        port: u16,
    ) -> crate::adb::FakeAdb {
        use crate::adb::{Answer, FakeAdb};
        FakeAdb::new(&[
            (
                "*settings get secure enabled_accessibility_services",
                Answer::says(format!("{prior}\n")),
            ),
            (
                "*settings get secure accessibility_enabled",
                Answer::says(format!("{prior_enabled}\n")),
            ),
            ("*forward tcp:0 *", Answer::says(format!("{port}\n"))),
            ("*", Answer::Silent),
        ])
    }

    /// The value the last `settings put secure enabled_accessibility_services` wrote.
    #[cfg(unix)]
    fn enabled_list_written(fake: &crate::adb::FakeAdb) -> String {
        fake.calls()
            .iter()
            .filter_map(|c| {
                c.split_once("settings put secure enabled_accessibility_services ")
                    .map(|(_, v)| v.to_string())
            })
            .next_back()
            .unwrap_or_default()
    }

    #[test]
    #[cfg(unix)]
    fn enabling_the_service_adds_it_to_whatever_the_device_already_had() {
        // Dropping the device's other services would turn off a screen reader for the session,
        // and leave it off if teardown is missed.
        let (port, _ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let fake = a_device_ready_to_enable("com.other/.Reader", "1", port);

        let registry = A11yServiceRegistry::new();
        registry
            .ensure(fake.adb(), "/opt/glass/glass-a11y.apk")
            .expect("the service is installed, enabled and serving");

        assert_eq!(
            enabled_list_written(&fake),
            format!("com.other/.Reader:{SERVICE_COMPONENT}"),
            "{:?}",
            fake.calls()
        );
        assert!(
            fake.called("settings put secure accessibility_enabled 1"),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn enabling_a_service_the_device_already_lists_does_not_list_it_twice() {
        let (port, _ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let already = format!("com.other/.Reader:{SERVICE_COMPONENT}");
        let fake = a_device_ready_to_enable(&already, "1", port);

        let registry = A11yServiceRegistry::new();
        registry
            .ensure(fake.adb(), "/opt/glass/glass-a11y.apk")
            .expect("the service is already listed");

        assert_eq!(enabled_list_written(&fake), already, "{:?}", fake.calls());
    }

    #[test]
    #[cfg(unix)]
    fn a_device_with_no_accessibility_at_all_is_restored_to_exactly_that() {
        // `settings get` prints the string "null" for an unset key. Carrying that through as a
        // value would enable a service literally named `null` and restore one on teardown.
        let (port, _ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let fake = a_device_ready_to_enable("null", "null", port);

        let registry = A11yServiceRegistry::new();
        registry
            .ensure(fake.adb(), "/opt/glass/glass-a11y.apk")
            .expect("the service comes up on a device with none enabled");
        assert_eq!(
            enabled_list_written(&fake),
            SERVICE_COMPONENT,
            "{:?}",
            fake.calls()
        );

        registry.shutdown(Deadline::UNBOUNDED);

        // An empty list is a `delete`, and the global flag goes back to off rather than to "null".
        assert!(
            fake.called("settings delete secure enabled_accessibility_services"),
            "{:?}",
            fake.calls()
        );
        assert!(
            fake.called("settings put secure accessibility_enabled 0"),
            "{:?}",
            fake.calls()
        );
        assert!(
            fake.called(&format!("forward --remove tcp:{port}")),
            "{:?}",
            fake.calls()
        );
    }

    #[test]
    #[cfg(unix)]
    fn shutting_down_restores_what_the_device_had_and_only_once() {
        let (port, _ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let fake = a_device_ready_to_enable("com.other/.Reader", "1", port);

        let registry = A11yServiceRegistry::new();
        registry
            .ensure(fake.adb(), "/opt/glass/glass-a11y.apk")
            .expect("the service comes up");
        registry.shutdown(Deadline::UNBOUNDED);

        assert_eq!(
            enabled_list_written(&fake),
            "com.other/.Reader",
            "the device's own service is put back, without glass's"
        );

        let after = fake.calls().len();
        registry.shutdown(Deadline::UNBOUNDED);
        assert_eq!(
            fake.calls().len(),
            after,
            "a second shutdown must not touch a device glass no longer owns"
        );
    }

    #[test]
    #[cfg(unix)]
    fn a_rebind_reinstalls_only_as_a_last_resort() {
        // The reinstall is what SIGKILLs the service and starts the binding race, so trying it
        // first restarts the thing being waited on.
        use crate::adb::{Answer, FakeAdb};
        let want = format!("com.other/.Reader:{SERVICE_COMPONENT}");

        let early = FakeAdb::new(&[("*", Answer::Silent)]);
        escalate(early.adb(), "/opt/glass/glass-a11y.apk", &want, 0, 3)
            .expect("a rebind on its own");
        assert!(!early.called("install"), "{:?}", early.calls());
        // It disables and re-enables, which is what makes the platform rebuild the binding.
        assert!(
            early.called("settings put secure accessibility_enabled 0"),
            "{:?}",
            early.calls()
        );
        assert!(
            early.called("settings put secure accessibility_enabled 1"),
            "{:?}",
            early.calls()
        );

        let last = FakeAdb::new(&[("*", Answer::Silent)]);
        escalate(last.adb(), "/opt/glass/glass-a11y.apk", &want, 2, 3)
            .expect("the last attempt reinstalls first");
        assert!(last.called("install -r"), "{:?}", last.calls());
    }

    #[test]
    fn readiness_keeps_trying_a_socket_that_is_not_answering_yet() {
        // Between the reinstall's SIGKILL and the rebind the service accepts and then drops the
        // connection. Taking that first refusal as the answer would fail a launch that is a
        // fraction of a second from working.
        let (port, ops) = fake_service_stumbling(
            vec![compose_like()],
            vec![OnAction::Ok],
            vec![TreePackage::Echo],
            vec![OnTree::Serve],
            2,
        );
        wait_for_ready(port, READY_ATTEMPT).expect("the third connection is greeted");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:dropped".to_string(),
                "conn2:dropped".to_string(),
                "conn3:tree".to_string(),
            ],
        );
    }

    #[test]
    fn readiness_gives_up_on_a_socket_that_never_greets() {
        // A service that never comes back has to end the attempt, or `ensure` waits on it for
        // as long as the process lives.
        let (port, _ops) = fake_service_stumbling(
            vec![compose_like()],
            vec![OnAction::Ok],
            vec![TreePackage::Echo],
            vec![OnTree::Serve],
            usize::MAX,
        );
        let started = std::time::Instant::now();
        let Err(err) = wait_for_ready(port, std::time::Duration::from_millis(300)) else {
            panic!("a socket that never answers cannot become ready");
        };
        assert!(
            started.elapsed() < std::time::Duration::from_secs(5),
            "waited {:?} — the deadline never ended the wait",
            started.elapsed()
        );
        assert!(err.contains("never accepted a connection"), "{err}");
    }

    #[test]
    fn a_bounded_read_does_not_leave_its_bound_on_the_connection() {
        // The socket outlives any one request, so a bound left behind applies to a later call
        // that never agreed to it — including a snapshot with no deadline at all.
        let (port, _ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let mut bounded = ctx();
        bounded.deadline = Deadline::from_millis(500);
        a.snapshot(&bounded).expect("a bounded snapshot");

        let left = a
            .client
            .conn
            .lock()
            .expect("conn lock")
            .reader
            .get_ref()
            .read_timeout()
            .expect("the socket has a read timeout");
        assert_eq!(
            left,
            Some(crate::conn::STANDING_TIMEOUT),
            "the next call would inherit this one's bound"
        );
    }

    #[test]
    fn a_toggle_that_flips_a_beat_later_is_waited_for() {
        // The accessibility tree catches up after the toolkit's own UI, so the first read back
        // legitimately still shows the old state.
        let (port, ops) = fake_service(
            vec![
                one_checkable("android.widget.CheckBox", false),
                one_checkable("android.widget.CheckBox", false),
                one_checkable("android.widget.CheckBox", true),
            ],
            OnAction::Ok,
        );
        let mut a = reader(port, std::time::Duration::from_secs(2));
        let t = built(&one_checkable("android.widget.CheckBox", false));
        a.invoke(&ctx(), &target_for(&t, AxNodeId(1)))
            .expect("the state moves on the second read-back");
        assert_eq!(
            ops_of(&ops).iter().filter(|o| o.ends_with(":tree")).count(),
            3,
            "the plan's read plus two read-backs: {:?}",
            ops_of(&ops)
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
        let rt = a.snapshot_with_refs(&ctx()).expect("snapshot");
        let plan = invoke_plan(&rt.tree, &target).expect("plan resolves");

        // Break the connection's write half — deterministic, not raced: this line has already
        // returned before `dispatch_click` is called below.
        a.client
            .conn
            .lock()
            .expect("lock")
            .writer
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");

        a.dispatch_click(&ctx(), &target, &plan, &rt)
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

        let rt = a.snapshot_with_refs(&ctx()).expect("snapshot");
        let plan = invoke_plan(&rt.tree, &target).expect("plan resolves");

        a.client
            .conn
            .lock()
            .expect("lock")
            .writer
            .shutdown(std::net::Shutdown::Write)
            .expect("shutdown");

        let e = a
            .dispatch_click(&ctx(), &target, &plan, &rt)
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

    /// [`editable_field`] with the field slid `dx` px along x and nothing resized.
    fn editable_field_slid(value: &str, dx: i64) -> Value {
        let mut v = editable_field(value);
        v["children"][0]["bounds"] = json!({"x": dx, "y": 100, "w": 600, "h": 120});
        v
    }

    /// [`editable_field`] with the field no longer editable — a collapsed display row, whose text
    /// `labels` maps to `name`, leaving no value to read.
    fn settled_field(value: &str) -> Value {
        let mut v = editable_field(value);
        v["children"][0]["editable"] = json!(false);
        v
    }

    /// A reader whose write-verify budget is spent, so a timeout costs no wall clock.
    fn impatient_writer(port: u16) -> ServiceA11y {
        let client = ServiceClient::connect(port).expect("connect to the fake service");
        let mut a = ServiceA11y::new(client, "com.example.app".to_string());
        a.write_verify_budget = std::time::Duration::ZERO;
        a
    }

    #[test]
    fn a_write_that_never_lands_names_both_values_and_the_compose_no_op() {
        // The Compose no-op is a fact only this backend knows, and the verdict must also be one
        // the session reads as post-dispatch, or it keeps its cached value (glass#405).
        let (port, _ops) = fake_service(vec![editable_field("old")], OnAction::Ok);
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));

        let err = a
            .set_value(&ctx(), &target_for(&t, AxNodeId(1)), "new")
            .expect_err("the field never takes the value");
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { id: 1, requested, observed, why }
                if requested == "new"
                    && observed.as_deref() == Some("old")
                    && why.is_some_and(|w| w.contains("ACTION_SET_TEXT")
                        && w.contains("GLASS_ANDROID_A11Y=off"))),
            "{err}"
        );
        assert!(err.set_value_failed_after_writing(), "{err}");
    }

    #[test]
    fn a_field_holding_something_neither_value_gets_no_compose_explanation() {
        // The clause tells the caller to clear a field that already holds text; a field holding
        // something the write put there was not refused.
        let (port, _ops) = fake_service(
            vec![editable_field("old"), editable_field("filtered")],
            OnAction::Ok,
        );
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));
        let target = target_for(&t, AxNodeId(1));

        let err = a
            .set_value(&ctx(), &target, "new")
            .expect_err("the field holds neither the request nor what it held");
        assert!(
            matches!(&err, GlassError::AxValueNotApplied { observed, why: None, .. }
                if observed.as_deref() == Some("filtered")),
            "{err}"
        );
    }

    #[test]
    fn a_clear_is_not_confirmed_by_a_field_that_stopped_being_editable() {
        // A collapsed row reports no value, so an unguarded `"" == ""` confirms a clear that may
        // never have fired. Move the editable check back below the value check and this is the
        // only test that notices.
        let (port, _ops) = fake_service(
            vec![editable_field("old"), settled_field("old")],
            OnAction::Ok,
        );
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));

        let err = a
            .set_value(&ctx(), &target_for(&t, AxNodeId(1)), "")
            .expect_err("an element that stopped being a field cannot confirm a clear");
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
    }

    #[test]
    fn an_element_gone_from_the_tree_is_unconfirmed_rather_than_an_empty_reading() {
        // `find` answers `None` for a node that is gone and for one holding nothing. Folding them
        // together reports a reading nobody took — and confirms a clear, since `"" == ""`.
        let (port, _ops) = fake_service(
            vec![
                editable_field("old"),
                json!({"class": "android.widget.FrameLayout",
                       "bounds": {"x": 0, "y": 0, "w": 1080, "h": 2400}, "children": []}),
            ],
            OnAction::Ok,
        );
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));

        let err = a
            .set_value(&ctx(), &target_for(&t, AxNodeId(1)), "")
            .expect_err("a clear cannot be confirmed by an element that left the tree");
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.to_string().contains("no longer in the tree"), "{err}");
    }

    #[test]
    fn a_write_whose_answer_was_lost_is_unconfirmed_rather_than_never_sent() {
        // `AnswerLost` means the request went out and may have run, so the session must drop the
        // value it cached — the same reasoning `action_error` applies to a click (glass#405).
        let (port, _ops) = fake_service_ex(
            vec![editable_field("old")],
            vec![OnAction::DropWithoutAnswering],
            vec![TreePackage::Other("com.other.app".into())],
        );
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));

        let err = a
            .set_value(&ctx(), &target_for(&t, AxNodeId(1)), "new")
            .expect_err("the reconnect cannot re-send into a different foreground app");
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.set_value_failed_after_writing(), "{err}");
    }

    #[test]
    fn a_field_that_stopped_being_editable_is_unconfirmed_and_post_dispatch() {
        // Its value cannot be read back, so the write is unconfirmed rather than refused — and
        // `set_text` went out either way, so the session must drop its cached value.
        let (port, _ops) = fake_service(
            vec![editable_field("old"), settled_field("old")],
            OnAction::Ok,
        );
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));

        let err = a
            .set_value(&ctx(), &target_for(&t, AxNodeId(1)), "new")
            .expect_err("a field that stopped reporting editable cannot confirm the write");
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.set_value_failed_after_writing(), "{err}");
    }

    #[test]
    fn a_read_back_that_fails_is_unconfirmed_and_post_dispatch() {
        // The read-back runs after `set_text`, so its own failure must not propagate raw — the
        // transport verdicts are classified pre-dispatch, which keeps the stale cached value.
        let (port, _ops) = fake_service_full(
            vec![editable_field("old")],
            vec![OnAction::Ok],
            vec![TreePackage::Echo],
            vec![OnTree::Serve, OnTree::Refused("no active window")],
        );
        let mut a = impatient_writer(port);
        let t = built(&editable_field("old"));

        let err = a
            .set_value(&ctx(), &target_for(&t, AxNodeId(1)), "new")
            .expect_err("the verification read is refused");
        assert!(matches!(err, GlassError::AxWriteUnconfirmed(1, _)), "{err}");
        assert!(err.set_value_failed_after_writing(), "{err}");
        // The wrap exists to carry the cause; without it the caller learns only that something
        // could not be confirmed.
        assert!(err.to_string().contains("no active window"), "{err}");
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
                "id": req["id"], "ok": true, "tree": with_refs(&compose_like()),
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

    #[test]
    fn a_target_that_only_moved_is_actuated_where_it_now_is() {
        // glass#326: every observed refusal had role, name and value matching with only x
        // different.
        let (port, ops) = fake_service(vec![compose_like_slid(79)], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&compose_like());
        let substituted = a
            .invoke(&ctx(), &target_for(&seen, AxNodeId(2)))
            .expect("a control that only moved is still that control");
        assert_eq!(substituted, Some(AxNodeId(1)), "the climb is disclosed");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string(), "conn1:click ref=1".to_string()],
            "one read, one dispatch, against the moved node"
        );
    }

    #[test]
    fn two_elements_that_cannot_be_told_apart_are_refused_rather_than_guessed() {
        // Position is what `AxTarget::bounds` exists to discriminate on. Forgiving it is only
        // safe while nothing else in the tree could be mistaken for the target.
        let (port, ops) = fake_service(vec![two_identical_saves()], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&two_identical_saves());
        let mut label = target_for(&seen, AxNodeId(2));
        label.bounds = Some(AxRect {
            x: 199,
            ..label.bounds.expect("the fixture carries bounds")
        });
        let e = a
            .invoke(&ctx(), &label)
            .expect_err("two indistinguishable candidates must not be guessed between");
        let msg = e.to_string();
        assert!(msg.contains("2 elements"), "{msg}");
        assert!(msg.contains("(120,420 80x50)"), "{msg}");
        assert!(msg.contains("(660,420 80x50)"), "{msg}");
        assert!(!e.invoke_fallback_eligible(), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string()],
            "nothing may be dispatched at a guess"
        );
    }

    #[test]
    fn a_target_whose_name_changed_is_refused_however_far_it_moved() {
        let mut renamed = compose_like_slid(79);
        renamed["children"][0]["children"][0]["text"] = json!("Send");
        let (port, ops) = fake_service(vec![renamed], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&compose_like());
        let e = a
            .invoke(&ctx(), &target_for(&seen, AxNodeId(2)))
            .expect_err("only position is forgiven");
        // Renaming the node left nothing in the tree presenting as the target, so the
        // relaxation is never reached.
        assert!(matches!(e, GlassError::AxElementGone(2)), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string()],
            "nothing may be dispatched"
        );
    }

    #[test]
    fn a_node_is_placed_relative_to_a_window_that_is_not_at_the_origin() {
        // Both offsets non-zero: at zero the arithmetic reads the same whichever way round it
        // is combined.
        let inset = WindowGeometry {
            x: 40,
            y: 100,
            width: 600,
            height: 800,
        };
        let node = json_to_node(
            &with_refs(&json!({
                "class": "android.widget.Button",
                "bounds": {"x": 140, "y": 300, "w": 10, "h": 10},
                "enabled": true,
            })),
            &inset,
            0,
            &mut Walk::new(WalkLimits::DEFAULT),
        )
        .expect("maps");
        let bounds = node.bounds.expect("the device always sends bounds");
        assert_eq!((bounds.x, bounds.y), (100, 200));
    }

    #[test]
    fn a_value_that_lands_a_beat_later_is_waited_for_rather_than_called_a_failure() {
        // ACTION_SET_TEXT is async — Compose recomposes, then the accessibility tree catches up
        // — so the first read-back legitimately still shows the old text. Reporting that would
        // send a caller to clear a field that is about to be right.
        let (port, _ops) = fake_service(
            vec![
                editable_field("old"),
                editable_field("old"),
                editable_field("new"),
            ],
            OnAction::Ok,
        );
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&editable_field("old"));
        a.set_value(&ctx(), &target_for(&seen, AxNodeId(1)), "new")
            .expect("the value lands on the second read-back");
    }

    #[test]
    fn a_field_whose_value_changed_is_refused_however_far_it_moved() {
        // The recycled-row hazard `AxTarget::value` exists for: same role, name and size,
        // different data.
        let (port, ops) = fake_service(
            vec![editable_field_slid("someone.else@example.com", 79)],
            OnAction::Ok,
        );
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&editable_field("old@example.com"));
        let e = a
            .set_value(&ctx(), &target_for(&seen, AxNodeId(1)), "new@example.com")
            .expect_err("a field holding other data is not the field that was addressed");
        assert!(matches!(e, GlassError::AxElementChanged(1)), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string()],
            "nothing may be written"
        );
    }

    #[test]
    fn a_target_that_is_no_longer_in_the_tree_is_still_refused_as_absent() {
        // Through `set_value`: the ref that reaches the device is the caller's `target.id`, so a
        // guard that re-found the element by identity anywhere in the tree would authorise a
        // write addressed to a node it never looked at.
        let (port, ops) = fake_service(vec![editable_field_slid("old", 79)], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&editable_field("old"));
        let mut gone = target_for(&seen, AxNodeId(1));
        gone.id = AxNodeId(9);
        let e = a
            .set_value(&ctx(), &gone, "new")
            .expect_err("an id that resolves to nothing is absent, not moved");
        assert!(matches!(e, GlassError::AxElementNotFound(9)), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string()],
            "nothing may be written"
        );
    }

    #[test]
    fn a_target_that_changed_size_is_refused_even_where_it_still_sits() {
        let mut resized = compose_like();
        resized["children"][0]["children"][0]["bounds"] =
            json!({"x": 120, "y": 520, "w": 140, "h": 50});
        let (port, ops) = fake_service(vec![resized], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&compose_like());
        let e = a
            .invoke(&ctx(), &target_for(&seen, AxNodeId(2)))
            .expect_err("a resize is not a translation");
        assert!(matches!(e, GlassError::AxElementChanged(2)), "{e}");
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string()],
            "nothing may be dispatched"
        );
    }

    #[test]
    fn an_invoke_on_a_target_that_did_not_move_costs_no_extra_read_and_no_wait() {
        let (port, ops) = fake_service(vec![compose_like()], OnAction::Ok);
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&compose_like());
        let started = std::time::Instant::now();
        a.invoke(&ctx(), &target_for(&seen, AxNodeId(2)))
            .expect("clicks");
        let elapsed = started.elapsed();
        assert_eq!(
            ops_of(&ops),
            vec!["conn1:tree".to_string(), "conn1:click ref=1".to_string()],
            "forgiving position must not buy itself a second read"
        );
        // The cheapest settle proposed for this path was 250ms, and the fake's two loopback
        // round-trips measure a stable ~80ms, so this bound sits between the two rather than
        // above both — a wait no op-count can see still has to fail here.
        assert!(
            elapsed < std::time::Duration::from_millis(150),
            "no wait may be added to the path every invoke takes: {elapsed:?}"
        );
    }

    #[test]
    fn set_values_guard_accepts_a_field_that_only_moved() {
        let (port, ops) = fake_service(
            vec![
                editable_field_slid("old", 79),
                editable_field_slid("new", 79),
            ],
            OnAction::Ok,
        );
        let mut a = reader(port, std::time::Duration::ZERO);
        let seen = built(&editable_field("old"));
        a.set_value(&ctx(), &target_for(&seen, AxNodeId(1)), "new")
            .expect("a field that only slid is still that field");
        assert_eq!(
            ops_of(&ops),
            vec![
                "conn1:tree".to_string(),
                "conn1:set_text ref=1".to_string(),
                "conn1:tree".to_string()
            ],
            "the guard's read, the write, and the read-back"
        );
    }
}
