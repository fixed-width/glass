"""Versioned workflows and a separate evaluator of their observed facts."""

import time

REVISION = 1
CASES = {
    "large-form": "task_completed",
    "disabled": "expected_refusal",
    "duplicate": "expected_refusal",
    "scoped": "task_completed",
    "delayed": "task_completed",
    "moving": "task_completed",
    "occluded": "expected_refusal",
    "occluded-distinct": "expected_refusal",
    "mutation": "task_completed",
    "iframe": "task_completed",
    "cross-origin": "task_completed",
    "artifact": "task_completed",
}


def evaluate_setup(events, viewport):
    facts = {e["step"]: e["facts"] for e in events}
    errors = []
    for step, name, value in (
        ("ready", "Fixture ready", "ready"),
        ("geometry", "Content geometry", f"{viewport[0]}x{viewport[1]}@1"),
    ):
        observed = facts.get(step, {})
        nodes = observed.get("nodes", [])
        if (
            observed.get("error") is not False
            or len(nodes) != 1
            or nodes[0].get("name") != name
            or nodes[0].get("value") != value
        ):
            errors.append(f"{step}: fixture readiness/geometry not established")
    return errors


def execute(case, driver):
    read, click, find = driver.read, driver.click, driver.find
    if case in ("large-form", "artifact"):
        find("discover_field", "Account name", "TextField")
        find("discover_save", "Save account")
        read("empty", "Account name", "")
        if case == "artifact":
            driver.snapshot()
        else:
            driver.type("type", "Account name", "Ada")
            read("typed", "Account name", "Ada")
            click("action", "Save account")
            read("saved", "Saved value", "Ada")
            read("count", "Submission count", "1")
            time.sleep(0.35)
            read("quiet_count", "Submission count", "1")
    elif case in ("disabled", "occluded", "occluded-distinct"):
        read("before", "Action count", "0")
        if case.startswith("occluded"):
            find("target", "Covered action")
            find("cover", "Cover action")
        click(
            "action",
            "Disabled action" if case == "disabled" else "Covered action",
            negative=True,
        )
        read("count", "Action count", "0")
        time.sleep(0.35)
        read("quiet_count", "Action count", "0")
        if case.startswith("occluded"):
            read("cover_count", "Cover count")
    elif case in ("duplicate", "scoped"):
        find("duplicates", "Duplicate action")
        scope = (
            {"role": "Group", "query": "Billing group"} if case == "scoped" else None
        )
        click("action", "Duplicate action", scope=scope, negative=case == "duplicate")
        read("billing", "Billing group count", "1" if case == "scoped" else "0")
        read("shipping", "Shipping group count", "0")
        time.sleep(0.35)
        read("quiet_billing", "Billing group count", "1" if case == "scoped" else "0")
    elif case == "delayed":
        find("absent", "Delayed action")
        click("trigger", "Start delay")
        find("still_absent", "Delayed action")
        click("action", "Delayed action")
        read("count", "Action count", "1")
        time.sleep(0.35)
        read("quiet_count", "Action count", "1")
    elif case == "moving":
        click("trigger", "Start motion")
        find("motion_first", "Moving action")
        time.sleep(0.15)
        find("motion_second", "Moving action")
        click("action", "Moving action")
        read("count", "Action count", "1")
        time.sleep(0.35)
        read("quiet_count", "Action count", "1")
    elif case == "mutation":
        find("original", "Current action")
        click("trigger", "Replace target")
        find("replacement", "Current action")
        click("action", "Current action")
        read("current", "Current count", "1")
        read("retired", "Retired count", "0")
        time.sleep(0.35)
        read("quiet_current", "Current count", "1")
    elif case in ("iframe", "cross-origin"):
        find("frame", "Inner fixture", "Document")
        click(
            "action",
            "Frame action",
            scope={"role": "Document", "query": "Inner fixture"},
        )
        read(
            "inner",
            "Inner count",
            "1",
            scope={"role": "Document", "query": "Inner fixture"},
        )
        read("outer", "Outer count", "0")
        time.sleep(0.35)
        read(
            "quiet_inner",
            "Inner count",
            "1",
            scope={"role": "Document", "query": "Inner fixture"},
        )
    else:
        raise ValueError(f"unknown case {case}")


def evaluate(case, events, artifacts):
    facts = {e["step"]: e["facts"] for e in events}
    errors = []
    if len(facts) != len(events):
        errors.append("duplicate evidence step")

    def value(step, name, expected):
        fact = facts.get(step, {})
        nodes = fact.get("nodes", [])
        if (
            fact.get("error") is not False
            or len(nodes) != 1
            or nodes[0].get("name") != name
            or nodes[0].get("value") != expected
        ):
            errors.append(f"{step}: expected {name} = {expected!r}")

    def find(step, name, count=1):
        fact = facts.get(step, {})
        nodes = fact.get("nodes", [])
        if (
            fact.get("error") is not False
            or fact.get("result", {}).get("search_complete") is not True
            or len(nodes) != count
            or any(n.get("name") != name for n in nodes)
        ):
            errors.append(f"{step}: expected {count} complete exact matches for {name}")
        return nodes

    def action(step, refusal=None):
        fact = facts.get(step, {})
        result = fact.get("result", {})
        if refusal:
            if (
                fact.get("error") is not True
                or fact.get("code") != refusal
                or result.get("dispatch") != "not_dispatched"
            ):
                errors.append(f"{step}: expected {refusal} with no dispatch")
        elif (
            fact.get("error") is not False
            or result.get("dispatch", result.get("type_dispatch")) != "dispatched"
        ):
            errors.append(f"{step}: missing successful dispatch")

    if case in ("large-form", "artifact"):
        find("discover_field", "Account name")
        find("discover_save", "Save account")
        value("empty", "Account name", "")
        if case == "artifact":
            snapshot = facts.get("artifact_snapshot", {}).get("snapshot", {})
            if (
                not artifacts
                or facts.get("artifact_snapshot", {}).get("error") is not False
                or snapshot.get("bytes", 0) <= 8192
                or not snapshot.get("last_section")
                or not snapshot.get("account")
            ):
                errors.append("no recovered oversized observation")
        else:
            action("type")
            typed = facts.get("type", {}).get("result", {})
            if (
                "type_dispatch" in typed
                and typed.get("focus_confirmation") != "focus_confirmed"
            ):
                errors.append("type: native focus was not confirmed")
            action("action")
            value("typed", "Account name", "Ada")
            value("saved", "Saved value", "Ada")
            value("count", "Submission count", "1")
            value("quiet_count", "Submission count", "1")
    elif case in ("disabled", "occluded", "occluded-distinct"):
        action("action", "not_actionable")
        for step in ("before", "count", "quiet_count"):
            value(step, "Action count", "0")
        if case.startswith("occluded"):
            target, cover = (
                find("target", "Covered action"),
                find("cover", "Cover action"),
            )
            if target and cover:
                a, b = target[0].get("bounds"), cover[0].get("bounds")
                if (
                    not a
                    or not b
                    or not (
                        b["x"] <= a["x"] + a["width"] / 2 < b["x"] + b["width"]
                        and b["y"] <= a["y"] + a["height"] / 2 < b["y"] + b["height"]
                    )
                ):
                    errors.append("occluder does not cover target center")
            value("cover_count", "Cover count", "0")
    elif case in ("duplicate", "scoped"):
        find("duplicates", "Duplicate action", 2)
        action("action", "ambiguous_target" if case == "duplicate" else None)
        value("billing", "Billing group count", "1" if case == "scoped" else "0")
        value("quiet_billing", "Billing group count", "1" if case == "scoped" else "0")
        value("shipping", "Shipping group count", "0")
    elif case in ("moving", "delayed"):
        action("trigger")
        action("action")
        value("count", "Action count", "1")
        value("quiet_count", "Action count", "1")
        if case == "delayed":
            find("absent", "Delayed action", 0)
            find("still_absent", "Delayed action", 0)
        else:
            a, b = (
                find("motion_first", "Moving action"),
                find("motion_second", "Moving action"),
            )
            if (
                not a
                or not b
                or not a[0].get("bounds")
                or not b[0].get("bounds")
                or a[0]["bounds"] == b[0]["bounds"]
            ):
                errors.append("motion was not observed in this generation")
    elif case == "mutation":
        a, b = find("original", "Current action"), find("replacement", "Current action")
        if (
            not a
            or not b
            or (
                a[0].get("id") == b[0].get("id")
                and a[0].get("bounds") == b[0].get("bounds")
            )
        ):
            errors.append("replacement identity/geometry was not observed")
        action("trigger")
        action("action")
        value("current", "Current count", "1")
        value("quiet_current", "Current count", "1")
        value("retired", "Retired count", "0")
    elif case in ("iframe", "cross-origin"):
        find("frame", "Inner fixture")
        action("action")
        value("inner", "Inner count", "1")
        value("quiet_inner", "Inner count", "1")
        value("outer", "Outer count", "0")
    else:
        errors.append(f"unknown case {case}")
    return errors
