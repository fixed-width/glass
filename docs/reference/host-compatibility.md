# Host compatibility

Glass is a standard [Model Context Protocol](https://modelcontextprotocol.io) server, so any
spec-compliant MCP host can drive it. This page records what Glass needs from a host and which
host versions have been verified end to end. The table records prior observations, not a guarantee
for every later client release.

## What Glass needs from a host

A host can drive Glass if it:

- speaks MCP over **stdio** or **HTTP** (streamable) — Glass serves both;
- renders **image content blocks** — `glass_screenshot` always returns one, and tools like
  `glass_diff` return one only when asked (`include_image: true`);
- handles the selected [tool profile](tools.md#tool-profiles): 33 tools in full or 18 in lean;
- follows resource links with MCP `resources/read` when a response externalises oversized text,
  and preserves untrusted-content markers. Video resources return binary blobs.

The transport/image capabilities are exercised by the host-conformance tests
(`crates/glass-testapp/tests/host_conformance.rs`): on both transports they negotiate a protocol
version, list the tool set (compared for cross-transport parity), and return a decodable screenshot
image.

## Verified hosts

A host is *verified* when its own MCP client connects to Glass, lists the tools, and drives a real
loop — a tool call that returns text and one that returns an image — with the results crossing back.

| Host | stdio | HTTP | Basis |
|------|:-----:|:----:|-------|
| [Claude Code](https://docs.claude.com/en/docs/claude-code) | ✅ | ✅ | Driven in day-to-day development; generic transport/image regressions covered by host-conformance tests |
| [Antigravity](https://antigravity.google) | ✅ | ✅ | Driven end-to-end (v2.3.1) over both transports: the agent listed the Glass tools and a launch → screenshot → stop loop returned a screenshot |
| [Codex CLI](https://github.com/openai/codex) | ✅ | ✅ | Driven end-to-end (codex-cli 0.145.0) over both transports: the agent called `glass_start` → `glass_screenshot` → `glass_stop` and the screenshot returned an image |
| [GitHub Copilot](https://github.com/features/copilot) (VS Code, JetBrains) | ✅ | ✅ | Driven end-to-end via both the VS Code and JetBrains Copilot agents over both transports (`glass_start` → `glass_screenshot` → `glass_stop`, image returned) |

Registration for a host is in [Connect Glass to your agent](../how-to/connect-an-agent.md).

## Any other MCP host

Glass depends on nothing host-specific, so any host that meets the requirements above should work.
A host is listed here only after it has been verified against the standard — an unverified list
would not tell you anything you can rely on. If you drive Glass from a host that is not yet listed,
follow the [host-verification guide](../how-to/verify-an-mcp-host.md) and report the result.

## Verify a new host

Follow [Verify an MCP host](../how-to/verify-an-mcp-host.md) to check text, image and resource
handling and record the exact host version, transport and Glass tool profile.

For the tools themselves, see [Tools](tools.md). To have the agent drive Glass well from its first
turn, install the [glass-drive skill](../how-to/drive-glass-well.md).
