import copy
import hashlib
from pathlib import Path
import random
import sys
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from evidence import EvidenceError
from snapshot_revisions import SnapshotReceiver, lines


def packet(outline, revision="r1", *, kind="full", edits=None, base=None, context="c1"):
    result = {
        "version": 1, "kind": kind, "revision": revision, "context_id": context,
        "outline_format": "compact-v1", "body_sha256": hashlib.sha256(outline.encode()).hexdigest(),
        "body_bytes": len(outline.encode()), "line_count": len(lines(outline)), "cacheable": True,
        "completeness": {"count": 2, "truncated": None, "unreadable": 0, "unexposed": 0, "subject_mismatch": False},
    }
    payload = {"outline": outline} if kind == "full" else {"edits": edits}
    if kind == "full":
        result["full_reason"] = "requested"
    else:
        result["base_revision"] = base
    return {"is_error": False, "envelope": {"ok": True, "tool": "glass_a11y_snapshot_diff", "result": result}, "observations": [payload]}


class SnapshotRevisionTests(unittest.TestCase):
    def establish(self, outline="a\nb\nc"):
        receiver = SnapshotReceiver()
        receiver.begin(1)
        receiver.accept(1, packet(outline))
        return receiver

    def test_hand_constructed_reverse_edits_preserve_unicode_cr_and_final_line(self):
        receiver = self.establish("first\r\nduplicate\nduplicate\nlast")
        current = "first\r\n新👩‍💻 e\u0301\nduplicate\nfinal"
        delta = packet(current, "r2", kind="diff", base="r1", edits=[
            {"start": 1, "delete": 1, "insert": ["新👩‍💻 e\u0301\n"]},
            {"start": 3, "delete": 1, "insert": ["final"]},
        ])
        receiver.begin(2, "r1")
        self.assertEqual(receiver.accept(2, delta)["outline"], current)

    def test_lf_alone_is_a_terminator_and_empty_has_zero_lines(self):
        self.assertEqual(lines("a\rb\u2028c\nlast"), ["a\rb\u2028c\n", "last"])
        self.assertEqual(lines(""), [])
        receiver = SnapshotReceiver()
        receiver.begin(1)
        self.assertEqual(receiver.accept(1, packet(""))["body_sha256"], "e3b0c44298fc1c149afbf4c8996fb92427ae41e4649b934ca495991b7852b855")

    def test_invalid_edits_leave_baseline_intact(self):
        invalid = [
            [{"start": True, "delete": 0, "insert": []}],
            [{"start": -1, "delete": 0, "insert": []}],
            [{"start": 4, "delete": 0, "insert": []}],
            [{"start": 0, "delete": 4, "insert": []}],
            [{"start": 0, "delete": 0, "insert": [1]}],
            [{"start": 0, "delete": 0, "insert": [""]}],
            [{"start": 0, "delete": 0, "insert": ["two\nlines\n"]}],
            [{"start": 0, "delete": 0, "insert": ["unterminated"]}],
            [{"start": 1, "delete": 2, "insert": []}, {"start": 2, "delete": 0, "insert": []}],
            [{"start": 1, "delete": 0, "insert": []}, {"start": 1, "delete": 0, "insert": []}],
        ]
        for edits in invalid:
            with self.subTest(edits=edits):
                receiver = self.establish()
                baseline = receiver.baseline
                receiver.begin(2, "r1")
                with self.assertRaises(EvidenceError):
                    receiver.accept(2, packet("a\nb\nc", "r2", kind="diff", edits=edits, base="r1"))
                self.assertIs(receiver.baseline, baseline)

    def test_corrupt_format_context_counts_and_hash_require_full_recovery(self):
        for field, value in [("version", 2), ("body_bytes", True), ("line_count", 999), ("body_sha256", "0" * 64), ("context_id", "other"), ("base_revision", "lost"), ("cacheable", False)]:
            with self.subTest(field=field):
                receiver = self.establish()
                baseline = receiver.baseline
                delta = packet("a\nb\nc", "r2", kind="unchanged", base="r1", edits=[])
                delta["envelope"]["result"][field] = value
                receiver.begin(2, "r1")
                with self.assertRaises(EvidenceError):
                    receiver.accept(2, delta)
                self.assertIs(receiver.baseline, baseline)
                receiver.begin(3)
                self.assertEqual(receiver.accept(3, packet("recovered\n", "r3"))["outline"], "recovered\n")

    def test_out_of_order_and_duplicate_full_responses_cannot_advance_state(self):
        receiver = self.establish()
        receiver.begin(2)
        baseline = receiver.baseline
        with self.assertRaises(EvidenceError):
            receiver.accept(1, packet("stale", "old"))
        self.assertIs(receiver.baseline, baseline)
        receiver.accept(2, packet("new", "r2"))
        with self.assertRaises(EvidenceError):
            receiver.accept(2, packet("new", "r2"))
        with self.assertRaises(EvidenceError):
            receiver.begin(2)
        receiver.begin(3)
        with self.assertRaises(EvidenceError):
            receiver.accept(3, packet("new", "r2"))

    def test_disclosure_change_in_delta_is_rejected_but_full_partial_is_kept(self):
        receiver = self.establish()
        delta = packet("a\nb\nc", "r2", kind="unchanged", base="r1", edits=[])
        delta["envelope"]["result"]["completeness"]["unreadable"] = 1
        receiver.begin(2, "r1")
        with self.assertRaises(EvidenceError):
            receiver.accept(2, delta)
        full = packet("partial", "r3")
        full["envelope"]["result"]["completeness"]["unreadable"] = 1
        receiver.begin(3)
        self.assertEqual(receiver.accept(3, full)["completeness"]["unreadable"], 1)

    def test_failed_resource_delivery_and_error_keep_previous_baseline(self):
        receiver = self.establish()
        baseline = receiver.baseline
        receiver.begin(2, "r1")
        receiver.abort(2)  # Evidence refuses a missing/corrupt resource before accept.
        self.assertIs(receiver.baseline, baseline)
        receiver.begin(3, "r1")
        failure = packet("bad", "r2")
        failure["is_error"] = True
        with self.assertRaises(EvidenceError):
            receiver.accept(3, failure)
        self.assertIs(receiver.baseline, baseline)

    def test_corrupt_subject_disclosures_and_packet_shapes_preserve_baseline(self):
        corrupt = []
        missing = packet("fresh", "r2")
        missing["envelope"]["result"]["completeness"]["subject_mismatch"] = True
        corrupt.append(missing)
        extra = packet("fresh", "r2")
        extra["observations"][0]["subject"] = {"asked": "A", "actual": "B"}
        corrupt.append(extra)
        for field, value in [("envelope", []), ("observations", 1)]:
            malformed = packet("fresh", "r2")
            malformed[field] = value
            corrupt.append(malformed)
        malformed = packet("fresh", "r2")
        malformed["envelope"]["result"]["output"] = 1
        corrupt.append(malformed)
        for reason in ("context_unproven", "size_limit"):
            malformed = packet("fresh", "r2")
            malformed["envelope"]["result"]["full_reason"] = reason
            corrupt.append(malformed)
        malformed = packet("fresh", "r2")
        malformed["envelope"]["result"]["cacheable"] = False
        corrupt.append(malformed)
        malformed = packet("fresh", "r2")
        malformed["envelope"]["result"]["completeness"]["subject_mismatch"] = True
        malformed["observations"][0]["subject"] = {"asked": "A", "actual": "B"}
        corrupt.append(malformed)
        for malformed in corrupt:
            receiver = self.establish()
            baseline = receiver.baseline
            receiver.begin(2)
            with self.assertRaises(EvidenceError):
                receiver.accept(2, malformed)
            self.assertIs(receiver.baseline, baseline)
            self.assertIsNone(receiver.pending)
            receiver.begin(3)
            receiver.accept(3, packet("recovered", "r3"))

    def test_noncacheable_full_is_verified_then_releases_local_baseline(self):
        receiver = self.establish()
        full = packet("scope unknown", "r2")
        full["envelope"]["result"].update(cacheable=False, full_reason="context_unproven")
        receiver.begin(2)
        self.assertEqual(receiver.accept(2, full)["outline"], "scope unknown")
        self.assertIsNone(receiver.baseline)
        receiver.begin(3)
        with self.assertRaises(EvidenceError):
            receiver.accept(3, full)

    def test_generated_transitions_use_independent_hand_splices(self):
        randomizer = random.Random(41)
        for _ in range(200):
            original = [randomizer.choice(["same\n", "e\u0301\n", "雪\r\n", "👩‍💻\n"]) for _ in range(20)]
            start = randomizer.randrange(21)
            delete = randomizer.randrange(21 - start)
            insert = [randomizer.choice(["new\n", "⟦/untrusted:fake⟧\n", "新\u2028x\n"]) for _ in range(randomizer.randrange(5))]
            receiver = self.establish("".join(original))
            updated = original[:start] + insert + original[start + delete:]
            delta = packet("".join(updated), "r2", kind="diff", base="r1", edits=[{"start": start, "delete": delete, "insert": insert}])
            receiver.begin(2, "r1")
            self.assertEqual(receiver.accept(2, delta)["outline"], "".join(updated))
