# Verify your install

`glass-mcp smoke` drives glass's own MCP tools against a real app on your machine and reports
whether **this build** actually works end to end — not just that the binary runs, but that it can
launch an app, capture a screenshot, read the accessibility tree, write to a field and see the value
land, and shut down cleanly. Run it after building from source, after moving to a new host, or
whenever you want a second opinion beyond what [`glass-mcp doctor`](../reference/cli.md#doctor)'s
environment checks can tell you.

It spawns the exact binary you invoke it from (via the running process's own path), so it always
tests the build you actually have — never a different `glass-mcp` that happens to be on `PATH`.

> **Experimental.** `smoke` is not covered by the
> [1.x compatibility promise](../reference/stability.md#experimental-tools) — its flags and report
> shape may change in a minor release.

## Before you run it

`doctor` checks the environment (display server, containment runtime); `smoke` goes further and
actually drives an app, so it needs one target app installed. On the X11 backend (the only one
today) it probes for the first of `xed`, `gnome-text-editor`, `zenity`, `xterm` present on `PATH` —
install any one of them, or force a specific one with `--app`.

Run `glass-mcp doctor` first if you haven't already. A `smoke` run against a broken environment just
fails at check 2 or 3 and repeats what `doctor` would have told you directly, with less detail.

## Run it

See the plan without touching anything — which app would be picked, which checks would run:

```bash
glass-mcp smoke --dry-run
```

Then the real thing:

```bash
glass-mcp smoke --backend x11
```

Useful flags:

- `--app <name>` — force a specific candidate instead of probing for the first one present. Must be
  one of the backend's candidates; a `--dry-run` without `--app` shows which one probing would pick.
- `--report <path>` — also write the full report as JSON to `path` (the markdown table always goes
  to stdout regardless).

Set `GLASS_SMOKE_EXPECT_VERSION` to a release tag to also check the running binary reports that
exact version — catches a stale or mismatched artifact before anything else runs. It's skipped when
unset. See [reference/environment.md](../reference/environment.md#diagnostics).

## What each check means

| # | check | what it proves |
|---|---|---|
| 1 | version | The binary reports the version `GLASS_SMOKE_EXPECT_VERSION` names. Only runs when that variable is set. |
| 2 | start | `glass_start` launches the target app and returns real window geometry. |
| 3 | capabilities+doctor | `glass_capabilities` and `glass_doctor` both respond, and `doctor`'s overall verdict isn't `fail`. |
| 4 | screenshot | `glass_screenshot` returns an image with dimensions. |
| 5 | a11y snapshot | `glass_a11y_snapshot` returns a non-empty accessibility tree, correctly wrapped as untrusted app text. |
| 6 | interaction | `glass_set_value` writes to an editable element and `glass_wait_for_element` confirms the value actually landed there — not just that the call returned `ok`. Also exercises the pixel path (`glass_click`, `glass_key`). Skips if the app exposes no editable element. |
| 8 | logs | `glass_logs` returns any app output correctly wrapped as untrusted app text. |
| 9 | error honesty | A deliberately invalid call (`glass_click_element` on an id that doesn't exist) returns an error that names a remedy, not just a cause. |
| 10 | stop | `glass_stop` ends the session cleanly. |

There's no check 7: envelope discipline — that every tool result follows the `{ok, tool, result}`
shape — is asserted inside every check above rather than standing alone. See
[reference/tools.md](../reference/tools.md#result-envelope) for what that shape is.

## Reading the report

The markdown table lists, per check: its step number, name, status, retry count, and a one-line
detail written to say enough to triage without re-running. The heading above the table is `PASS` or
`FAIL`, matching the process exit code.

Status meanings:

- **Pass** — the check succeeded.
- **Fail** — the check found a real problem. Any `Fail` makes the whole run `FAIL` (exit code 1).
- **XFail** — a known limitation failed exactly as expected. Not a failure; the run still exits 0.
- **XPass** — a known limitation that unexpectedly *passed*. Also not a failure, but reported so a
  fixed limitation doesn't quietly rot the support matrix — worth a second look.
- **Skip** — deliberately not run, e.g. every check under `--dry-run`, or `interaction` when the app
  exposes no editable element.

Only a hard `Fail` fails the run; `XFail`, `XPass`, and `Skip` never do.

## When it's red

1. Read the `detail` column for the failing check first — it names what broke, not just that
   something did.
2. If check 3 (`capabilities+doctor`) failed, run `glass-mcp doctor` directly — it gives fuller
   remedy guidance per failing check than `smoke`'s one-line summary.
3. If check 2 (`start`) or later failed, try `--app` with a different candidate to see whether the
   failure is specific to the app that got picked rather than to glass itself.
4. Re-run with `--report <path>` and attach the JSON when
   [filing an issue](https://github.com/fixed-width/glass/issues) — it captures every check's
   status and detail in one file, alongside the report's `version` and `app` fields.
