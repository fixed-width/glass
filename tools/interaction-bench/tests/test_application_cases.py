import sys
import unittest
from pathlib import Path

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from application_cases import Oracle, evaluate, evaluate_setup


def field(step, name, value, participant="app"):
    return {
        "step": step,
        "participant": participant,
        "facts": {"error": False, "nodes": [{"name": name, "value": value}]},
    }


class ApplicationOracleTests(unittest.TestCase):
    def test_empty_reset_requires_exact_confirmed_write(self):
        from copy import deepcopy

        fact = {
            "error": False,
            "nodes": [],
            "set_value": {
                "target": {"query": "Account name", "role": "TextField"},
                "text": "",
            },
            "result": {"dispatch": "dispatched", "confirmation": "value_confirmed"},
        }

        def errors(value):
            oracle = Oracle([{"participant": "app", "step": "empty", "facts": value}])
            oracle.value("empty", "Account name", "")
            return oracle.errors

        self.assertFalse(errors(fact))
        for key, value in (
            ("confirmation", "unconfirmed"),
            ("dispatch", "not_dispatched"),
        ):
            wrong = deepcopy(fact)
            wrong["result"][key] = value
            self.assertTrue(errors(wrong))
        for key, value in (
            ("text", "Ada"),
            ("target", {"query": "Other", "role": "TextField"}),
        ):
            wrong = deepcopy(fact)
            wrong["set_value"][key] = value
            self.assertTrue(errors(wrong))
        self.assertTrue(
            errors({"error": False, "nodes": [{"name": "Account name", "value": None}]})
        )

    def test_native_completion_requires_saved_value_and_one_submission(self):
        errors = evaluate(
            "native-form",
            [
                field("saved", "Saved value", "Ada"),
                field("count", "Submission count", "2"),
            ],
        )
        self.assertTrue(any("count" in error for error in errors))

    def test_native_result_cannot_substitute_for_crossing_android_boundary(self):
        events = [
            field("native_saved", "Native saved value: Ada", None),
            field("native_count", "Native submission count: 1", None),
            field("native_reviews", "Native review count: 1", None),
        ]
        self.assertTrue(evaluate("android-boundary", events))
        self.assertTrue(evaluate_setup("android-boundary", events, [1000, 700]))

    def test_transfer_rejects_hardcoded_or_wrong_participant_proof(self):
        value = "ticket-12-" + "a" * 32
        events = [
            field("source_value", "Source value", value, "source"),
            field("typed", "Account name", "Ada", "destination"),
            field("saved", "Saved value", "Ada", "destination"),
        ]
        errors = evaluate("cross-application", events)
        self.assertTrue(any("typed" in error for error in errors))
        events[0]["participant"] = "destination"
        self.assertTrue(
            any(
                "participant" in error
                for error in evaluate("cross-application", events)
            )
        )

    def test_electron_confirmation_requires_observed_native_window(self):
        events = [
            field("confirmed", "Confirmed value", "Ada"),
            field("confirmation_count", "Confirmation count", "1"),
        ]
        self.assertTrue(
            any("dialog" in error for error in evaluate("electron-form", events))
        )


if __name__ == "__main__":
    unittest.main()
