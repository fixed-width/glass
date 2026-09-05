import copy
import json
from pathlib import Path
import subprocess
import sys
import tempfile
import time
import unittest
from unittest.mock import Mock, patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from android_session import Android
from application_run import validate_application
from drivers.glass import normalize
from evidence import Evidence
from measurement import (
    canonical,
    call_metrics,
    digest,
    file_identity,
    phase_totals,
    write_json,
)
from run import load_config, GlassDriver


class ApplicationRunTests(unittest.TestCase):
    def test_replay_charges_both_participants_and_rejects_swapped_provenance(self):
        with tempfile.TemporaryDirectory() as temporary:
            root = Path(temporary)
            records, events, calls = {}, [], []
            for index, name in enumerate(("source", "destination")):
                directory = root / name
                (directory / "mcp").mkdir(parents=True)
                inventory = [{"name": "glass_key", "inputSchema": {"type": "object"}}]
                request = {
                    "id": 1,
                    "method": "tools/call",
                    "params": {"name": "glass_key", "arguments": {"chord": "Return"}},
                }
                reply = {
                    "id": 1,
                    "result": {
                        "content": [
                            {
                                "type": "text",
                                "text": '{"ok":true,"tool":"glass_key","result":{}}',
                            }
                        ]
                    },
                }
                for filename, body in (
                    ("request.json", request),
                    ("response.json", reply),
                    ("inventory.json", inventory),
                ):
                    (directory / "mcp" / filename).write_bytes(canonical(body))
                call = {
                    **call_metrics(request, reply),
                    "sequence": 1,
                    "step": "key",
                    "phase": "task",
                    "method": "tools/call",
                    "started": float(index),
                    "request_file": "request.json",
                    "response_file": "response.json",
                    "request_wire_bytes": len(canonical(request)),
                    "response_wire_bytes": len(canonical(reply)),
                }
                calls.append(call)
                write_json(directory / "mcp/calls.json", [call])
                facts = normalize(
                    request["params"],
                    Evidence(
                        directory / "evidence", Mock(calls=[{"sequence": 1}])
                    ).decode("glass_key", reply),
                )
                event = {"step": "key", "call": 1, "facts": facts}
                events.append({**event, "participant": name})
                records[name] = {
                    "case": "cross-application",
                    "kind": "native" if index == 0 else "electron",
                    "events": [event],
                    "artifacts": {},
                    "file_reads": [],
                    "files": {},
                    "tool_definitions_bytes": len(canonical(inventory)),
                    "tool_definitions_sha256": digest(inventory),
                }
                records[name]["files"] = {
                    str(p.relative_to(directory)): file_identity(p)
                    for p in directory.rglob("*")
                    if p.is_file()
                }
            result = {
                "case": "cross-application",
                "outcome": "failed",
                "participants": records,
                "events": events,
                "files": {},
                "wall_ms": 4,
                "phases": phase_totals(calls, {"task": 3}),
                "tool_definitions_bytes": sum(
                    r["tool_definitions_bytes"] for r in records.values()
                ),
                "tool_definitions_sha256": digest(
                    {n: r["tool_definitions_sha256"] for n, r in records.items()}
                ),
            }
            write_json(root / "timing.json", {"wall_ms": 4, "durations": {"task": 3}})
            self.assertEqual(validate_application(root, result, {}), [])
            changed = copy.deepcopy(result)
            changed["phases"] = phase_totals(calls[:1], {"task": 3})
            self.assertIn(
                "phase totals differ from recorded calls and timing",
                validate_application(root, changed, {}),
            )
            changed = copy.deepcopy(result)
            changed["events"][0]["participant"] = "destination"
            self.assertIn(
                "combined events differ from participant wire evidence",
                validate_application(root, changed, {}),
            )
            result["tool_definitions_bytes"] //= 2
            self.assertIn(
                "combined inventory accounting mismatch",
                validate_application(root, result, {}),
            )

    def test_android_shutdown_reaps_processes_after_adb_failure(self):
        android = Android.__new__(Android)
        android.process, android.server = Mock(), Mock()
        android.process.poll.return_value = None
        android.adb, android.serial, android.streams = "adb", "emulator-5554", []
        android.command = Mock(side_effect=subprocess.TimeoutExpired("adb", 5))
        self.assertFalse(android.close())
        android.process.terminate.assert_called_once()
        android.process.wait.assert_called_once()
        android.server.terminate.assert_called_once()
        android.server.wait.assert_called_once()

    def test_android_preparation_retains_partial_timeout_output(self):
        with tempfile.TemporaryDirectory() as temporary:
            android = Android.__new__(Android)
            android.directory, android.deadline = Path(temporary), time.monotonic() + 10
            android.session, android.commands = Mock(env={}), []
            with patch(
                "android_session.subprocess.run",
                side_effect=subprocess.TimeoutExpired("adb", 1, output=b"partial"),
            ):
                with self.assertRaises(subprocess.TimeoutExpired):
                    android.command(["adb"])
            record = json.loads(
                (android.directory / "android-preparation.json").read_bytes()
            )[0]
            self.assertEqual(
                (record["error"], record["stdout"], record["exit_code"]),
                ("timeout", "partial", None),
            )

    def test_application_config_has_no_browser_requirement_and_rejects_ineligible_adapter(
        self,
    ):
        with tempfile.TemporaryDirectory() as temporary:
            path = Path(temporary) / "config.json"
            config = {
                "cases": ["native-form"],
                "applications": {"native": {"executable": sys.executable}},
                "drivers": [
                    {"id": "glass", "adapter": "glass", "command": [sys.executable]}
                ],
            }
            write_json(path, config)
            self.assertIsNone(load_config(path, {"glass": GlassDriver})["browser"])
            config["drivers"][0]["adapter"] = "browser-only"
            write_json(path, config)
            with self.assertRaisesRegex(ValueError, "eligible"):
                load_config(path, {"browser-only": GlassDriver})


if __name__ == "__main__":
    unittest.main()
