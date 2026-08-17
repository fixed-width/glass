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

`update` refuses to run rather than guess in several cases:
- **A from-source build.** A binary built locally (its version carries a `git describe` suffix)
  is not something a release binary should silently replace — rebuild it with `git pull && cargo
  build --release` instead.
- **macOS.** glass installs there as `GlassMcp.app`, not a bare binary — download the latest `.dmg`
  instead (see [how-to/setup-macos.md](../how-to/setup-macos.md)).
- **An unsupported architecture**, where no release asset is published — build from source instead
  (see [how-to/build-from-source.md](../how-to/build-from-source.md)).
- **An install directory that is not writable** by the current user. `update` never escalates
  privileges to get around this (no `sudo` re-exec) — it prints the release URL so you can download
  and move the binary into place by hand.
- **A non-interactive run without `--yes`.** With no terminal to confirm on and no `--yes`, `update`
  declines rather than assuming consent.

If a `glass-mcp serve --http` process is already running, it keeps serving the previous build in
memory until it is restarted — `update` replaces the file on disk, not the running process.

## `uninstall`

Stop glass from starting at login: remove the LaunchAgent and boot out the running job (macOS). Does
not remove the app bundle. See [how-to/setup-macos.md](../how-to/setup-macos.md#uninstall).

