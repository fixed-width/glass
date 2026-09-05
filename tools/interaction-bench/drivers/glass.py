"""Glass recipes use only public MCP tools and resources."""

import re
import time

from evidence import EvidenceError


def normalize(request, decoded):
    envelope = decoded["envelope"] or {}
    result = envelope.get("result", {})
    nodes = []
    for value in decoded["observations"]:
        if isinstance(value, dict):
            if "matches" in value:
                nodes.extend(value["matches"])
            elif "role" in value:
                nodes.append(value)
    facts = {
        "error": decoded["is_error"],
        "code": envelope.get("error", {}).get("code"),
        "result": result,
        "nodes": nodes,
    }
    if request["name"] == "glass_a11y_snapshot":
        body = "\n".join(
            value for value in decoded["observations"] if isinstance(value, str)
        )
        facts["snapshot"] = {
            "bytes": len(body.encode()),
            "last_section": "Repeated section 199" in body,
            "account": "Account name" in body,
        }
    return facts


class Driver:
    name = "glass"
    required_tools = {
        "glass_start",
        "glass_stop",
        "glass_find_elements",
        "glass_wait_for_element",
        "glass_type",
        "glass_click_element",
        "glass_window",
        "glass_a11y_snapshot",
    }

    def __init__(self, config, session, client, evidence, deadline):
        self.config, self.session, self.client, self.evidence = (
            config,
            session,
            client,
            evidence,
        )
        self.deadline = deadline
        self.events = []

    def call(self, step, name, arguments, allow_error=False):
        self.client.step = step
        timeout = min(
            self.config["action_timeout_ms"] / 1000 + 5,
            self.deadline - time.monotonic(),
        )
        if self.client.phase == "app_start":
            timeout = min(35, self.deadline - time.monotonic())
        if timeout <= 0:
            raise TimeoutError("attempt action deadline exhausted")
        request = {"name": name, "arguments": arguments}
        reply = self.client.rpc("tools/call", request, timeout=timeout)
        origin = self.client.calls[-1]["sequence"]
        if self.client.calls[-1]["response_text_bytes"] > 8192:
            raise EvidenceError("Glass tool reply exceeded its inline text budget")
        self.evidence.timeout = max(0.01, self.deadline - time.monotonic())
        decoded = self.evidence.decode(name, reply)
        facts = normalize(request, decoded)
        self.events.append({"step": step, "call": origin, "facts": facts})
        if facts["error"] and not allow_error:
            raise EvidenceError(
                f"{step}: {name} failed ({facts['code']}): {decoded['texts'][0][:300]}"
            )
        return facts

    def target(self, name, role="Button", scope=None):
        return {
            "query": name,
            "role": role,
            "within": scope or {"query": "Interaction fixture", "role": "Document"},
        }

    def launch(self, url):
        self.call(
            "launch",
            "glass_start",
            {
                "run": self.session.browser_args(
                    self.config["browser"],
                    self.config["browser_family"],
                    url,
                    self.config.get("browser_args", []),
                ),
                "backend": "x11",
                "sandbox": self.config["sandbox"],
                "a11y": True,
                "timeout_ms": 30000,
                "window_hint": {"title": "Interaction fixture"},
                "env": {"LIBGL_ALWAYS_SOFTWARE": "1", **self.config.get("app_env", {})},
            },
        )
        self.read("ready", "Fixture ready", "ready")
        current = self.read("initial_geometry", "Content geometry")
        value = current["nodes"][0]["value"] if current["nodes"] else ""
        match = re.fullmatch(r"(\d+)x(\d+)@([\d.]+)", value or "")
        if not match or match[3] != "1":
            raise EvidenceError(f"unknown content geometry: {value}")
        geometry = self.call("window_geometry", "glass_window", {"op": "geometry"})[
            "result"
        ]
        width, height = self.config["viewport"]
        self.call(
            "resize",
            "glass_window",
            {
                "op": "resize",
                "width": geometry["width"] + width - int(match[1]),
                "height": geometry["height"] + height - int(match[2]),
            },
        )
        self.read("geometry", "Content geometry", f"{width}x{height}@1")

    def read(self, step, name, expected=None, scope=None):
        arguments = {
            "name": name,
            "role": "TextField",
            "timeout_ms": self.config["action_timeout_ms"],
        }
        if expected is not None:
            arguments["value"] = expected
        return self.call(step, "glass_wait_for_element", arguments)

    def find(self, step, name, role="Button", scope=None):
        return self.call(
            step,
            "glass_find_elements",
            {**self.target(name, role, scope), "max_results": 20},
        )

    def click(self, step, name, *, scope=None, negative=False):
        return self.call(
            step,
            "glass_click_element",
            {
                "target": self.target(name, scope=scope),
                "mode": "pointer",
                "timeout_ms": 1000 if negative else self.config["action_timeout_ms"],
            },
            allow_error=negative,
        )

    def type(self, step, name, value):
        return self.call(
            step,
            "glass_type",
            {
                "target": self.target(name, "TextField"),
                "text": value,
                "focus_mode": "native",
                "timeout_ms": self.config["action_timeout_ms"],
            },
        )

    def snapshot(self):
        return self.call("artifact_snapshot", "glass_a11y_snapshot", {"max_nodes": 0})

    def stop(self):
        self.call("stop", "glass_stop", {})
