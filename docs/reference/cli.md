<!-- KEEP IN SYNC with the clap command definitions in `crates/glass-mcp`. -->

# `glass-mcp` command reference

`glass-mcp` is the server binary. Run `glass-mcp --help` for the command list, `glass-mcp <command>
--help` for a command's flags, and `glass-mcp --version` for the version. With **no command**,
`glass-mcp` serves MCP over stdio — the default.

## `glass-mcp` (no subcommand)

Serve MCP over **stdio** on stdin/stdout. This is how an MCP client that spawns the binary talks to
it; see [how-to/connect-an-agent.md](../how-to/connect-an-agent.md).

Tool responses can contain `glass-artifact://` links for bounded text output. Read them with MCP
`resources/read`. A local path in resource metadata is on the server host, not necessarily the MCP
client's filesystem.

- `--audit-log <path>` — append a JSONL audit record per actuation (same as `GLASS_AUDIT_LOG`); see
  [reference/audit-log.md](audit-log.md).
- `--tool-profile full|lean` — fixed tool inventory for this server, default `full`.
  Lean uses `glass_do` for actions; see [tool profiles](tools.md#tool-profiles).

## `serve`

Serve MCP over the network (Streamable HTTP) instead of stdio.

- `--http` — use the HTTP transport.
- `--addr <host:port>` — bind address, e.g. `0.0.0.0:7300`. A non-loopback bind requires a token.
- `--token-file <path>` — read the bearer token from this file (alternative: `GLASS_TOKEN`).
- `--menubar` — run as the visible **`glass ●`** menu-bar app (macOS); without it the server stays
  headless (no menu bar, MCP served silently).
- `--audit-log <path>` — as above.
- `--tool-profile full|lean` — as above, including menu-bar mode.

Loopback binds need no token; see [how-to/run-over-the-network.md](../how-to/run-over-the-network.md).
Remote HTTP clients must read `glass-artifact://` links with MCP `resources/read`; they must not try
to open the server-local path from resource metadata on the client machine.

## `tools`

List the selected profile's MCP tools and schema cost without starting a backend/session or opening
the audit sink or artifact store. The global `--tool-profile` flag selects full (default) or lean.

- `--json` — return `profile`, `tools`, `instructions`, `tools_json_bytes`, `instructions_bytes`,
  `total_bytes`, and `per_tool` (name and serialized bytes). Definitions and instructions match the
  live MCP server for that profile.

Byte counts use compact UTF-8 JSON for the tool array and UTF-8 text for instructions, excluding
JSON-RPC framing. They are not token counts. This command inspects schemas; actions still run over MCP.

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
- `--color <when>` — `auto` (default; color only when writing to a terminal), `always`, or `never`.
  Ignored with `--json`. A non-empty `NO_COLOR`, or `TERM=dumb`, suppresses `auto`.

## `env`

List every `GLASS_*` variable with its purpose, default, and current value (see
[reference/environment.md](environment.md)). `GLASS_TOKEN` is shown only as `set`/`(unset)`.

- `--json` — machine-readable output.
- `--color <when>` — `auto` (default; color only when writing to a terminal), `always`, or `never`.
  Ignored with `--json`. A non-empty `NO_COLOR`, or `TERM=dumb`, suppresses `auto`.

## `status`

Report whether glass is running and at what endpoint (reads `/healthz`). Primarily used with the
macOS menu-bar LaunchAgent — see [how-to/setup-macos.md](../how-to/setup-macos.md).

## `update`

Update this binary to the latest published release: resolve the latest tag, download the asset for
this platform, verify its checksum, and replace this binary in place. Verifies build provenance with
the GitHub CLI (`gh attestation verify`) when it is installed; if `gh` is not on `PATH` the update
still proceeds, but says so.

- `--check` — report the current and latest version without changing anything. Refuses nothing:
  unlike a real update, `--check` works on every platform, including macOS and a from-source build.
- `--yes` — skip the confirmation prompt. Does **not** skip any verification (checksum, provenance,
  or the post-download smoke check) — pair it with `--skip-attestation` explicitly if that is also
  wanted.
- `--skip-attestation` — proceed even if `gh attestation verify` fails.
- `--json` — machine-readable output.
- `--color <when>` — `auto` (default; color only when writing to a terminal), `always`, or `never`.
  Ignored with `--json`. A non-empty `NO_COLOR`, or `TERM=dumb`, suppresses `auto`.

Exit code: `0` for a successful update, for "already up to date", and for every `--check` run
(whether or not a newer release exists) — a script should read `--json`'s `update_available` field,
not the exit code, to learn whether news exists. `1` for a refusal or any other error.

`--json` prints exactly one object for every outcome of the update itself — including a refusal,
and including a failure to reach or resolve the release. (The exception is a failure before the
update starts at all, such as `glass-mcp` being unable to resolve its own path; that reports as a
plain error message with no JSON.) Its fields:

| Field | Meaning |
|---|---|
| `action` | `checked`, `updated`, `refused`, or `error` (the release could not be resolved, downloaded, or installed at all) |
| `current` | the version of the binary that is running |
| `latest` | the latest published release, or `null` if it was never resolved |
| `update_available` | `true` only when `latest` is known **and** strictly newer than `current` |
| `current_comparable` | `false` when `current` is not a released version (a from-source build), so `update_available: false` means "unknown", not "up to date" |
| `supported` | whether this platform is one `update` can replace in place at all |
| `asset` · `url` | the release asset chosen for this platform, and where it lives — `null` before they are resolved |
| `install_path` | the binary that would be, or was, replaced |
| `attestation` | `not_checked`, `verified`, `unavailable` (no `gh` on `PATH`), `skipped` (`--skip-attestation`), or `failed` |
| `running_server` | whether a `glass-mcp serve --http` is answering on `127.0.0.1:7300` and therefore still running the previous build |
| `reason` | present only for `refused` and `error`: the same text the human output prints |

`update` refuses to run rather than guess in several cases:
- **A from-source build.** A binary built locally (its version carries a `git describe` suffix)
  is not something a release binary should silently replace — rebuild it with `git pull && cargo
  build --release` instead.
- **macOS.** glass installs there as `GlassMcp.app`, not a bare binary — download the latest `.dmg`
  instead (see [how-to/setup-macos.md](../how-to/setup-macos.md)).
- **An unsupported architecture**, where no release asset is published — build from source instead
  (see [how-to/build-from-source.md](../how-to/build-from-source.md)).
- **An install directory it cannot stage the download in.** Before downloading anything, `update`
  creates the temporary file it is about to download into, beside the installed binary. If that
  fails it reports the operating system's own error — a read-only directory is the usual reason,
  but a full disk or a quota lands here too — and stops. It never escalates privileges to get
  around this (no `sudo` re-exec); it prints the release URL so you can download and move the
  binary into place by hand.
- **A non-interactive run without `--yes`.** With no terminal to confirm on and no `--yes`, `update`
  declines rather than assuming consent.

If a `glass-mcp serve --http` process is already running, it keeps serving the previous build in
memory until it is restarted — `update` replaces the file on disk, not the running process.

## `uninstall`

Stop glass from starting at login: remove the LaunchAgent and boot out the running job (macOS). Does
not remove the app bundle. See [how-to/setup-macos.md](../how-to/setup-macos.md#uninstall).
