import sys
import json
import tempfile
import unittest
from copy import deepcopy
from pathlib import Path
from unittest.mock import Mock

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from ios_publication import evaluate, IOS, validate_probe


def event(step, **facts):
    return {"step": step, "participant": "app", "facts": {"error": False, **facts}}


class PublicationTests(unittest.TestCase):
    def test_empty_web_publication_is_unsupported_with_capture_evidence(self):
        events = [
            event("launch"),
            event("launch_web"),
            event("web_save", result={"search_complete": True}, nodes=[]),
        ]
        for stage in ("native", "web"):
            events += [
                event(stage + "_capture", images=1),
                event(stage + "_snapshot", snapshot={"bytes": 200}),
                event(stage + "_field", result={"matched": False}, nodes=[]),
            ]
        self.assertEqual(evaluate(events), ("unsupported", []))
        broken = deepcopy(events)
        broken[-3]["facts"]["images"] = 0
        self.assertEqual(evaluate(broken)[0], "failed")
        self.assertTrue(evaluate(broken)[1])

    def test_unsupported_replay_rejects_wrong_query_and_foreign_cleanup(self):
        with tempfile.TemporaryDirectory() as temporary:
            directory = Path(temporary)
            (directory / "mcp").mkdir()
            requests = [
                (
                    "launch",
                    "glass_start",
                    {"run": ["fixture.app", "--tab=controls"], "backend": "ios"},
                ),
                (
                    "native_field",
                    "glass_wait_for_element",
                    {"name": "the-described-field", "role": "TextField"},
                ),
                ("native_snapshot", "glass_a11y_snapshot", {"max_depth": 50}),
                ("native_capture", "glass_screenshot", {}),
                (
                    "launch_web",
                    "glass_start",
                    {"run": ["fixture.app", "--tab=web"], "backend": "ios"},
                ),
                (
                    "web_field",
                    "glass_wait_for_element",
                    {"name": "Account name", "role": "TextField"},
                ),
                ("web_snapshot", "glass_a11y_snapshot", {"max_depth": 50}),
                ("web_capture", "glass_screenshot", {}),
                (
                    "web_save",
                    "glass_find_elements",
                    {"query": "Save account", "role": "Button", "max_results": 20},
                ),
                ("stop", "glass_stop", {}),
            ]
            calls = []
            for step, tool, arguments in requests:
                filename = step + ".json"
                (directory / "mcp" / filename).write_text(
                    json.dumps({"params": {"name": tool, "arguments": arguments}})
                )
                calls.append(
                    {"method": "tools/call", "step": step, "request_file": filename}
                )
            (directory / "mcp/calls.json").write_text(json.dumps(calls))
            preparation = [
                {
                    "argv": ["xcrun", "simctl", "create"],
                    "exit_code": 0,
                    "stdout": "OWNED",
                }
            ]
            preparation += [
                {"argv": ["xcrun", "simctl", action, "OWNED"], "exit_code": 0}
                for action in ("boot", "bootstatus", "shutdown", "delete")
            ]
            preparation.append(
                {
                    "argv": ["xcrun", "simctl", "list", "devices", "--json"],
                    "exit_code": 0,
                    "stdout": '{"devices": {}}',
                }
            )
            path = directory / "ios-preparation.json"
            path.write_text(json.dumps(preparation))
            record = {
                "device": {"udid": "OWNED"},
                "events": [{"step": s} for s, _, _ in requests],
            }
            self.assertFalse(validate_probe(directory, record, {"app": "fixture.app"}))
            (directory / "mcp/web_field.json").write_text(
                json.dumps(
                    {
                        "params": {
                            "name": "glass_wait_for_element",
                            "arguments": {"name": "nonsense", "role": "TextField"},
                        }
                    }
                )
            )
            self.assertTrue(
                any(
                    "web_field" in e
                    for e in validate_probe(directory, record, {"app": "fixture.app"})
                )
            )
            preparation[4]["argv"][3] = "UNRELATED"
            path.write_text(json.dumps(preparation))
            self.assertTrue(
                any(
                    "delete" in e
                    for e in validate_probe(directory, record, {"app": "fixture.app"})
                )
            )

    def test_created_identifier_preserves_companion_case(self):
        device = IOS.__new__(IOS)
        device.session = Mock(token="run", env={})
        device.config = {
            "device_type": "type",
            "runtime": "runtime",
            "companion": "companion",
        }
        identity = "71504757-050A-4962-8B9D-DAF2A665F6F4"
        device.command = Mock(side_effect=['{"devices": {}}', identity, "", ""])
        self.assertEqual(device.start()["udid"], identity)
        self.assertEqual(device.session.env["GLASS_IOS_UDID"], identity)

    def test_cleanup_targets_only_created_device_even_after_shutdown_failure(self):
        device = IOS.__new__(IOS)
        device.udid = "owned-uuid"
        device.command = Mock(
            side_effect=[RuntimeError("shutdown failed"), "", '{"devices": {}}']
        )
        self.assertFalse(device.close())
        self.assertEqual(
            [c.args[0] for c in device.command.call_args_list],
            [
                ["shutdown", "owned-uuid"],
                ["delete", "owned-uuid"],
                ["list", "devices", "--json"],
            ],
        )
        self.assertIsNone(device.udid)
