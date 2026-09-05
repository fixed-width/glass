"""Owned Simulator lifecycle and native/WebView publication evidence."""

import json
import subprocess
import time
import uuid

from measurement import write_json


class IOS:
    def __init__(self, session, config, directory, deadline):
        self.session, self.config, self.directory, self.deadline = (
            session,
            config,
            directory,
            deadline,
        )
        self.udid, self.commands = None, []

    def command(self, arguments, timeout=30):
        record = {"argv": ["xcrun", "simctl", *arguments], "exit_code": None}
        started = time.monotonic()
        try:
            result = subprocess.run(
                record["argv"],
                capture_output=True,
                env=self.session.env,
                timeout=max(0.01, min(timeout, self.deadline - started)),
            )
            record.update(
                exit_code=result.returncode,
                stdout=result.stdout.decode(errors="replace"),
                stderr=result.stderr.decode(errors="replace"),
            )
            if result.returncode:
                raise RuntimeError(
                    f"Simulator preparation failed: {record['stderr'][:500]}"
                )
            return record["stdout"].strip()
        except subprocess.TimeoutExpired as exc:
            record.update(
                error="timeout",
                stdout=(exc.stdout or b"").decode(errors="replace"),
                stderr=(exc.stderr or b"").decode(errors="replace"),
            )
            raise
        finally:
            record["elapsed_ms"] = (time.monotonic() - started) * 1000
            self.commands.append(record)
            write_json(self.directory / "ios-preparation.json", self.commands)

    def start(self):
        baseline = json.loads(self.command(["list", "devices", "--json"]))
        name = "interaction-" + self.session.token
        self.udid = self.command(
            ["create", name, self.config["device_type"], self.config["runtime"]]
        )
        uuid.UUID(self.udid)
        self.command(["boot", self.udid])
        self.command(["bootstatus", self.udid, "-b"], timeout=180)
        self.session.env.update(
            GLASS_IOS_UDID=self.udid,
            GLASS_SIMULATOR_KEEP="1",
            GLASS_IDB_COMPANION=self.config["companion"],
        )
        return {
            "udid": self.udid,
            "runtime": self.config["runtime"],
            "device_type": self.config["device_type"],
            "baseline_devices": baseline,
        }

    def close(self):
        if not self.udid:
            return True
        self.deadline = time.monotonic() + 30
        healthy = True
        for action in ("shutdown", "delete"):
            try:
                self.command([action, self.udid], timeout=12)
            except Exception:
                healthy = False
        listing = json.loads(self.command(["list", "devices", "--json"]))
        remaining = any(
            d["udid"].lower() == self.udid.lower()
            for devices in listing["devices"].values()
            for d in devices
        )
        if not remaining:
            self.udid = None
        return healthy and not remaining


def observe(app, stage, name):
    app.call(
        stage + "_field",
        "glass_wait_for_element",
        {
            "name": name,
            "role": "TextField",
            "timeout_ms": app.config["action_timeout_ms"],
        },
        allow_error=True,
    )
    app.call(
        stage + "_snapshot", "glass_a11y_snapshot", {"max_depth": 50}, allow_error=True
    )
    app.call(stage + "_capture", "glass_screenshot", {})


def execute(app):
    observe(app, "native", "the-described-field")
    app.call(
        "launch_web",
        "glass_start",
        {
            "run": [app.config["applications"]["ios"]["app"], "--tab=web"],
            "backend": "ios",
            "sandbox": app.config["sandbox"],
            "a11y": True,
            "timeout_ms": 30000,
        },
    )
    observe(app, "web", "Account name")
    app.call(
        "web_save",
        "glass_find_elements",
        {
            "query": "Save account",
            "role": "Button",
            "max_results": 20,
        },
        allow_error=True,
    )


def evaluate(events):
    facts = {e["step"]: e["facts"] for e in events if e["participant"] == "app"}
    errors = []
    for step in ("launch", "launch_web", "native_capture", "web_capture"):
        fact = facts.get(step, {})
        if fact.get("error") is not False or (
            step.endswith("_capture") and fact.get("images") != 1
        ):
            errors.append(f"{step}: missing successful launch/capture evidence")
    for stage in ("native", "web"):
        for suffix in ("field", "snapshot"):
            if stage + "_" + suffix not in facts:
                errors.append(f"{stage}_{suffix}: missing publication probe")
    if "web_save" not in facts:
        errors.append("web_save: missing publication probe")
    if errors:
        return "failed", errors
    published = True
    for step, name, role in (
        ("native_field", "the-described-field", "TextField"),
        ("web_field", "Account name", "TextField"),
        ("web_save", "Save account", "Button"),
    ):
        fact = facts.get(step, {})
        nodes = fact.get("nodes", [])
        complete = (
            fact.get("result", {}).get(
                "matched" if step.endswith("field") else "search_complete"
            )
            is True
        )
        published &= (
            fact.get("error") is False
            and complete
            and len(nodes) == 1
            and nodes[0].get("name") == name
            and nodes[0].get("role") == role
        )
    return ("task_completed" if published else "unsupported"), []


def validate_probe(directory, record, config):
    """An unsupported result still requires the declared probes and owned lifecycle."""
    from measurement import owned_path

    errors = []
    expected = {
        "launch": (
            "glass_start",
            {"run": [config["app"], "--tab=controls"], "backend": "ios"},
        ),
        "launch_web": (
            "glass_start",
            {"run": [config["app"], "--tab=web"], "backend": "ios"},
        ),
        "native_field": (
            "glass_wait_for_element",
            {"name": "the-described-field", "role": "TextField"},
        ),
        "web_field": (
            "glass_wait_for_element",
            {"name": "Account name", "role": "TextField"},
        ),
        "web_save": (
            "glass_find_elements",
            {"query": "Save account", "role": "Button", "max_results": 20},
        ),
    }
    for stage in ("native", "web"):
        expected[stage + "_snapshot"] = ("glass_a11y_snapshot", {"max_depth": 50})
        expected[stage + "_capture"] = ("glass_screenshot", {})
    calls = json.loads((directory / "mcp/calls.json").read_bytes())
    for step, (tool, arguments) in expected.items():
        selected = [
            c for c in calls if c["method"] == "tools/call" and c["step"] == step
        ]
        if len(selected) != 1:
            errors.append(f"{step}: missing unique probe request")
            continue
        request = json.loads(
            owned_path(directory / "mcp", selected[0]["request_file"]).read_bytes()
        )["params"]
        if request.get("name") != tool or any(
            request.get("arguments", {}).get(k) != v for k, v in arguments.items()
        ):
            errors.append(f"{step}: request differs from publication recipe")
    sequence = [e["step"] for e in record["events"]]
    ordered = [
        "launch",
        "native_field",
        "native_snapshot",
        "native_capture",
        "launch_web",
        "web_field",
        "web_snapshot",
        "web_capture",
        "web_save",
        "stop",
    ]
    if sequence != ordered:
        errors.append("publication observations are missing or out of order")
    preparation = json.loads((directory / "ios-preparation.json").read_bytes())
    udid = record["device"]["udid"]
    create = [c for c in preparation if c["argv"][2] == "create"]
    if len(create) != 1 or create[0].get("stdout", "").strip() != udid:
        errors.append("owned Simulator creation identity mismatch")
    for action in ("boot", "bootstatus", "shutdown", "delete"):
        selected = [c for c in preparation if c["argv"][2] == action]
        if (
            len(selected) != 1
            or selected[0]["exit_code"] != 0
            or selected[0]["argv"][3] != udid
        ):
            errors.append(f"Simulator {action} lacks successful owned-device evidence")
    final = preparation[-1]
    if final["argv"][2:] != ["list", "devices", "--json"] or final["exit_code"] != 0:
        errors.append("missing final Simulator inventory")
    else:
        listing = json.loads(final["stdout"])
        if any(
            d["udid"].lower() == udid.lower()
            for ds in listing["devices"].values()
            for d in ds
        ):
            errors.append("owned Simulator remains after deletion")
    return errors
