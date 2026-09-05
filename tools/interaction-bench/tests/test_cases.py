from pathlib import Path
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from cases import evaluate


def field(step, name, value):
    return {
        "step": step,
        "facts": {"error": False, "nodes": [{"name": name, "value": value}]},
    }


class OracleTests(unittest.TestCase):
    def test_successful_dispatch_cannot_substitute_for_saved_value(self):
        events = [
            {
                "step": "action",
                "facts": {"error": False, "result": {"dispatch": "dispatched"}},
            }
        ]
        errors = evaluate("large-form", events, [])
        self.assertTrue(any("Saved value" in e for e in errors))
        self.assertTrue(any("Submission count" in e for e in errors))

    def test_wrong_or_repeated_submission_fails(self):
        events = [
            field("saved", "Saved value", "Wrong"),
            field("count", "Submission count", "2"),
        ]
        errors = evaluate("large-form", events, [])
        self.assertTrue(any("saved:" in e for e in errors))
        self.assertTrue(any("count:" in e for e in errors))

    def test_refusal_requires_no_dispatch_and_unchanged_state(self):
        events = [
            {
                "step": "action",
                "facts": {
                    "error": True,
                    "code": "not_actionable",
                    "result": {"dispatch": "not_dispatched"},
                },
            }
        ]
        events += [
            field(step, "Action count", "0")
            for step in ("before", "count", "quiet_count")
        ]
        self.assertEqual(evaluate("disabled", events, []), [])
        events[-1]["facts"]["nodes"][0]["value"] = "1"
        self.assertTrue(evaluate("disabled", events, []))

    def test_unsupported_and_missing_motion_are_not_passes(self):
        self.assertTrue(evaluate("moving", [], []))
        self.assertTrue(evaluate("artifact", [], []))


if __name__ == "__main__":
    unittest.main()
