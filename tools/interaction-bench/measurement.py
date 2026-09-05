"""Stable identities, evidence checks, scheduling and distribution summaries."""

from collections import Counter
import hashlib
import json
import math
from pathlib import Path
import random
import statistics as stats

PHASES = ("server_start", "app_start", "task", "evidence", "cleanup")
OUTCOMES = (
    "task_completed",
    "expected_refusal",
    "failed",
    "unsupported",
    "skipped",
    "harness_error",
)
COUNTERS = (
    "tool_calls",
    "rpc_calls",
    "request_wire_bytes",
    "response_wire_bytes",
    "response_text_bytes",
    "images",
    "image_encoded_bytes",
    "image_decoded_bytes",
    "resource_reads",
    "file_reads",
    "action_count",
    "retries",
)


def canonical(value):
    return json.dumps(
        value,
        sort_keys=True,
        separators=(",", ":"),
        ensure_ascii=False,
        allow_nan=False,
    ).encode("utf-8")


def digest(value):
    return hashlib.sha256(
        value if isinstance(value, bytes) else canonical(value)
    ).hexdigest()


def file_identity(path):
    path = Path(path)
    h = hashlib.sha256()
    with path.open("rb") as stream:
        for chunk in iter(lambda: stream.read(1024 * 1024), b""):
            h.update(chunk)
    return {"bytes": path.stat().st_size, "sha256": h.hexdigest()}


def write_json(path, value):
    path = Path(path)
    temporary = path.with_suffix(path.suffix + ".tmp")
    temporary.write_bytes(canonical(value) + b"\n")
    temporary.replace(path)


def owned_path(root, relative):
    root = Path(root).resolve()
    relative = Path(relative)
    if relative.is_absolute() or ".." in relative.parts:
        raise ValueError(f"unsafe evidence path: {relative}")
    path = root / relative
    if any(p.is_symlink() for p in (path, *path.parents) if p != root.parent):
        raise ValueError(f"symlink in evidence path: {relative}")
    if not path.resolve().is_relative_to(root):
        raise ValueError(f"evidence path leaves output directory: {relative}")
    return path


def verify_files(root, files):
    errors = []
    for relative, expected in files.items():
        try:
            if file_identity(owned_path(root, relative)) != expected:
                errors.append(f"changed evidence: {relative}")
        except (OSError, ValueError) as exc:
            errors.append(str(exc))
    return errors


def schedule(cases, drivers, repetitions, warmups, seed):
    rng = random.Random(seed)
    order = list(cases)
    rng.shuffle(order)
    rows = []
    for case in order:
        arms = list(drivers)
        rng.shuffle(arms)
        for iteration in range(warmups + repetitions):
            for driver in arms if iteration % 2 == 0 else arms[::-1]:
                rows.append(
                    {
                        "case": case,
                        "driver": driver,
                        "iteration": iteration,
                        "warmup": iteration < warmups,
                    }
                )
    return rows


def statistics(values):
    values = sorted(values)
    return {
        "n": len(values),
        "min": min(values) if values else None,
        "median": stats.median(values) if values else None,
        "max": max(values) if values else None,
        "p95": values[math.ceil(len(values) * 0.95) - 1] if len(values) >= 20 else None,
    }


def call_metrics(request, response):
    import base64

    result = response.get("result", {})
    blocks = (
        result.get("content", result.get("contents", []))
        if isinstance(result, dict)
        else []
    )
    metrics = {key: 0 for key in COUNTERS}
    metrics["rpc_calls"] = 1
    metrics["tool_calls"] = int(request["method"] == "tools/call")
    metrics["resource_reads"] = int(request["method"] == "resources/read")
    params = request.get("params", {})
    if metrics["tool_calls"]:
        actions = params.get("arguments", {}).get("actions")
        metrics["action_count"] = len(actions) if isinstance(actions, list) else 1
    for block in blocks:
        if isinstance(block.get("text"), str):
            metrics["response_text_bytes"] += len(block["text"].encode("utf-8"))
        if block.get("type") == "image":
            metrics["images"] += 1
            data = block.get("data", "")
            metrics["image_encoded_bytes"] += len(data.encode("ascii"))
            metrics["image_decoded_bytes"] += len(base64.b64decode(data, validate=True))
    return metrics


def phase_totals(calls, durations):
    result = {}
    for phase in PHASES:
        selected = [row for row in calls if row["phase"] == phase]
        result[phase] = {
            key: sum(row.get(key, 0) for row in selected) for key in COUNTERS
        }
        result[phase]["elapsed_ms"] = durations.get(phase, 0)
        result[phase]["internal_polls"] = None
        result[phase]["image_dimensions"] = None
    return result


def summarize(rows):
    groups = {}
    for row in rows:
        groups.setdefault((row["driver"], row["case"]), []).append(row)
    result = []
    for (driver, case), attempts in sorted(groups.items()):
        measured = [r for r in attempts if not r["warmup"]]
        counts = Counter(r["outcome"] for r in measured)
        healthy = [r for r in measured if r.get("cleanup_ok") and r.get("evidence_ok")]
        completed = [r for r in healthy if r["outcome"] == "task_completed"]
        refused = [r for r in healthy if r["outcome"] == "expected_refusal"]
        failed = [r for r in measured if r["outcome"] in ("failed", "harness_error")]
        result.append(
            {
                "driver": driver,
                "case": case,
                "scheduled": len(attempts),
                "warmups": len(attempts) - len(measured),
                "measured": len(measured),
                "attempted": sum(r["outcome"] != "skipped" for r in measured),
                "optional_exclusions": sum(
                    bool(r.get("optional_exclusion")) for r in measured
                ),
                "counts": {name: counts[name] for name in OUTCOMES},
                "cleanup_failures": sum(
                    not r.get("cleanup_ok", True) for r in measured
                ),
                "evidence_failures": sum(
                    not r.get("evidence_ok", True) for r in measured
                ),
                "successful_task_ms": statistics(
                    [r["phases"]["task"]["elapsed_ms"] for r in completed]
                ),
                "failed_wall_ms": statistics([r["wall_ms"] for r in failed]),
                "expected_refusal_ms": statistics(
                    [r["phases"]["task"]["elapsed_ms"] for r in refused]
                ),
                "refusal_cost": {
                    key: statistics([r["phases"]["task"].get(key, 0) for r in refused])
                    for key in COUNTERS
                },
                "task_cost": {
                    key: statistics(
                        [
                            r.get("phases", {}).get("task", {}).get(key, 0)
                            for r in completed
                        ]
                    )
                    for key in COUNTERS
                },
            }
        )
    return result
