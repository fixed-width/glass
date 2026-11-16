# Verify an MCP host

Use this checklist when connecting a client that is not in the
[host compatibility reference](../reference/host-compatibility.md), or checking a newer client build.
Set up a supported backend and a small app whose input/outcome you can recognise.

1. [Register Glass](connect-an-agent.md) over the transport you intend to use. Record the host
   name/version, `glass-mcp --version`, OS, transport and tool profile.
2. Confirm the client lists the selected tool inventory and makes a text-only call such as
   `glass_capabilities`. Full and lean expose different inventories.
3. Ask the agent to `glass_start` the app and `glass_screenshot` it. Confirm that the image is
   decoded and visible to the agent. A successful JSON-RPC response alone does not check rendering.
4. Drive a known input through `glass_do` and verify its expected app state with accessibility text,
   logs or a pixel predicate. Inspect errors before deciding whether any input can be repeated.
5. Check an oversized accessibility/log response when your workload produces one. The client must
   read linked `glass-artifact://` content through MCP `resources/read`, preserve its untrusted
   markers and avoid treating a server-local path as a client file. An unavailable artifact must
   not trigger blind replay of the action that produced it.
6. Stop the app with `glass_stop`. Confirm that app work ends. A resident macOS/Windows environment
   can remain ready for later app runs.

Repeat for each transport/profile combination you want to report. Preserve host-side diagnostic
output without credentials or sensitive app content, and report the tested combinations in a
[GitHub issue](https://github.com/fixed-width/glass/issues). Mark any untested resource/video support
explicitly; a launch/screenshot smoke check establishes only that smaller subset.

Contributors can run `scripts/test-x11.sh host_conformance` for Glass's generic stdio/HTTP regression
suite. That suite exercises the server protocol with a test client; it does not launch or qualify a
third-party host's own client.
