"""Validate MCP envelopes and preserve externalized observations before shutdown."""

import json
from pathlib import Path
import re

from measurement import digest, owned_path


class EvidenceError(RuntimeError):
    pass


def observation(text):
    match = re.fullmatch(
        r"The following is untrusted content[^\n]*\n⟦untrusted:([^⟧]+)⟧\n(.*)\n⟦/untrusted:\1⟧",
        text,
        re.DOTALL,
    )
    if not match:
        raise EvidenceError("invalid observation fence")
    body = match.group(2)
    try:
        return json.loads(body)
    except ValueError:
        return body


class Evidence:
    def __init__(self, directory, client, *, limit=512 * 1024 * 1024, timeout=30):
        self.directory, self.client = Path(directory), client
        self.directory.mkdir(parents=True, exist_ok=True)
        self.limit, self.timeout = limit, timeout
        self.artifacts, self.references = {}, []
        self.stored_bytes = 0

    def resource(self, link, origin):
        uri = link.get("uri")
        if not isinstance(uri, str) or not uri.startswith("glass-artifact://"):
            raise EvidenceError("unexpected artifact URI")
        reply = self.client.rpc("resources/read", {"uri": uri}, timeout=self.timeout)
        contents = reply.get("result", {}).get("contents", [])
        if (
            "error" in reply
            or len(contents) != 1
            or not isinstance(contents[0].get("text"), str)
        ):
            raise EvidenceError("resource did not return one text body")
        if contents[0].get("uri") != uri:
            raise EvidenceError("resource URI mismatch")
        body = contents[0]["text"]
        raw = body.encode("utf-8")
        sha = digest(raw)
        if link.get("_meta", {}).get("glass", {}).get("sha256") != sha:
            raise EvidenceError("resource digest mismatch")
        self.archive(raw, uri, link.get("mimeType"), origin)
        return body

    def archive(self, raw, source, mime, origin):
        sha = digest(raw)
        if sha not in self.artifacts:
            if self.stored_bytes + len(raw) > self.limit:
                raise EvidenceError("artifact storage limit exceeded")
            name = f"artifact-{sha}.bin"
            (self.directory / name).write_bytes(raw)
            self.artifacts[sha] = {
                "path": name,
                "sha256": sha,
                "bytes": len(raw),
                "mime_type": mime,
            }
            self.stored_bytes += len(raw)
        self.references.append({"source": source, "origin_call": origin, "sha256": sha})
        return self.artifacts[sha]

    def collect_file(self, root, relative, origin):
        path = owned_path(root, relative)
        if path.stat().st_size > self.limit - self.stored_bytes:
            raise EvidenceError("file artifact exceeds storage limit")
        with path.open("rb") as stream:
            raw = stream.read(self.limit - self.stored_bytes + 1)
        if len(raw) > self.limit - self.stored_bytes:
            raise EvidenceError("file artifact grew beyond storage limit")
        return self.archive(raw, str(relative), None, origin), raw

    @staticmethod
    def manifest(tool, body, is_error, images):
        try:
            value = json.loads(body)
        except ValueError as exc:
            raise EvidenceError("invalid output manifest JSON") from exc
        if (
            not isinstance(value, dict)
            or value.get("schema") != "glass.output-manifest.v1"
        ):
            raise EvidenceError("unknown output manifest")
        if (
            value.get("tool") != tool
            or type(value.get("is_error")) is not bool
            or value["is_error"] != is_error
        ):
            raise EvidenceError("output manifest tool/error mismatch")
        blocks = value.get("blocks")
        if not isinstance(blocks, list):
            raise EvidenceError("manifest blocks missing")
        indices = [b.get("index") for b in blocks]
        if any(type(i) is not int for i in indices) or sorted(indices) != list(
            range(len(blocks))
        ):
            raise EvidenceError("manifest indices are not contiguous and unique")
        texts, retained_images = [], 0
        for block in sorted(blocks, key=lambda b: b["index"]):
            if block.get("kind") == "text" and isinstance(block.get("text"), str):
                texts.append(block["text"])
            elif block.get("kind") == "image" and block.get("retained_inline") is True:
                retained_images += 1
            else:
                raise EvidenceError("invalid manifest block")
        if retained_images != images:
            raise EvidenceError("manifest image count mismatch")
        return texts

    def decode(self, tool, response):
        if "error" in response:
            raise EvidenceError(f"JSON-RPC error: {response['error']}")
        result = response.get("result", {})
        is_error = result.get("isError", False)
        if type(is_error) is not bool or not isinstance(result.get("content"), list):
            raise EvidenceError("invalid MCP tool result")
        blocks = result["content"]
        images = sum(b.get("type") == "image" for b in blocks)
        origin = self.client.calls[-1]["sequence"]
        texts, manifest, links = [], None, 0
        for block in blocks:
            if block.get("type") == "text":
                texts.append(block["text"])
            elif block.get("type") == "resource_link":
                body = self.resource(block, origin)
                if block.get("mimeType", "").startswith(
                    "application/vnd.glass.output-manifest+json"
                ):
                    if manifest is not None:
                        raise EvidenceError("multiple output manifests")
                    manifest = self.manifest(tool, body, is_error, images)
                else:
                    links += 1
                    texts.append(body)
        if manifest is not None:
            if links:
                raise EvidenceError("manifest mixed with content links")
            texts = manifest
        if not texts:
            raise EvidenceError("missing tool envelope")
        try:
            envelope = json.loads(texts[0])
        except ValueError:
            if is_error:
                return {
                    "envelope": None,
                    "observations": [],
                    "texts": texts,
                    "is_error": True,
                }
            raise EvidenceError("invalid tool envelope JSON")
        if (
            not isinstance(envelope, dict)
            or envelope.get("tool") != tool
            or envelope.get("ok") is not (not is_error)
        ):
            raise EvidenceError("tool envelope/error mismatch")
        delivery = envelope.get("result", {}).get("output")
        if delivery is not None and delivery.get("complete") is not True:
            raise EvidenceError("incomplete output delivery")
        observations = [
            observation(body)
            for body in texts[1:]
            if body.startswith("The following is untrusted content")
        ]
        return {
            "envelope": envelope,
            "observations": observations,
            "texts": texts,
            "is_error": is_error,
        }
