from pathlib import Path
import sys
import tempfile
import time
import unittest
from unittest.mock import patch

sys.path.insert(0, str(Path(__file__).resolve().parents[1]))
from protocol import Client, ProtocolError


FAKE = r"""
import json, os, sys, time
mode = sys.argv[1]
for line in sys.stdin.buffer:
    r = json.loads(line)
    if 'id' not in r: continue
    if mode == 'hang': time.sleep(30)
    if mode == 'exit': sys.exit(2)
    if mode == 'invalid': os.write(1, b'\xff\n'); continue
    if mode == 'large': os.write(1, b'x' * 4096 + b'\n'); continue
    if mode == 'stderr': os.write(2, b'x' * 4096)
    if mode == 'null': os.write(1, json.dumps({'jsonrpc':'2.0','id':r['id'],'result':None}).encode() + b'\n'); continue
    os.write(1, b'{"jsonrpc":"2.0","method":"notifications/test"}\n')
    reply = {'jsonrpc':'2.0', 'id':r['id'], 'result': {'content':[{'type':'text','text':'雪'}]}}
    raw = json.dumps(reply, ensure_ascii=False).encode() + b'\n'
    for part in (raw[:-3], raw[-3:]): os.write(1, part)
"""


class ProtocolTests(unittest.TestCase):
    def client(self, directory, mode="ok", **kwargs):
        return Client(
            [sys.executable, "-c", FAKE, mode], Path(directory), timeout=1, **kwargs
        )

    def test_chunked_utf8_notifications_and_exact_bytes(self):
        with tempfile.TemporaryDirectory() as directory:
            client = self.client(directory)
            try:
                response = client.rpc("tools/call", {"name": "sample", "arguments": {}})
                self.assertEqual(response["result"]["content"][0]["text"], "雪")
                call = client.calls[0]
                self.assertEqual(call["response_text_bytes"], 3)
                self.assertEqual(
                    call["response_wire_bytes"],
                    (Path(directory) / call["response_file"]).stat().st_size,
                )
                self.assertIn(
                    b"notifications/test", (Path(directory) / "stdout.bin").read_bytes()
                )
            finally:
                self.assertTrue(client.close())

    def test_lost_mutation_reply_is_never_retried(self):
        with tempfile.TemporaryDirectory() as directory:
            client = self.client(directory, "hang")
            before = time.monotonic()
            try:
                with self.assertRaises(ProtocolError):
                    client.rpc("tools/call", {"name": "mutate"}, timeout=0.1)
                self.assertEqual(len(client.calls), 1)
                self.assertEqual(client.calls[0]["status"], "timeout")
            finally:
                client.close(grace=0.1)
            self.assertLess(time.monotonic() - before, 3)

    def test_malformed_or_oversized_output_and_early_exit_fail(self):
        for mode in ("invalid", "large", "exit", "null"):
            with self.subTest(mode=mode), tempfile.TemporaryDirectory() as directory:
                client = self.client(
                    directory, mode, frame_limit=1024, evidence_limit=2048
                )
                try:
                    with self.assertRaises(ProtocolError):
                        client.rpc("tools/call", {"name": "sample"})
                finally:
                    client.close(grace=0.1)

    def test_stderr_budget_violation_cannot_finish_healthy(self):
        with tempfile.TemporaryDirectory() as directory:
            client = self.client(directory, "stderr", evidence_limit=2048)
            try:
                client.rpc("tools/call", {"name": "sample"})
            except ProtocolError:
                pass
            self.assertFalse(client.close(grace=0.1))
            self.assertIn("limit", client.fault)
            self.assertLessEqual((Path(directory) / "stderr.bin").stat().st_size, 2048)

    def test_cancellation_preserves_one_attempt_and_closes_child(self):
        with tempfile.TemporaryDirectory() as directory:
            client = self.client(directory, "hang")
            with patch.object(client.messages, "get", side_effect=KeyboardInterrupt):
                with self.assertRaises(KeyboardInterrupt):
                    client.rpc("tools/call", {"name": "mutate"})
            self.assertEqual([c["status"] for c in client.calls], ["interrupted"])
            self.assertTrue(client.poisoned)
            client.close(grace=0.1)
            self.assertIsNotNone(client.process.poll())

    def test_inventory_pagination_and_repeated_cursor(self):
        for repeated in (False, True):
            with (
                self.subTest(repeated=repeated),
                tempfile.TemporaryDirectory() as directory,
            ):
                child = """import json, sys
for line in sys.stdin:
 r = json.loads(line)
 if 'id' not in r: continue
 if r['method'] == 'initialize': value = {'protocolVersion':'2024-11-05','serverInfo':{'name':'fake'}}
 elif r['params'].get('cursor') and not REPEATED: value = {'tools':[{'name':'second','inputSchema':{}}]}
 else: value = {'tools':[{'name':'first','inputSchema':{}}], 'nextCursor':'next'}
 print(json.dumps({'jsonrpc':'2.0','id':r['id'],'result':value}), flush=True)
""".replace("REPEATED", repr(repeated))
                client = Client(
                    [sys.executable, "-c", child], Path(directory), timeout=1
                )
                try:
                    if repeated:
                        with self.assertRaises(ProtocolError):
                            client.initialize()
                    else:
                        _, inventory = client.initialize()
                        self.assertEqual(
                            [t["name"] for t in inventory], ["first", "second"]
                        )
                finally:
                    client.close(grace=0.1)


if __name__ == "__main__":
    unittest.main()
