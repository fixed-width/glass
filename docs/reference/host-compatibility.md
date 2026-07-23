# Host compatibility

glass is a standard [Model Context Protocol](https://modelcontextprotocol.io) server, so any
spec-compliant MCP host can drive it. This page records what glass needs from a host and which
hosts have been verified end to end.

## What glass needs from a host

A host can drive glass if it:

- speaks MCP over **stdio** or **HTTP** (streamable) — glass serves both;
- renders **image content blocks** — `glass_screenshot` always returns one, and tools like
  `glass_diff` return one only when asked (`include_image: true`);
- handles glass's tool set (~30 tools).

These are exactly the capabilities the host-conformance tests exercise
(`crates/glass-testapp/tests/host_conformance.rs`): on both transports they negotiate a protocol
version, list the tool set (compared for cross-transport parity), and return a decodable screenshot
image.

## Verified hosts

A host is *verified* when its own MCP client connects to glass, lists the tools, and drives a real
loop — a tool call that returns text and one that returns an image — with the results crossing back.

| Host | stdio | HTTP | Basis |
|------|:-----:|:----:|-------|
| [Claude Code](https://docs.claude.com/en/docs/claude-code) | ✅ | ✅ | Driven in day-to-day development; covered by the host-conformance tests |
| Antigravity | ✅ | — | Driven end-to-end (v2.3.1): the agent listed the glass tools and a launch → screenshot → stop loop returned a screenshot |

Registration for a host is in [Connect glass to your agent](../how-to/connect-an-agent.md).

## Any other MCP host

glass depends on nothing host-specific, so any host that meets the requirements above should work.
A host is listed here only after it has been verified against the standard — an unverified list
would not tell you anything you can rely on. If you drive glass from a host that is not yet listed,
follow the recipe below and open an issue; we will add it.

## Verify a new host

To verify a host and get it added to the table:

1. Register glass with the host over stdio or HTTP — see
   [Connect glass to your agent](../how-to/connect-an-agent.md).
2. Confirm the agent lists the glass tools (it can see, e.g., `glass_start` and `glass_screenshot`).
3. Have the agent run a real loop: `glass_start` an app, `glass_screenshot` it (an image comes
   back), then `glass_stop`.
4. Open an issue noting the host and version, the transport(s) you used, and that the screenshot
   image came back.

For the tools themselves, see [Tools](tools.md). To have the agent drive glass well from its first
turn, install the [glass-drive skill](../how-to/drive-glass-well.md).
