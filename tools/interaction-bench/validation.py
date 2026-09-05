"""Replay channel evidence independently of case scheduling and live drivers."""

import json
import tempfile
from pathlib import Path

from drivers.glass import normalize as glass_normalize
from evidence import Evidence, EvidenceError
from measurement import (
    COUNTERS,
    PHASES,
    call_metrics,
    file_identity,
    owned_path,
    phase_totals,
    verify_files,
)


class ReplayClient:
    def __init__(self, directory, calls, origin):
        self.directory, self.all_calls = directory, calls
        self.calls = [{"sequence": origin}]
        self.origin = origin

    def rpc(self, method, params, **kwargs):
        for call in self.all_calls:
            if call["sequence"] <= self.origin:
                continue
            if call["method"] == "tools/call":
                break
            request = json.loads(
                owned_path(self.directory, call["request_file"]).read_bytes()
            )
            if (
                request["method"] == method
                and request["params"] == params
                and call["response_file"]
            ):
                self.origin = call["sequence"]
                return json.loads(
                    owned_path(self.directory, call["response_file"]).read_bytes()
                )
        raise EvidenceError("missing recorded resource read")


def read_channel(directory, result, adapter):
    errors = verify_files(directory, result["files"])
    calls_path = directory / "mcp/calls.json"
    if not calls_path.is_file():
        return errors + ["missing call ledger"], [], []
    calls = json.loads(calls_path.read_bytes())
    sequences = [c["sequence"] for c in calls]
    if sequences != list(range(1, len(calls) + 1)):
        errors.append("RPC ledger sequences are not contiguous and unique")
    for call in calls:
        try:
            request_path = owned_path(directory / "mcp", call["request_file"])
            raw = request_path.read_bytes()
            request = json.loads(raw)
            if (
                request["id"] != call["sequence"]
                or request["method"] != call["method"]
                or len(raw) != call["request_wire_bytes"]
            ):
                errors.append(f"request accounting mismatch: {call['sequence']}")
            response = {"result": {}}
            if call["response_file"]:
                raw = owned_path(directory / "mcp", call["response_file"]).read_bytes()
                response = json.loads(raw)
                if (
                    response["id"] != call["sequence"]
                    or len(raw) != call["response_wire_bytes"]
                ):
                    errors.append(f"response accounting mismatch: {call['sequence']}")
            for key, expected in call_metrics(request, response).items():
                if (
                    key not in ("request_wire_bytes", "response_wire_bytes")
                    and call.get(key, 0) != expected
                ):
                    errors.append(f"call metric mismatch: {call['sequence']} {key}")
        except (ValueError, KeyError, OSError) as exc:
            errors.append(f"invalid call record: {exc}")
    indexed = {c["sequence"]: c for c in calls}
    for sha, artifact in result["artifacts"].items():
        try:
            identity = file_identity(
                owned_path(directory / "evidence", artifact["path"])
            )
            if sha != artifact["sha256"] or identity != {
                "bytes": artifact["bytes"],
                "sha256": sha,
            }:
                errors.append("artifact metadata differs from archived body")
        except (ValueError, KeyError, OSError) as exc:
            errors.append(f"invalid archived artifact: {exc}")
    for read in result.get("file_reads", []):
        artifact = result["artifacts"].get(read.get("sha256"), {})
        expected_metrics = {key: 0 for key in COUNTERS}
        expected_metrics.update(file_reads=1, response_text_bytes=artifact.get("bytes"))
        if (
            read.get("method") != "files/read"
            or read.get("phase") not in PHASES
            or any(read.get(key, 0) != value for key, value in expected_metrics.items())
        ):
            errors.append("file read contains invalid method, phase or counters")
        if (
            read.get("file_reads") != 1
            or read.get("response_text_bytes") != artifact.get("bytes")
            or read.get("origin_call") not in indexed
            or not any(
                ref["sha256"] == read.get("sha256")
                and ref["origin_call"] == read.get("origin_call")
                for ref in result.get("artifact_references", [])
            )
        ):
            errors.append("file read accounting differs from artifact evidence")
    if result["case"] == "large-form":
        task_calls = [c for c in calls if c["phase"] == "task"]
        if any(
            c.get("images")
            or c.get("tool") in ("glass_snapshot", "glass_a11y_snapshot")
            for c in task_calls
        ):
            errors.append("normal form workflow acquired an image or full snapshot")
    reconstructed = []
    with tempfile.TemporaryDirectory(prefix="interaction-validate-") as temporary:
        for event in result["events"]:
            try:
                call = indexed[event["call"]]
                if call["step"] != event["step"] or call["response_file"] is None:
                    raise EvidenceError("event does not match its RPC ledger entry")
                request = json.loads(
                    owned_path(directory / "mcp", call["request_file"]).read_bytes()
                )
                response = json.loads(
                    owned_path(directory / "mcp", call["response_file"]).read_bytes()
                )
                if hasattr(adapter, "replay"):
                    facts = adapter.replay(
                        request["params"], response, directory, result
                    )
                else:
                    evidence = Evidence(
                        Path(temporary),
                        ReplayClient(directory / "mcp", calls, event["call"]),
                    )
                    facts = glass_normalize(
                        request["params"],
                        evidence.decode(request["params"]["name"], response),
                    )
                if facts != event["facts"]:
                    errors.append(f"changed interpreted facts: {event['step']}")
                reconstructed.append({"step": event["step"], "facts": facts})
            except (ValueError, KeyError, OSError, EvidenceError) as exc:
                errors.append(f"{event['step']}: {exc}")
    return errors, reconstructed, calls


def validate_totals(directory, result, calls):
    errors = []
    timing = json.loads((directory / "timing.json").read_bytes())
    if result["wall_ms"] != timing["wall_ms"] or result["phases"] != phase_totals(
        calls + result.get("file_reads", []), timing["durations"]
    ):
        errors.append("phase totals differ from recorded calls and timing")
    return errors
