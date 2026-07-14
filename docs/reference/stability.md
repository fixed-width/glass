# Stability and versioning

glass follows [Semantic Versioning](https://semver.org). From **1.0.0** onward, the version number
tells you what a change can do to the surface your agent depends on:

- **Major** (`2.0.0`) — a breaking change to the covered surface below is allowed.
- **Minor** (`1.1.0`) — new tools, new optional parameters, new `result` fields, new `GLASS_*`
  variables. Additive only; nothing covered is removed or changed incompatibly.
- **Patch** (`1.0.1`) — fixes and internal changes with no surface change.

Every user-facing change is recorded in [`CHANGELOG.md`](../../CHANGELOG.md).

Before 1.0 (the `0.x` series), any release may change anything. This policy takes effect at 1.0.0.

## What is covered

Within a major version, these are stable — glass will not change them incompatibly:

- **Tool names** — the `glass_*` tool an agent calls.
- **Parameters** — each tool's parameter names and types, and which are required.
- **Result shapes** — the success envelope `{ "ok": true, "tool": "…", "result": { … } }` and the
  fields inside each tool's `result`. See [Tools](tools.md) for every tool's shape.
- **Accepted enum values** — the fixed value sets for parameters like `button`, `mode`, `direction`,
  `backend`, and `op`. An unrecognized value returns an error rather than being silently accepted.
- **The untrusted-marker convention** — text the target app controls (accessibility labels, log
  lines, clipboard contents, window titles) is delivered in its own content block wrapped in the
  untrusted marker, never inside `result`.
- **The `GLASS_*` environment surface** — the variable names and their meanings. See
  [Environment variables](environment.md).
- **Type conventions** — element ids are `u32`, window ids are `u64`, input coordinates are signed
  `i32` (window-relative), and region coordinates are unsigned `u32`.
- **Release-artifact names and layout** — the `glass-mcp-<tag>-<platform>.<ext>` assets on the
  [Releases](https://github.com/fixed-width/glass/releases) page, their per-platform suffixes, and the
  accompanying `.sha256` files. An installer or script can depend on the download-URL pattern. See
  [Platform support](platforms.md#release-artifacts) for the exact names.

## What is not covered

These may change in any release, including a patch, and are not a breaking change:

- **The exact wording of error and diagnostic messages.** The *behavior* is stable — an unknown enum
  value still returns an error, a failed capture still fails rather than returning a blank frame — but
  the message text may be reworded to be clearer (including for an agent reading it to self-correct).
- **Internal timing and performance** — poll intervals, retry counts, how long an operation takes.
- **Log-line formatting** — the text of glass's own diagnostic logging.

## Notes for agent authors

An MCP agent re-reads the tool list, descriptions, input schemas, and error messages at run time, so
it adapts to an additive change on its own — a new tool or a new optional parameter needs nothing from
you. Because an agent reads the *schema and descriptions* (not this changelog), glass signals a
deprecation where the agent will see it:

- A deprecated parameter or `result` field is marked `(deprecated: use X)` **in its description**, and
  keeps working — accepted on input, still emitted in output — for at least one minor release before a
  major removes it. glass will not remove a covered field silently, because an agent reading an absent
  field as null could misbehave with no error to catch.

If you pin behavior in a skill or prompt (for example, teaching an agent to read `result.matched`),
treat a **major** version bump as the point to revisit it, and watch for `(deprecated: …)` notes in
tool and parameter descriptions in the meantime.

## Experimental tools

A tool introduced after 1.0 may ship marked **experimental** in its description. An experimental tool
is **not** covered by the guarantee above: its name, parameters, and result shape may change, or the
tool may be removed, in any release — until it is promoted to stable in a later version. This lets a
new tool's shape settle against real use before it is locked.

All tools in the current release are **stable**; none is experimental.
