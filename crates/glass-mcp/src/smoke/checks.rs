//! The release checks. Each takes the transport seam and returns one outcome, so
//! every one is unit-testable against a scripted transport — no display required.

use crate::smoke::envelope::{check_envelope, untrusted_sibling};
use crate::smoke::profile::{OutlineNode, Profile, parse_outline};
use crate::smoke::report::CheckOutcome;
use crate::smoke::transport::{CallResult, McpTransport};
use rand::RngExt;
use serde_json::{Value, json};

/// Fold a `Result` into an outcome so a transport error is a reported failure,
/// never a panic that loses the rest of the run.
fn outcome(step: u8, name: &str, r: Result<String, String>) -> CheckOutcome {
    match r {
        Ok(detail) => CheckOutcome::pass(step, name, detail),
        Err(detail) => CheckOutcome::fail(step, name, detail),
    }
}

/// A bounded excerpt for a failure detail: enough to triage, never a whole captured log
/// buffer pasted onto one report line.
fn excerpt(s: &str) -> String {
    const MAX: usize = 120;
    if s.chars().count() <= MAX {
        return s.to_string();
    }
    format!("{}…", s.chars().take(MAX).collect::<String>())
}

/// The candidates this run did not select, as a line telling the caller how to choose one.
/// Derived from the table rather than written per backend: prose that names a table drifts from it
/// silently, and this is the only place a run without a probe can say what else exists.
fn other_candidates(p: &Profile) -> Option<String> {
    let others: Vec<&str> = p
        .candidates
        .iter()
        .filter(|c| c.label != p.app.label)
        .map(|c| c.label)
        .collect();
    (!others.is_empty()).then(|| {
        format!(
            "other candidates: {} — select with --app",
            others.join(", ")
        )
    })
}

pub fn check_start(t: &mut dyn McpTransport, p: &Profile) -> CheckOutcome {
    let mut run = vec![Value::String(p.app.bin.to_string())];
    run.extend(p.app.args.iter().map(|a| Value::String((*a).to_string())));
    let mut args = json!({ "run": run, "backend": p.backend, "a11y": true });
    if !p.app.env.is_empty() {
        args["env"] = p.app.env.iter().copied().collect();
    }
    let launched = t
        .call_with_deadline("glass_start", args, p.start_deadline)
        .and_then(|r| {
            let result = check_envelope("glass_start", &r)?;
            for field in ["x", "y", "width", "height"] {
                if !result[field].is_number() {
                    return Err(format!("glass_start returned no `{field}` in its geometry"));
                }
            }
            Ok(format!("{}x{}", result["width"], result["height"]))
        })
        .map_err(|e| match other_candidates(p) {
            Some(offer) => format!("{e}; {offer}"),
            None => e,
        });
    outcome(1, "start", launched)
}

pub fn check_health(t: &mut dyn McpTransport, p: &Profile) -> CheckOutcome {
    outcome(2, "capabilities+doctor", health(t, p))
}

/// Every verdict `glass_doctor`'s `overall` can report (`glass_core::Diagnosis::overall`
/// yields exactly these three). Anything else — absent, null, a status name that isn't a
/// verdict — means there is no verdict to grade, which must fail rather than read as
/// "not fail".
const DOCTOR_VERDICTS: [&str; 3] = ["ok", "warn", "fail"];

/// `check_health`'s body. `glass_doctor`'s `overall` already applies its severity rule — a
/// non-default backend's failing check is only a warning there — so this reads `overall` as
/// the verdict; walking `sections[].checks[]` is only for naming which check(s) failed.
///
/// That severity rule is also why this first pins down *which* backend the server is
/// grading: a verdict about some other backend is not evidence about the one under test.
fn health(t: &mut dyn McpTransport, p: &Profile) -> Result<String, String> {
    let caps = t.call("glass_capabilities", json!({}))?;
    let caps = check_envelope("glass_capabilities", &caps)?;
    // Deliberately no `backend` argument: this asks which backend the server resolved as
    // ACTIVE, which is what `glass_doctor`'s verdict below grades. Naming a backend here
    // would echo the name straight back and prove nothing.
    let active = caps["backend"].as_str().ok_or_else(|| {
        format!(
            "glass_capabilities reported no `backend` string (got {}), so which backend \
             glass_doctor grades below cannot be established",
            caps["backend"]
        )
    })?;
    if active != p.backend {
        return Err(format!(
            "the server's active backend is {active:?}, but this run is testing {want:?} — \
             glass_doctor's verdict grades {active:?}, and a failing check outside the active \
             backend is only a warning there, so a green here would say nothing about {want:?}",
            want = p.backend
        ));
    }

    let doc = t.call("glass_doctor", json!({}))?;
    let doc = check_envelope("glass_doctor", &doc)?;
    let overall = doc["overall"]
        .as_str()
        .filter(|v| DOCTOR_VERDICTS.contains(v))
        .ok_or_else(|| {
            format!(
                "glass_doctor's `overall` is {} — expected one of {}; with no verdict to read \
                 this check cannot grade the environment at all",
                doc["overall"],
                DOCTOR_VERDICTS.join("/")
            )
        })?;

    if overall == "fail" {
        let failed: Vec<&str> = doc["sections"]
            .as_array()
            .into_iter()
            .flatten()
            .flat_map(|s| s["checks"].as_array().into_iter().flatten())
            .filter(|c| c["status"].as_str() == Some("fail"))
            .map(|c| c["name"].as_str().unwrap_or("<unnamed>"))
            .collect();
        return Err(if failed.is_empty() {
            // Silence here would be worse than an odd message: the run truly failed.
            "doctor overall verdict is fail, but no individual check reports fail".to_string()
        } else {
            format!("doctor FAIL: {}", failed.join(", "))
        });
    }

    Ok(format!("backend {active}, doctor {overall}"))
}

pub fn check_screenshot(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        3,
        "screenshot",
        t.call("glass_screenshot", json!({})).and_then(|r| {
            let result = check_envelope("glass_screenshot", &r)?;
            if r.images == 0 {
                return Err("glass_screenshot returned no image block".into());
            }
            if !result["width"].is_number() || !result["height"].is_number() {
                return Err("glass_screenshot envelope has no width/height".into());
            }
            Ok(format!("{}x{} image", result["width"], result["height"]))
        }),
    )
}

/// Returns the outcome and the parsed tree the interaction check needs — `None` when no
/// tree was read at all, which is a different thing from a tree with nothing editable in
/// it and must not be reported as if the app were at fault.
pub fn check_a11y(t: &mut dyn McpTransport) -> (CheckOutcome, Option<Vec<OutlineNode>>) {
    let mut nodes = None;
    let r = (|| {
        let call = t.call("glass_a11y_snapshot", json!({}))?;
        check_envelope("glass_a11y_snapshot", &call)?;
        let body = untrusted_sibling(&call)?;
        let parsed = parse_outline(body);
        if parsed.is_empty() {
            return Err("accessibility tree is empty".into());
        }
        let detail = format!("{} nodes", parsed.len());
        nodes = Some(parsed);
        Ok(detail)
    })();
    (outcome(4, "a11y snapshot", r), nodes)
}

/// The text the interaction check writes: a fresh nonce per run. A fixed string could collide
/// with text the target app itself displays, and `value_contains` would then match something
/// the check never wrote — a verified-looking interaction where nothing happened.
fn probe_text() -> String {
    format!("glass-smoke-{:08x}", rand::rng().random::<u32>())
}

/// Both interaction paths. The element path is verified through `glass_wait_for_element`'s
/// `value_contains` — the signal glass actually exposes for a written value. A written text
/// field's content lands in the a11y `value` property, not `name`, and the outline glass
/// renders carries only `name`, so neither "the tool returned ok" nor a re-read of the
/// outline is evidence the app changed.
///
/// `nodes` is `None` when check 4 never produced a tree; that is reported as its own skip
/// reason rather than blamed on the app.
pub fn check_interaction(t: &mut dyn McpTransport, nodes: Option<&[OutlineNode]>) -> CheckOutcome {
    let Some(nodes) = nodes else {
        return CheckOutcome::skip(
            5,
            "interaction",
            "no accessibility tree was read (check 4 failed), so there was nothing to drive",
        );
    };
    let Some(target) = crate::smoke::profile::first_editable(nodes) else {
        return CheckOutcome::skip(
            5,
            "interaction",
            "the tree was read but exposes no editable element",
        );
    };
    let id = target.id;
    // An unmapped role renders as `Other(<native token>)` in the outline, which
    // `AxRole::from_name` cannot parse back, so `glass_wait_for_element`'s `role` selector
    // cannot target it. A skip beats a fail that blames the app for a role glass itself
    // cannot address.
    if glass_core::AxRole::from_name(&target.role).is_none() {
        return CheckOutcome::skip(
            5,
            "interaction",
            format!(
                "target element #{id} has no addressable role ({}) to verify against",
                target.role
            ),
        );
    }
    let role = target.role.as_str();
    let probe = probe_text();
    outcome(
        5,
        "interaction",
        (|| {
            let set = t.call("glass_set_value", json!({ "id": id, "text": probe }))?;
            check_envelope("glass_set_value", &set)?;

            let wait = t.call(
                "glass_wait_for_element",
                json!({ "role": role, "value_contains": probe, "timeout_ms": 5000 }),
            )?;
            let result = check_envelope("glass_wait_for_element", &wait)?;
            if result["matched"].as_bool() != Some(true) {
                return Err(
                    "glass_set_value returned ok but no element reported the written value"
                        .to_string(),
                );
            }
            // The matched element rides in an untrusted sibling; a missing, unparseable or
            // id-less sibling means the id cannot be confirmed, not that it mismatches — only
            // a definite mismatch fails the check, and the detail has to say which of the two
            // happened or a guard that stopped guarding reads exactly like one that held.
            let confirmation = match matched_element_id(&wait) {
                Some(matched_id) if matched_id != u64::from(id) => {
                    return Err(format!(
                        "glass_set_value wrote to #{id} but element #{matched_id} \
                         reported the value instead — the write landed somewhere unintended"
                    ));
                }
                Some(_) => "",
                None => " (id unconfirmed: the reply carried no element id)",
            };

            // Pixel path: a click inside the window, then a key. These assert the tools
            // accept and report; the element path above is what proves an effect.
            let click = t.call("glass_click", json!({ "x": 5, "y": 5 }))?;
            check_envelope("glass_click", &click)?;
            // `Home` deliberately: it moves the caret and nothing else. Anything with dismiss
            // or commit semantics — `Escape` (a GtkDialog's cancel binding), `Return`, `alt+F4`
            // — can close the target app right here, and checks 6-8 would not notice: logs
            // read a retained buffer, error honesty resolves against the cached snapshot, and
            // stopping an already-exited child is not an error. So do not "simplify" this to a
            // more familiar key.
            let key = t.call("glass_key", json!({ "chord": "Home" }))?;
            check_envelope("glass_key", &key)?;

            // ...and prove the app really did survive the pixel path, rather than trusting the
            // choice of chord. Without this, the checks after it would report on a corpse and
            // still pass.
            let live = t.call("glass_list_windows", json!({}))?;
            let live = check_envelope("glass_list_windows", &live)?;
            if live["count"].as_u64().unwrap_or(0) == 0 {
                return Err(
                    "the app has no windows left after the pixel path — it was dismissed, so \
                     the checks after this one would not be about a live session"
                        .to_string(),
                );
            }
            Ok(format!(
                "element #{id} took the value{confirmation}; pixel path ok"
            ))
        })(),
    )
}

/// The JSON object carried inside an untrusted-wrapped sibling. The wrapper is a note line,
/// the markers and the body — not bare JSON — so the object is located by its outermost
/// braces. `None` when there is no sibling, no `{...}` span, or the span doesn't parse. `get`
/// rather than indexing: a `}` before a `{` would panic on a slice, and a panic here aborts
/// the run with no report at all.
fn untrusted_json(r: &CallResult) -> Option<Value> {
    let sibling = untrusted_sibling(r).ok()?;
    let start = sibling.find('{')?;
    let end = sibling.rfind('}')?;
    serde_json::from_str::<Value>(sibling.get(start..=end)?).ok()
}

/// The matched element's `id`. `None` means "cannot confirm", never a mismatch.
fn matched_element_id(r: &CallResult) -> Option<u64> {
    untrusted_json(r)?.get("id")?.as_u64()
}

pub fn check_logs(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        6,
        "logs",
        t.call("glass_logs", json!({})).and_then(|r| {
            check_envelope("glass_logs", &r)?;
            // `glass_logs` wraps its `{"lines":[…]}` body unconditionally — an app that
            // logged nothing still gets the wrapper around an empty array. So a missing
            // sibling is not "no output", it is the untrusted wrapper having stopped being
            // emitted: the freeze violation this check exists to catch.
            let sibling = untrusted_sibling(&r)?;
            let lines = untrusted_json(&r)
                .as_ref()
                .and_then(|v| v.get("lines"))
                .and_then(Value::as_array)
                .map(Vec::len)
                .ok_or_else(|| {
                    format!(
                        "the untrusted-wrapped block is not the `{{\"lines\":[…]}}` body \
                         glass_logs emits: {}",
                        excerpt(sibling)
                    )
                })?;
            Ok(format!("{lines} line(s), untrusted-wrapped"))
        }),
    )
}

/// The element id `check_error_honesty` asks for: far outside any real snapshot's range,
/// so the server has to answer about *this* id rather than succeed by accident.
const STALE_ELEMENT_ID: u32 = 999_999;

/// Deliberate misuse must return an error that names a remedy, not only a cause.
pub fn check_error_honesty(t: &mut dyn McpTransport) -> CheckOutcome {
    const STEP: u8 = 7;
    const NAME: &str = "error honesty";
    let r = match t.call("glass_click_element", json!({ "id": STALE_ELEMENT_ID })) {
        Ok(r) => r,
        Err(e) => return CheckOutcome::fail(STEP, NAME, e),
    };
    if !r.is_error {
        return CheckOutcome::fail(
            STEP,
            NAME,
            "glass_click_element on a nonexistent id succeeded",
        );
    }
    let msg = r.siblings.join(" ");
    // The error must be about the stale id this check provoked. A run with no session, or one
    // that never took a snapshot, answers with `NoActiveSession` / `NoAxSnapshot` instead —
    // both actionable, so grading their wording would hand this check a green precisely in
    // the runs already broken further up.
    if !msg.contains(&STALE_ELEMENT_ID.to_string()) {
        return CheckOutcome::skip(
            STEP,
            NAME,
            format!(
                "precondition not met: glass_click_element answered about something other \
                 than element #{STALE_ELEMENT_ID}, so this is not the error the check \
                 provokes: {msg}"
            ),
        );
    }
    if crate::smoke::envelope::names_a_remedy(&msg) {
        CheckOutcome::pass(STEP, NAME, format!("actionable: {msg}"))
    } else {
        CheckOutcome::fail(
            STEP,
            NAME,
            format!("error names a cause but no remedy: {msg}"),
        )
    }
}

pub fn check_stop(t: &mut dyn McpTransport) -> CheckOutcome {
    outcome(
        8,
        "stop",
        t.call("glass_stop", json!({})).and_then(|r| {
            check_envelope("glass_stop", &r)?;
            Ok("session ended".into())
        }),
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::smoke::profile::{ANDROID_CANDIDATES, WAYLAND_CANDIDATES, X11_CANDIDATES};
    use crate::smoke::report::CheckStatus;
    use crate::smoke::transport::{CALL_TIMEOUT, CallResult, ScriptedTransport};
    use std::time::Duration;

    fn ok(tool: &str, result: Value, siblings: &[&str], images: usize) -> CallResult {
        CallResult::ok(tool, result, siblings, images)
    }

    fn err_call(message: &str) -> CallResult {
        CallResult {
            is_error: true,
            envelope: None,
            siblings: vec![message.to_string()],
            images: 0,
        }
    }

    fn profile() -> Profile {
        Profile {
            backend: "x11".into(),
            app: &X11_CANDIDATES[3],
            candidates: &X11_CANDIDATES,
            start_deadline: CALL_TIMEOUT,
        }
    }

    fn wayland_profile() -> Profile {
        Profile {
            backend: "wayland".into(),
            app: &WAYLAND_CANDIDATES[0],
            candidates: &WAYLAND_CANDIDATES,
            start_deadline: CALL_TIMEOUT,
        }
    }

    fn started() -> CallResult {
        ok(
            "glass_start",
            json!({ "x": 0, "y": 0, "width": 800, "height": 600 }),
            &[],
            0,
        )
    }

    #[test]
    fn start_requires_geometry_in_the_envelope() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Ok(ok(
                "glass_start",
                json!({ "x": 0, "y": 0, "width": 800, "height": 600 }),
                &[],
                0,
            )),
        )]);
        assert_eq!(check_start(&mut t, &profile()).status, CheckStatus::Pass);

        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Ok(ok("glass_start", json!({ "x": 0, "y": 0 }), &[], 0)),
        )]);
        let out = check_start(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("width"),
            "must say what was missing: {}",
            out.detail
        );
    }

    #[test]
    fn start_sends_the_candidates_env_on_the_wire() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Ok(started()))]);
        let _ = check_start(&mut t, &wayland_profile());
        let args = t.args_for("glass_start").expect("glass_start was called");
        assert_eq!(args["env"]["GDK_BACKEND"], json!("wayland"));
    }

    /// An empty `env` object is a different payload from no `env` key, and the server is free to
    /// treat them differently — so a candidate that declares none must send none.
    #[test]
    fn start_sends_no_env_key_for_a_candidate_that_needs_none() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Ok(started()))]);
        let _ = check_start(&mut t, &profile());
        let args = t.args_for("glass_start").expect("glass_start was called");
        assert!(args.get("env").is_none(), "got: {args}");
    }

    /// Check 1 is the only call that may wait out a device boot, and the profile is where that budget
    /// is decided. A hardcoded deadline here would ignore it silently.
    #[test]
    fn start_is_called_with_the_profiles_deadline() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Ok(CallResult::ok(
                "glass_start",
                json!({ "x": 0, "y": 0, "width": 8, "height": 6 }),
                &[],
                0,
            )),
        )]);
        let mut p = profile();
        p.start_deadline = Duration::from_secs(321);
        let _ = check_start(&mut t, &p);
        assert_eq!(
            t.deadline_for("glass_start"),
            Some(Duration::from_secs(321))
        );
    }

    /// A failed launch is the only place a run tells the operator what else it could have driven,
    /// because nothing probed for alternatives beforehand.
    #[test]
    fn a_failed_start_names_the_candidates_it_did_not_try() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Err("Error type 3: Activity class does not exist".to_string()),
        )]);
        let mut p = profile();
        p.app = &X11_CANDIDATES[0];
        let out = check_start(&mut t, &p);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("Activity class does not exist"),
            "the launch error must survive: {}",
            out.detail
        );
        for other in ["gnome-text-editor", "zenity", "xterm"] {
            assert!(
                out.detail.contains(other),
                "must offer {other}: {}",
                out.detail
            );
        }
        assert!(out.detail.contains("--app"), "must say how: {}", out.detail);
    }

    /// The selected candidate is not an alternative to itself; offering it would send the operator
    /// back to the run that just failed.
    #[test]
    fn the_offer_excludes_the_candidate_the_run_selected() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Err("boom".to_string()))]);
        let mut p = profile();
        p.app = &X11_CANDIDATES[0];
        let out = check_start(&mut t, &p);
        let (_, offered) = out
            .detail
            .split_once("other candidates: ")
            .unwrap_or_else(|| panic!("must carry an offer: {}", out.detail));
        assert!(
            !offered.contains(X11_CANDIDATES[0].label),
            "the failed candidate must not be offered again: {offered}"
        );
    }

    /// A table with nothing else in it has nothing to offer, and a dangling "other candidates:" would
    /// read as one.
    #[test]
    fn a_single_candidate_backend_offers_nothing() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Err("boom".to_string()))]);
        let mut p = profile();
        p.candidates = &X11_CANDIDATES[..1];
        p.app = &X11_CANDIDATES[0];
        let out = check_start(&mut t, &p);
        assert_eq!(out.detail, "boom", "nothing to offer, so nothing appended");
    }

    /// The offer belongs to failure. A passing launch must report geometry and nothing else.
    #[test]
    fn a_successful_start_carries_no_offer() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_start",
            Ok(CallResult::ok(
                "glass_start",
                json!({ "x": 0, "y": 0, "width": 8, "height": 6 }),
                &[],
                0,
            )),
        )]);
        let out = check_start(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Pass);
        assert!(
            !out.detail.contains("--app"),
            "a pass has no alternatives to offer: {}",
            out.detail
        );
    }

    /// `label` and `bin` coincide in every candidate above, so a filter or map that used `bin`
    /// where it meant `label` would still pass all of them. Android is the one table where the
    /// two diverge, and the backend this whole feature exists for — nothing probes it, so a
    /// mixed-up offer would ship silently and read out a raw component string instead of the
    /// short name `--app` accepts.
    #[test]
    fn an_android_offer_names_the_label_operators_pass_not_the_component() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Err("boom".to_string()))]);
        let mut p = profile();
        p.backend = "android".into();
        p.candidates = &ANDROID_CANDIDATES;
        p.app = &ANDROID_CANDIDATES[0];
        let out = check_start(&mut t, &p);
        let (_, offered) = out
            .detail
            .split_once("other candidates: ")
            .unwrap_or_else(|| panic!("must carry an offer: {}", out.detail));
        assert!(
            offered.contains(ANDROID_CANDIDATES[1].label),
            "must offer the settings label: {offered}"
        );
        assert!(
            !offered.contains(ANDROID_CANDIDATES[1].bin),
            "must offer the short label, not the raw component string: {offered}"
        );
    }

    /// Only the launch gets the long budget: a five-minute hang is justified where a boot can happen
    /// and nowhere else.
    #[test]
    fn a_check_that_cannot_boot_a_device_uses_the_default_deadline() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_screenshot",
            Ok(CallResult::ok(
                "glass_screenshot",
                json!({ "width": 8, "height": 6 }),
                &[],
                1,
            )),
        )]);
        let _ = check_screenshot(&mut t);
        assert_eq!(t.deadline_for("glass_screenshot"), Some(CALL_TIMEOUT));
    }

    #[test]
    fn screenshot_requires_both_an_image_and_dimensions() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_screenshot",
            Ok(ok(
                "glass_screenshot",
                json!({ "width": 800, "height": 600 }),
                &[],
                1,
            )),
        )]);
        assert_eq!(check_screenshot(&mut t).status, CheckStatus::Pass);

        let mut t = ScriptedTransport::new(vec![(
            "glass_screenshot",
            Ok(ok(
                "glass_screenshot",
                json!({ "width": 800, "height": 600 }),
                &[],
                0,
            )),
        )]);
        let out = check_screenshot(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("image"), "got: {}", out.detail);
    }

    /// Both dimensions are required. As `&&`, a response missing only one of them passed.
    #[test]
    fn screenshot_fails_when_either_dimension_is_missing() {
        for result in [json!({ "width": 8 }), json!({ "height": 8 }), json!({})] {
            let mut t = ScriptedTransport::new(vec![(
                "glass_screenshot",
                Ok(ok("glass_screenshot", result.clone(), &[], 1)),
            )]);
            let out = check_screenshot(&mut t);
            assert_eq!(
                out.status,
                CheckStatus::Fail,
                "must reject {result}: {}",
                out.detail
            );
            assert!(
                out.detail.contains("width/height"),
                "must name the missing geometry: {}",
                out.detail
            );
        }
    }

    #[test]
    fn a11y_requires_a_nonempty_tree_in_an_untrusted_sibling() {
        let outline = "⟦untrusted:abc⟧\n#1 Window \"Untitled\"\n  #2 TextField \"Body\" [editable]\n⟦/untrusted:abc⟧";
        let mut t = ScriptedTransport::new(vec![(
            "glass_a11y_snapshot",
            Ok(ok("glass_a11y_snapshot", json!({}), &[outline], 0)),
        )]);
        let (out, nodes) = check_a11y(&mut t);
        assert_eq!(out.status, CheckStatus::Pass);
        assert_eq!(nodes.expect("a passing snapshot yields a tree").len(), 2);

        // Unwrapped app text is a freeze violation, not a pass.
        let mut t = ScriptedTransport::new(vec![(
            "glass_a11y_snapshot",
            Ok(ok("glass_a11y_snapshot", json!({}), &["#1 Window"], 0)),
        )]);
        let (out, _) = check_a11y(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
    }

    #[test]
    fn a11y_yields_no_tree_when_the_snapshot_fails() {
        // The distinction `check_interaction` needs: "no tree was read" is not the same as
        // "a tree was read and had nothing editable in it".
        let mut t = ScriptedTransport::new(vec![("glass_a11y_snapshot", Err("boom".into()))]);
        let (out, nodes) = check_a11y(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(nodes.is_none(), "a failed snapshot must yield no tree");
    }

    fn caps(backend: &str) -> CallResult {
        ok("glass_capabilities", json!({ "backend": backend }), &[], 0)
    }

    /// A `glass_doctor` result mirroring the server's real payload shape
    /// (`{report, sections, overall}`), not the `{checks}` shape a stale mock once fabricated.
    fn doctor_result(overall: &str, sections: Value) -> Value {
        json!({ "report": "glass doctor\n...", "overall": overall, "sections": sections })
    }

    fn doctor(overall: &str, sections: Value) -> CallResult {
        ok("glass_doctor", doctor_result(overall, sections), &[], 0)
    }

    fn x11_sections(status: &str) -> Value {
        json!([{
            "title": "x11",
            "backend": "x11",
            "checks": [{ "name": "display", "status": status, "detail": "…" }],
        }])
    }

    #[test]
    fn health_fails_when_doctor_reports_fail() {
        let mut t = ScriptedTransport::new(vec![
            ("glass_capabilities", Ok(caps("x11"))),
            ("glass_doctor", Ok(doctor("fail", x11_sections("fail")))),
        ]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("display"),
            "must name the failing check: {}",
            out.detail
        );
    }

    #[test]
    fn health_fails_on_overall_fail_even_with_no_check_individually_reporting_it() {
        let mut t = ScriptedTransport::new(vec![
            ("glass_capabilities", Ok(caps("x11"))),
            ("glass_doctor", Ok(doctor("fail", json!([])))),
        ]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            !out.detail.is_empty(),
            "an unattributed overall fail must still say something, not go silent"
        );
    }

    #[test]
    fn health_passes_when_doctor_reports_ok() {
        let mut t = ScriptedTransport::new(vec![
            ("glass_capabilities", Ok(caps("x11"))),
            ("glass_doctor", Ok(doctor("ok", x11_sections("ok")))),
        ]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Pass);
        assert_eq!(
            out.detail, "backend x11, doctor ok",
            "must render the plain backend string and the verdict it graded: {}",
            out.detail
        );
    }

    #[test]
    fn health_fails_when_the_server_is_on_a_different_backend_than_the_one_under_test() {
        // The reproduced defect: with the server resolving another backend, `overall` applies
        // its severity rule to THAT backend — a broken x11 there is only a warning — so this
        // check reported Pass while saying nothing about x11.
        let mut t = ScriptedTransport::new(vec![
            ("glass_capabilities", Ok(caps("wayland"))),
            ("glass_doctor", Ok(doctor("warn", x11_sections("fail")))),
        ]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("wayland") && out.detail.contains("x11"),
            "must name both the graded backend and the one under test: {}",
            out.detail
        );
    }

    #[test]
    fn health_fails_when_capabilities_reports_no_backend() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_capabilities",
            Ok(ok("glass_capabilities", json!({}), &[], 0)),
        )]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("backend"), "got: {}", out.detail);
    }

    #[test]
    fn health_fails_when_doctor_omits_overall() {
        // A regression to the older `{report}`-only payload must go red, not pass: reading a
        // missing `overall` as "not fail" is a check that cannot fail.
        let mut t = ScriptedTransport::new(vec![
            ("glass_capabilities", Ok(caps("x11"))),
            (
                "glass_doctor",
                Ok(ok(
                    "glass_doctor",
                    json!({ "report": "glass doctor\n..." }),
                    &[],
                    0,
                )),
            ),
        ]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("overall"), "got: {}", out.detail);
    }

    #[test]
    fn health_fails_when_overall_is_not_a_known_verdict() {
        let mut t = ScriptedTransport::new(vec![
            ("glass_capabilities", Ok(caps("x11"))),
            ("glass_doctor", Ok(doctor("green", json!([])))),
        ]);
        let out = check_health(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("green"),
            "must name what it got: {}",
            out.detail
        );
    }

    #[test]
    fn a_transport_error_is_a_failure_not_a_panic() {
        let mut t = ScriptedTransport::new(vec![("glass_start", Err("broken pipe".into()))]);
        let out = check_start(&mut t, &profile());
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("broken pipe"));
    }

    fn nodes_with_editable() -> Vec<OutlineNode> {
        vec![OutlineNode {
            id: 12,
            role: "TextField".into(),
            name: None,
            states: vec!["editable".into()],
        }]
    }

    /// A `glass_wait_for_element` match, wrapped exactly as the real tool wraps it: a note
    /// line, the nonce-delimited markers, then the element JSON — not bare JSON. Going through
    /// the real wrapper is what actually exercises `matched_element_id`'s extraction.
    fn matched_element(id: u32) -> String {
        let body = json!({
            "id": id,
            "role": "TextField",
            "name": Value::Null,
            "value": "glass-smoke-0000beef",
            "bounds": Value::Null,
            "states": ["editable"],
        })
        .to_string();
        crate::untrusted::wrap_untrusted(&body)
    }

    /// The calls `check_interaction` makes after the element path is confirmed: the pixel
    /// path, then the liveness re-read that proves the app survived it.
    fn pixel_path_script(windows: u64) -> Vec<(&'static str, Result<CallResult, String>)> {
        vec![
            ("glass_click", Ok(ok("glass_click", json!({}), &[], 0))),
            ("glass_key", Ok(ok("glass_key", json!({}), &[], 0))),
            (
                "glass_list_windows",
                Ok(ok(
                    "glass_list_windows",
                    json!({ "count": windows }),
                    &[],
                    0,
                )),
            ),
        ]
    }

    fn interaction_script(
        matched: Value,
        siblings: &[&str],
        tail: Vec<(&'static str, Result<CallResult, String>)>,
    ) -> Vec<(&'static str, Result<CallResult, String>)> {
        let mut script = vec![
            (
                "glass_set_value",
                Ok(ok("glass_set_value", json!({ "id": 12 }), &[], 0)),
            ),
            (
                "glass_wait_for_element",
                Ok(ok("glass_wait_for_element", matched, siblings, 0)),
            ),
        ];
        script.extend(tail);
        script
    }

    /// A fixed probe string could collide with text the app already shows, so each run gets a
    /// distinct one carrying a recognizable prefix.
    #[test]
    fn probe_text_is_distinctive_and_fresh_each_call() {
        let a = probe_text();
        let b = probe_text();
        assert!(a.starts_with("glass-smoke-"), "got: {a}");
        assert_ne!(
            a, b,
            "a constant probe cannot distinguish what the run wrote"
        );
    }

    #[test]
    fn interaction_verifies_at_the_consumer_layer_not_by_ok() {
        // set_value ok, then glass_wait_for_element — the signal glass actually exposes for a
        // written value — confirms it landed on the same element.
        let sibling = matched_element(12);
        let mut t = ScriptedTransport::new(interaction_script(
            json!({ "matched": true, "elapsed_ms": 5 }),
            &[sibling.as_str()],
            pixel_path_script(1),
        ));
        let out = check_interaction(&mut t, Some(&nodes_with_editable()));
        assert_eq!(out.status, CheckStatus::Pass);
        assert!(
            !out.detail.contains("unconfirmed"),
            "a confirmed id must not be reported as unconfirmed: {}",
            out.detail
        );
    }

    #[test]
    fn interaction_passes_when_matched_but_the_id_cannot_be_confirmed() {
        // matched:true with no untrusted sibling to read an id from: "cannot confirm" is not
        // a failure — only a definite id mismatch is.
        let mut t = ScriptedTransport::new(interaction_script(
            json!({ "matched": true, "elapsed_ms": 5 }),
            &[],
            pixel_path_script(1),
        ));
        let out = check_interaction(&mut t, Some(&nodes_with_editable()));
        assert_eq!(out.status, CheckStatus::Pass);
        assert!(
            out.detail.contains("unconfirmed"),
            "must say the id was not confirmed: {}",
            out.detail
        );
    }

    #[test]
    fn interaction_fails_when_the_app_is_gone_after_the_pixel_path() {
        // The failure mode the liveness re-read exists for: the key dismissed the app. The
        // checks after it would all still pass against the dead session.
        let mut t = ScriptedTransport::new(interaction_script(
            json!({ "matched": true, "elapsed_ms": 5 }),
            &[],
            pixel_path_script(0),
        ));
        let out = check_interaction(&mut t, Some(&nodes_with_editable()));
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("dismissed"), "got: {}", out.detail);
    }

    #[test]
    fn interaction_fails_when_no_element_reports_the_written_value() {
        let mut t = ScriptedTransport::new(interaction_script(
            json!({ "matched": false, "elapsed_ms": 5000 }),
            &[],
            vec![],
        ));
        let out = check_interaction(&mut t, Some(&nodes_with_editable()));
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("no element"), "got: {}", out.detail);
    }

    #[test]
    fn interaction_fails_when_a_different_element_reports_the_value() {
        // The probe text shows up, but on #99, not the #12 we wrote to: the write landed
        // somewhere unintended, which is a real defect this check must catch.
        let sibling = matched_element(99);
        let mut t = ScriptedTransport::new(interaction_script(
            json!({ "matched": true, "elapsed_ms": 5 }),
            &[sibling.as_str()],
            vec![],
        ));
        let out = check_interaction(&mut t, Some(&nodes_with_editable()));
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(
            out.detail.contains("12") && out.detail.contains("99"),
            "must name both ids: {}",
            out.detail
        );
    }

    #[test]
    fn interaction_skips_when_no_editable_element_exists() {
        let mut t = ScriptedTransport::new(vec![]);
        let nodes = vec![OutlineNode {
            id: 1,
            role: "Button".into(),
            name: None,
            states: vec![],
        }];
        let out = check_interaction(&mut t, Some(&nodes));
        assert_eq!(out.status, CheckStatus::Skip);
        assert!(
            out.detail.contains("editable"),
            "must blame the tree's contents, not the read: {}",
            out.detail
        );
    }

    #[test]
    fn interaction_skips_with_a_different_reason_when_no_tree_was_read() {
        // Distinct from "nothing editable": blaming the app for a snapshot that never
        // happened sends triage the wrong way.
        let mut t = ScriptedTransport::new(vec![]);
        let out = check_interaction(&mut t, None);
        assert_eq!(out.status, CheckStatus::Skip);
        assert!(
            out.detail.contains("check 4"),
            "must point at the snapshot check: {}",
            out.detail
        );
    }

    #[test]
    fn interaction_skips_when_the_target_has_no_addressable_role() {
        // `Other(...)` is what an unmapped role renders as in the outline; `AxRole::from_name`
        // returns `None` for exactly those, so `glass_wait_for_element`'s `role` selector
        // cannot target it. This must skip with a reason, not fail and blame the app.
        let mut t = ScriptedTransport::new(vec![]);
        let nodes = vec![OutlineNode {
            id: 7,
            role: "Other(AXDisclosureTriangle)".into(),
            name: None,
            states: vec!["editable".into()],
        }];
        let out = check_interaction(&mut t, Some(&nodes));
        assert_eq!(out.status, CheckStatus::Skip);
        assert!(out.detail.contains("role"), "got: {}", out.detail);
    }

    #[test]
    fn error_honesty_requires_an_actionable_message() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_click_element",
            Ok(err_call(&format!("unknown element id {STALE_ELEMENT_ID}"))),
        )]);
        let out = check_error_honesty(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("remedy"), "got: {}", out.detail);
    }

    /// The real wording `glass-core` answers a stale id with, built from the live enum so
    /// this tracks its actual text rather than a frozen copy of it.
    #[test]
    fn error_honesty_passes_on_the_real_stale_id_error() {
        let msg = glass_core::GlassError::AxElementNotFound(STALE_ELEMENT_ID).to_string();
        let mut t = ScriptedTransport::new(vec![("glass_click_element", Ok(err_call(&msg)))]);
        assert_eq!(check_error_honesty(&mut t).status, CheckStatus::Pass);
    }

    /// Reproduced live: check 4 failed, and this check then went green on `NoAxSnapshot`'s
    /// remedy — an error about a missing snapshot, not about the stale id it set out to
    /// provoke. Green precisely in a run already broken.
    #[test]
    fn error_honesty_skips_when_the_error_is_not_about_the_stale_id() {
        let msg = glass_core::GlassError::NoAxSnapshot.to_string();
        let mut t = ScriptedTransport::new(vec![("glass_click_element", Ok(err_call(&msg)))]);
        let out = check_error_honesty(&mut t);
        assert_eq!(out.status, CheckStatus::Skip);
        assert!(
            out.detail.contains("precondition"),
            "must say the precondition was not met: {}",
            out.detail
        );
    }

    /// The same shape for a run whose session never started: `NoActiveSession` is actionable
    /// too, so without the id guard this check would pass on it.
    #[test]
    fn error_honesty_skips_when_there_was_no_session() {
        let msg = glass_core::GlassError::NoActiveSession.to_string();
        let mut t = ScriptedTransport::new(vec![("glass_click_element", Ok(err_call(&msg)))]);
        assert_eq!(check_error_honesty(&mut t).status, CheckStatus::Skip);
    }

    #[test]
    fn error_honesty_fails_when_a_bad_call_succeeds() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_click_element",
            Ok(ok(
                "glass_click_element",
                json!({ "id": STALE_ELEMENT_ID }),
                &[],
                0,
            )),
        )]);
        let out = check_error_honesty(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("succeeded"), "got: {}", out.detail);
    }

    /// The body `glass_logs` really emits, through the real wrapper.
    fn logs_sibling(lines: Value) -> String {
        crate::untrusted::wrap_untrusted(&json!({ "lines": lines }).to_string())
    }

    #[test]
    fn logs_must_arrive_untrusted_wrapped() {
        let sibling = logs_sibling(json!([{ "seq": 1, "stream": "stdout", "text": "started" }]));
        let mut t = ScriptedTransport::new(vec![(
            "glass_logs",
            Ok(ok(
                "glass_logs",
                json!({ "cursor": 1 }),
                &[sibling.as_str()],
                0,
            )),
        )]);
        let out = check_logs(&mut t);
        assert_eq!(out.status, CheckStatus::Pass);
        assert!(out.detail.contains('1'), "got: {}", out.detail);
    }

    #[test]
    fn logs_pass_on_an_empty_but_still_wrapped_buffer() {
        // An app that logged nothing is normal — glass_logs still wraps the empty body.
        let sibling = logs_sibling(json!([]));
        let mut t = ScriptedTransport::new(vec![(
            "glass_logs",
            Ok(ok(
                "glass_logs",
                json!({ "cursor": 0 }),
                &[sibling.as_str()],
                0,
            )),
        )]);
        assert_eq!(check_logs(&mut t).status, CheckStatus::Pass);
    }

    #[test]
    fn logs_fail_when_the_untrusted_wrapper_stops_being_emitted() {
        // The freeze violation this check exists for. It used to read as "no output" and
        // pass — the one outcome that made the check unable to fail.
        let mut t = ScriptedTransport::new(vec![(
            "glass_logs",
            Ok(ok("glass_logs", json!({ "cursor": 0 }), &[], 0)),
        )]);
        let out = check_logs(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("untrusted"), "got: {}", out.detail);
    }

    #[test]
    fn logs_fail_when_the_wrapped_body_is_not_the_lines_payload() {
        let sibling = crate::untrusted::wrap_untrusted(&json!({ "rows": [] }).to_string());
        let mut t = ScriptedTransport::new(vec![(
            "glass_logs",
            Ok(ok(
                "glass_logs",
                json!({ "cursor": 0 }),
                &[sibling.as_str()],
                0,
            )),
        )]);
        let out = check_logs(&mut t);
        assert_eq!(out.status, CheckStatus::Fail);
        assert!(out.detail.contains("lines"), "got: {}", out.detail);
    }

    #[test]
    fn excerpt_returns_short_input_unchanged() {
        assert_eq!(excerpt("short"), "short");
    }

    /// The boundary the `<=` decides: exactly MAX characters is not too long.
    #[test]
    fn excerpt_truncates_only_past_the_limit() {
        let at_limit: String = "x".repeat(120);
        assert_eq!(excerpt(&at_limit), at_limit);

        let over = "x".repeat(121);
        let cut = excerpt(&over);
        assert!(cut.ends_with('…'), "must mark the truncation: {cut}");
        assert_eq!(
            cut.chars().count(),
            121,
            "120 kept plus the ellipsis: {cut}"
        );
    }

    #[test]
    fn stop_must_return_a_clean_envelope() {
        let mut t = ScriptedTransport::new(vec![(
            "glass_stop",
            Ok(ok("glass_stop", json!({}), &[], 0)),
        )]);
        assert_eq!(check_stop(&mut t).status, CheckStatus::Pass);
    }

    #[test]
    fn a_closing_brace_before_an_opening_one_does_not_panic() {
        // `matched_element_id` locates the body by its outermost braces; a sibling shaped
        // like this would panic on a slice, and a panic loses the report.
        let r = CallResult {
            is_error: false,
            envelope: None,
            siblings: vec!["⟦untrusted:abc⟧\n} not json {\n⟦/untrusted:abc⟧".into()],
            images: 0,
        };
        assert_eq!(matched_element_id(&r), None);
    }
}
