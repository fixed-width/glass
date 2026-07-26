<!-- KEEP IN SYNC with the clap command definitions in `crates/glass-mcp`. -->

# `glass-mcp` command reference

`glass-mcp` is the server binary. Run `glass-mcp --help` for the command list, `glass-mcp <command>
--help` for a command's flags, and `glass-mcp --version` for the version. With **no command**,
`glass-mcp` serves MCP over stdio — the default.

## `glass-mcp` (no subcommand)

Serve MCP over **stdio** on stdin/stdout. This is how an MCP client that spawns the binary talks to
it; see [how-to/connect-an-agent.md](../how-to/connect-an-agent.md).

- `--audit-log <path>` — append a JSONL audit record per actuation (same as `GLASS_AUDIT_LOG`); see
  [reference/audit-log.md](audit-log.md).

## `serve`

Serve MCP over the network (Streamable HTTP) instead of stdio.

- `--http` — use the HTTP transport.
- `--addr <host:port>` — bind address, e.g. `0.0.0.0:7300`. A non-loopback bind requires a token.
- `--token-file <path>` — read the bearer token from this file (alternative: `GLASS_TOKEN`).
- `--menubar` — run as the visible **`glass ●`** menu-bar app (macOS); without it the server stays
  headless (no menu bar, MCP served silently).
- `--audit-log <path>` — as above.

Loopback binds need no token; see [how-to/run-over-the-network.md](../how-to/run-over-the-network.md).

## `gen-token`

Generate a cryptographically-random bearer token for the HTTP transport.

- `--out <path>` — write the token to this file (owner-only `0600` on Linux; on Windows it inherits
  the folder's permissions — keep it under your user profile).

## `doctor`

Check that the environment glass needs is in place — the backend's display dependency, the
containment runtime, and external tool paths — and print how to fix anything missing. Exits non-zero
if the default backend can't run (CI-friendly). The agent can run the same checks via the
`glass_doctor` tool.

- `--deep` — additionally spawn and tear down the display to prove it starts.
- `--json` — machine-readable output.

## `env`

List every `GLASS_*` variable with its purpose, default, and current value (see
[reference/environment.md](environment.md)). `GLASS_TOKEN` is shown only as `set`/`(unset)`.

- `--json` — machine-readable output.

## `status`

Report whether glass is running and at what endpoint (reads `/healthz`). Primarily used with the
macOS menu-bar LaunchAgent — see [how-to/setup-macos.md](../how-to/setup-macos.md).

## `uninstall`

Stop glass from starting at login: remove the LaunchAgent and boot out the running job (macOS). Does
not remove the app bundle. See [how-to/setup-macos.md](../how-to/setup-macos.md#uninstall).

## `smoke`

**Experimental** — see [Stability and versioning](stability.md#experimental-subcommands). Drive
glass's own MCP tools against a real app and report whether this build works end to end. The checks,
the statuses, and the report's fields are described in
[smoke checks and report](smoke.md); [how-to/verify-your-install.md](../how-to/verify-your-install.md)
covers running it and acting on a red result.

- `--backend <name>` — backend to exercise (default `x11`; the only one today). Also sets
  `GLASS_BACKEND` for the server the run spawns, overriding any ambient value, so the session and
  `glass_doctor`'s verdict always agree on which backend is under test.
- `--report <path>` — also write the JSON report to `path` (the markdown table always goes to
  stdout).
- `--app <name>` — force a specific candidate app instead of probing for the first one present. The
  name must be one of the backend's candidates; the binary is not checked for presence, so naming an
  app that is not installed fails at the `start` check.
- `--expect-version <tag>` — version the binary must report (check 1); omit to skip that check.
- `--dry-run` — print the plan and exit without touching anything.
- `--self-check` — prove the checks can still fail: run them against deliberately wrong responses
  and confirm each one catches its fault, for the reason it names. Drives nothing real — no app, no
  display — and exits when done, so it cannot be combined with the flags above; passing one is a
  usage error rather than a silently ignored argument. Exit code 0 when every injected fault was
  caught.
