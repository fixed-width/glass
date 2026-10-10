"""Lossless compact-v1 receiver. Reconstructed application text remains untrusted."""

import hashlib
import re

from evidence import EvidenceError


def lines(text):
    # splitlines() also recognizes CR and Unicode separators; the protocol recognizes LF only.
    parts = text.split("\n")
    return [part + "\n" for part in parts[:-1]] + ([parts[-1]] if parts[-1] else [])


def token(value):
    return isinstance(value, str) and 0 < len(value) <= 128 and value.isascii()


def integer(value):
    return type(value) is int and value >= 0


class SnapshotReceiver:
    def __init__(self, *, max_bytes=64 * 1024 * 1024):
        self.max_bytes = max_bytes
        self.baseline = None
        self.pending = None
        self.last_sequence = 0
        self.last_revision = None

    def begin(self, sequence, base_revision=None):
        if self.pending is not None or not integer(sequence) or sequence <= self.last_sequence:
            raise EvidenceError("revision request must follow a completed RPC")
        if base_revision is not None and not token(base_revision):
            raise EvidenceError("invalid requested revision")
        self.pending = (sequence, base_revision)
        self.last_sequence = sequence

    def abort(self, sequence):
        if self.pending is not None and self.pending[0] == sequence:
            self.pending = None

    def accept(self, sequence, decoded):
        if self.pending is None or self.pending[0] != sequence:
            raise EvidenceError("unsolicited, repeated or out-of-order revision response")
        requested_base = self.pending[1]
        try:
            candidate = self._validate(decoded, requested_base)
        except (AttributeError, KeyError, TypeError, ValueError, UnicodeError) as exc:
            raise EvidenceError("invalid revision packet; recover with a full observation") from exc
        finally:
            self.pending = None
        self.baseline = candidate if candidate["cacheable"] else None
        self.last_revision = candidate["revision"]
        return candidate

    def _validate(self, decoded, requested_base):
        if decoded["is_error"]:
            raise EvidenceError("revision read failed; recover with a full observation")
        envelope = decoded["envelope"]
        if envelope.get("tool") != "glass_a11y_snapshot_diff" or envelope.get("ok") is not True:
            raise EvidenceError("wrong revision tool envelope")
        result = envelope["result"]
        if "output" in result and result["output"].get("complete") is not True:
            raise EvidenceError("incomplete revision delivery")
        observations = decoded["observations"]
        if len(observations) != 1 or not isinstance(observations[0], dict):
            raise EvidenceError("revision requires one untrusted JSON payload")
        payload = observations[0]
        if type(result["version"]) is not int or result["version"] != 1 or result["outline_format"] != "compact-v1":
            raise EvidenceError("unsupported snapshot revision format")
        if not token(result["revision"]) or not token(result["context_id"]):
            raise EvidenceError("invalid revision/context token")
        if self.last_revision == result["revision"]:
            raise EvidenceError("repeated snapshot revision")
        if type(result["cacheable"]) is not bool:
            raise EvidenceError("invalid cache eligibility")
        for name in ("body_bytes", "line_count"):
            if not integer(result[name]):
                raise EvidenceError("invalid outline counts")
        if result["body_bytes"] > self.max_bytes:
            raise EvidenceError("outline exceeds receiver budget")
        sha = result["body_sha256"]
        if not isinstance(sha, str) or re.fullmatch("[0-9a-f]{64}", sha) is None:
            raise EvidenceError("invalid outline checksum")
        metadata = result["completeness"]
        for name in ("count", "unreadable", "unexposed"):
            if not integer(metadata[name]):
                raise EvidenceError("invalid completeness count")
        if type(metadata["subject_mismatch"]) is not bool:
            raise EvidenceError("invalid subject disclosure")
        if metadata["subject_mismatch"] != ("subject" in payload):
            raise EvidenceError("subject disclosure does not match payload")
        truncation = metadata["truncated"]
        if truncation is not None and (
            not isinstance(truncation, dict)
            or truncation.get("limit") not in ("nodes", "depth", "siblings")
            or not integer(truncation.get("limit_value"))
            or not integer(truncation.get("nodes_walked"))
        ):
            raise EvidenceError("invalid truncation disclosure")
        if "subject" in payload and (
            not isinstance(payload["subject"], dict)
            or set(payload["subject"]) != {"asked", "actual"}
            or any(not isinstance(payload["subject"].get(name), str) for name in ("asked", "actual"))
        ):
            raise EvidenceError("invalid untrusted subject")
        kind = result["kind"]
        if kind == "full":
            if "base_revision" in result or "edits" in payload or result["full_reason"] not in (
                "requested", "baseline_unavailable", "context_changed", "context_unproven",
                "metadata_changed", "size_limit", "not_smaller",
            ):
                raise EvidenceError("invalid full snapshot")
            if (
                result["cacheable"] != (result["full_reason"] not in ("context_unproven", "size_limit"))
                or (metadata["subject_mismatch"] and result["full_reason"] != "context_unproven")
            ):
                raise EvidenceError("ineligible full snapshot cannot become a baseline")
            outline = payload["outline"]
            if not isinstance(outline, str):
                raise EvidenceError("full outline must be text")
        elif kind in ("diff", "unchanged"):
            base = self.baseline
            if (
                base is None or requested_base != base["revision"]
                or result["base_revision"] != requested_base
                or result["context_id"] != base["context_id"]
                or result["cacheable"] is not True or "full_reason" in result
                or "outline" in payload or metadata != base["completeness"]
                or payload.get("subject") != base.get("subject")
            ):
                raise EvidenceError("revision baseline/context/disclosure mismatch")
            edits = payload["edits"]
            if not isinstance(edits, list) or (kind == "unchanged" and edits) or (kind == "diff" and not edits):
                raise EvidenceError("invalid revision edits")
            original = lines(base["outline"])
            previous_start, previous_end = -1, 0
            for edit in edits:
                if not isinstance(edit, dict) or set(edit) != {"start", "delete", "insert"}:
                    raise EvidenceError("invalid edit fields")
                start, delete, insert = edit["start"], edit["delete"], edit["insert"]
                if (
                    not integer(start) or not integer(delete) or start <= previous_start
                    or start < previous_end or start + delete > len(original)
                    or not isinstance(insert, list)
                    or any(not isinstance(line, str) or not line or "\n" in line[:-1] for line in insert)
                ):
                    raise EvidenceError("invalid edit order/range/line")
                previous_start, previous_end = start, start + delete
            updated = original.copy()
            for edit in reversed(edits):
                updated[edit["start"]:edit["start"] + edit["delete"]] = edit["insert"]
            if any(not line.endswith("\n") for line in updated[:-1]):
                raise EvidenceError("unterminated line before outline end")
            if sum(len(line.encode("utf-8")) for line in updated) > self.max_bytes:
                raise EvidenceError("reconstruction exceeds receiver budget")
            outline = "".join(updated)
        else:
            raise EvidenceError("unknown snapshot revision kind")
        raw = outline.encode("utf-8")
        if len(raw) != result["body_bytes"] or len(lines(outline)) != result["line_count"] or hashlib.sha256(raw).hexdigest() != sha:
            raise EvidenceError("reconstructed outline count/checksum mismatch")
        return dict(result, outline=outline, subject=payload.get("subject"))
