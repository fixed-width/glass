import json
from pathlib import Path
import sys
import tempfile
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from cases import evaluate_setup
from measurement import call_metrics, canonical, file_identity, phase_totals, write_json
from run import GlassDriver, load_config, validate_attempt
import run as runner


class RunnerTests(unittest.TestCase):
    def test_cleanup_failure_halts_remaining_schedule_and_optional_skips_are_explicit(
        self,
    ):
        for optional in (False, True):
            with (
                self.subTest(optional=optional),
                tempfile.TemporaryDirectory() as directory,
            ):
                root = Path(directory)
                supplied = {
                    "browser": sys.executable,
                    "cases": ["disabled"],
                    "repetitions": 2,
                    "warmups": 0,
                    "drivers": [
                        {"id": "glass", "adapter": "glass", "command": [sys.executable]}
                    ],
                }
                if optional:
                    supplied.update(
                        optional_cases=["disabled"],
                        exclusions=[
                            {
                                "driver": "glass",
                                "case": "disabled",
                                "reason": "declared unavailable",
                            }
                        ],
                    )
                write_json(root / "config.json", supplied)
                config = load_config(root / "config.json", {"glass": GlassDriver})

                def fail(config, cell, directory, fixtures, adapter):
                    directory.mkdir()
                    row = {
                        **cell,
                        "outcome": "failed",
                        "cleanup_ok": False,
                        "evidence_ok": True,
                        "interrupted": False,
                        "files": {},
                        "wall_ms": 1,
                    }
                    write_json(directory / "result.json", row)
                    return row

                with (
                    patch.object(
                        runner,
                        "preflight",
                        return_value={"ready": True, "source": {"dirty": ""}},
                    ),
                    patch.object(runner, "Fixtures"),
                    patch.object(runner, "attempt", side_effect=fail) as launch,
                    patch("builtins.print"),
                ):
                    self.assertEqual(
                        runner.run(config, root / "run", {"glass": GlassDriver}),
                        0 if optional else 1,
                    )
                    self.assertEqual(launch.call_count, 0 if optional else 1)
                rows = [
                    json.loads(p.read_bytes())
                    for p in sorted((root / "run").glob("*/result.json"))
                ]
                self.assertEqual(rows[-1]["outcome"], "skipped")
                self.assertEqual(rows[-1]["optional_exclusion"], optional)

    def test_invalid_configurations_are_rejected_before_launch(self):
        base = {
            "browser": sys.executable,
            "drivers": [
                {"id": "sample", "adapter": "glass", "command": [sys.executable]}
            ],
        }
        for change in (
            {"repetitions": 0},
            {"repetitions": True},
            {"cases": []},
            {"cases": ["typo"]},
            {"drivers": []},
            {"warmups": -1},
            {"viewport": [True, 700]},
            {"repetition": 10},
            {"optional_cases": ["typo"]},
            {
                "exclusions": [
                    {"driver": "sample", "case": "disabled", "reason": "unavailable"}
                ]
            },
        ):
            with (
                self.subTest(change=change),
                tempfile.TemporaryDirectory() as directory,
            ):
                path = Path(directory) / "config.json"
                path.write_text(json.dumps({**base, **change}))
                with self.assertRaises(ValueError):
                    load_config(path, {"glass": GlassDriver})

    def test_geometry_is_observed_not_assumed_from_window_size(self):
        ready = {
            "step": "ready",
            "facts": {
                "error": False,
                "nodes": [{"name": "Fixture ready", "value": "ready"}],
            },
        }
        geometry = {
            "step": "geometry",
            "facts": {
                "error": False,
                "nodes": [{"name": "Content geometry", "value": "1000x615@1"}],
            },
        }
        self.assertTrue(evaluate_setup([ready, geometry], [1000, 700]))
        geometry["facts"]["nodes"][0]["value"] = "1000x700@1"
        self.assertEqual(evaluate_setup([ready, geometry], [1000, 700]), [])

    def test_offline_validation_recomputes_costs_from_wire(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "mcp").mkdir()
            request = {
                "jsonrpc": "2.0",
                "id": 1,
                "method": "tools/call",
                "params": {"name": "sample", "arguments": {}},
            }
            response = {
                "jsonrpc": "2.0",
                "id": 1,
                "result": {"content": [{"type": "text", "text": "雪"}]},
            }
            (root / "mcp/0001.request.json").write_bytes(canonical(request))
            (root / "mcp/0001.response.json").write_bytes(canonical(response))
            call = {
                **call_metrics(request, response),
                "sequence": 1,
                "step": "sample",
                "phase": "task",
                "method": "tools/call",
                "request_file": "0001.request.json",
                "response_file": "0001.response.json",
                "request_wire_bytes": len(canonical(request)),
                "response_wire_bytes": len(canonical(response)),
            }
            write_json(root / "mcp/calls.json", [call])
            durations = {"task": 3}
            write_json(root / "timing.json", {"wall_ms": 4, "durations": durations})
            result = {
                "outcome": "failed",
                "case": "disabled",
                "events": [],
                "artifacts": {},
                "wall_ms": 4,
                "phases": phase_totals([call], durations),
                "files": {},
            }
            for path in root.rglob("*"):
                if path.is_file():
                    result["files"][str(path.relative_to(root))] = file_identity(path)
            self.assertEqual(validate_attempt(root, result, GlassDriver), [])
            (root / "evidence").mkdir()
            body = root / "evidence/note.txt"
            body.write_bytes(b"note")
            artifact = {**file_identity(body), "path": "note.txt"}
            sha = artifact["sha256"]
            result["artifacts"] = {sha: artifact}
            result["artifact_references"] = [
                {"source": "output/note.txt", "sha256": sha, "origin_call": 1}
            ]
            result["files"]["evidence/note.txt"] = file_identity(body)
            result["file_reads"] = [
                {
                    "method": "files/read",
                    "phase": "evidence",
                    "file_reads": 1,
                    "response_text_bytes": 4,
                    "sha256": sha,
                    "origin_call": 1,
                }
            ]
            result["phases"] = phase_totals([call] + result["file_reads"], durations)
            self.assertEqual(validate_attempt(root, result, GlassDriver), [])
            result["file_reads"][0]["tool_calls"] = 100
            result["phases"] = phase_totals([call] + result["file_reads"], durations)
            self.assertIn(
                "file read contains invalid method, phase or counters",
                validate_attempt(root, result, GlassDriver),
            )
            del result["file_reads"][0]["tool_calls"]
            result["phases"] = phase_totals([call] + result["file_reads"], durations)
            result["phases"]["task"]["response_text_bytes"] = 0
            self.assertIn(
                "phase totals differ from recorded calls and timing",
                validate_attempt(root, result, GlassDriver),
            )


if __name__ == "__main__":
    unittest.main()
