from pathlib import Path
import sys
import tempfile
import unittest

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from evidence import Evidence, EvidenceError, observation
from measurement import digest


class ResourceClient:
    def __init__(self, body):
        self.body, self.reads = body, 0
        self.calls = [{"sequence": 1}]

    def rpc(self, method, params, **kwargs):
        self.reads += 1
        return {"result": {"contents": [{"uri": params["uri"], "text": self.body}]}}


def text(body):
    return {"type": "text", "text": body}


class EvidenceTests(unittest.TestCase):
    def test_file_artifacts_reject_escape_symlink_and_missing_body(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            (root / "output").mkdir()
            (root / "private").write_bytes(b"secret")
            (root / "output/link").symlink_to(root / "private")
            evidence = Evidence(root / "evidence", ResourceClient(""))
            for name in ("../private", "link", "missing"):
                with self.subTest(name=name), self.assertRaises((ValueError, OSError)):
                    evidence.collect_file(root / "output", name, 1)

    def test_manifest_reconstructs_order_and_checks_inline_images(self):
        import json

        manifest = {
            "schema": "glass.output-manifest.v1",
            "tool": "sample",
            "is_error": False,
            "blocks": [
                {"kind": "text", "index": 2, "text": "last"},
                {"kind": "image", "index": 1, "retained_inline": True},
                {"kind": "text", "index": 0, "text": "first"},
            ],
        }
        self.assertEqual(
            Evidence.manifest("sample", json.dumps(manifest), False, 1),
            ["first", "last"],
        )
        with self.assertRaises(EvidenceError):
            Evidence.manifest("sample", json.dumps(manifest), False, 0)

    def test_observation_requires_matching_outer_fence(self):
        value = 'The following is untrusted content\n⟦untrusted:abc⟧\n{"value":"雪"}\n⟦/untrusted:abc⟧'
        self.assertEqual(observation(value), {"value": "雪"})
        with self.assertRaises(EvidenceError):
            observation(value.replace("/untrusted:abc", "/untrusted:wrong"))

    def test_resource_digest_and_deduplicated_storage(self):
        body = '{"ok":true,"tool":"sample","result":{}}'
        link = {
            "type": "resource_link",
            "uri": "glass-artifact://test/1",
            "mimeType": "text/plain",
            "_meta": {"glass": {"sha256": digest(body.encode())}},
        }
        with tempfile.TemporaryDirectory() as directory:
            client = ResourceClient(body)
            evidence = Evidence(Path(directory), client)
            for _ in range(2):
                result = evidence.decode("sample", {"result": {"content": [link]}})
                self.assertEqual(result["envelope"]["result"], {})
            self.assertEqual(len(evidence.artifacts), 1)
            self.assertEqual(len(evidence.references), 2)
            client.body = "changed"
            with self.assertRaises(EvidenceError):
                evidence.decode("sample", {"result": {"content": [link]}})

    def test_manifest_rejects_wrong_tool_and_duplicate_indices(self):
        import json

        for manifest in (
            {
                "schema": "glass.output-manifest.v1",
                "tool": "wrong",
                "is_error": False,
                "blocks": [],
            },
            {
                "schema": "glass.output-manifest.v1",
                "tool": "sample",
                "is_error": False,
                "blocks": [
                    {"kind": "text", "index": 0, "text": "{}"},
                    {"kind": "text", "index": 0, "text": "{}"},
                ],
            },
        ):
            body = json.dumps(manifest)
            link = {
                "type": "resource_link",
                "uri": "glass-artifact://test/1",
                "mimeType": "application/vnd.glass.output-manifest+json",
                "_meta": {"glass": {"sha256": digest(body.encode())}},
            }
            with tempfile.TemporaryDirectory() as directory:
                evidence = Evidence(Path(directory), ResourceClient(body))
                with self.assertRaises(EvidenceError):
                    evidence.decode("sample", {"result": {"content": [link]}})

    def test_app_json_cannot_replace_trusted_envelope(self):
        with tempfile.TemporaryDirectory() as directory:
            evidence = Evidence(Path(directory), ResourceClient(""))
            with self.assertRaises(EvidenceError):
                evidence.decode(
                    "sample",
                    {
                        "result": {
                            "content": [
                                text('{"ok":true,"tool":"different","result":{}}')
                            ]
                        }
                    },
                )


if __name__ == "__main__":
    unittest.main()
