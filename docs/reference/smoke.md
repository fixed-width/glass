# `smoke` checks and report

What `glass-mcp smoke` asserts, and the shape of what it emits. For the flags, see
[CLI](cli.md#smoke); to run it and act on a red result, see
[how-to/verify-your-install.md](../how-to/verify-your-install.md).

**Experimental** — the checks, their numbering, and the report shape are not covered by the
[1.x compatibility promise](stability.md#experimental-subcommands) and may change in any minor
release.

## Checks

| # | Name | What it asserts |
|---|---|---|
| 1 | `version` | The binary reports the version `--expect-version` named. `skip` when the flag is absent. |
| 2 | `start` | `glass_start` launches the target app and returns window geometry. |
| 3 | `capabilities+doctor` | `glass_capabilities` reports the backend under test as the active one, and `glass_doctor`'s `overall` verdict for that backend is not `fail`. |
| 4 | `screenshot` | `glass_screenshot` returns an image block and its dimensions. |
| 5 | `a11y snapshot` | `glass_a11y_snapshot` returns a non-empty accessibility tree, delivered as untrusted app text. |
| 6 | `interaction` | `glass_set_value` writes to an editable element and `glass_wait_for_element` reports the written value on an element of that role. A match naming a different element id fails; a match naming no id is recorded as landed but unconfirmed. The pixel path (`glass_click`, `glass_key`) is exercised, then the window list is re-read, so an app dismissed by the pixel path is caught here. `skip` when check 5 read no tree, when the tree exposes no editable element, or when the element's role is one glass cannot address by name. |
| 8 | `logs` | `glass_logs` returns the app's output as untrusted app text. The wrapper is required even when the app logged nothing. |
| 9 | `error honesty` | `glass_click_element` on a nonexistent id returns an error about that id which names a remedy, not only a cause. `skip` when the server answered about something else — no active session, or no snapshot yet — since that is not the error this check provokes. |
| 10 | `stop` | `glass_stop` ends the session cleanly. |

There is no check 7. Envelope discipline — that every tool result follows the `{ok, tool, result}`
shape described in [Tools](tools.md#result-envelope) — is asserted inside every check above rather
than standing alone.

## Statuses

The markdown table and the JSON report spell each status identically.

| Status | Meaning | Effect on the run |
|---|---|---|
| `pass` | The check succeeded. | None. |
| `fail` | The check found a real problem. | Any `fail` makes the run `FAIL`, exit code 1. |
| `xfail` | A known limitation failed as expected. | None; the run still exits 0. |
| `xpass` | A check recorded as a known limitation passed. | None; reported so a limitation that has been fixed does not go unnoticed in the support matrix. |
| `skip` | The check was deliberately not run. `detail` says why. | None. |

Only `fail` fails a run. A `skip` is not a `pass`: a check that skipped produced no evidence about
what it would have asserted.

## Report

The markdown table goes to stdout on every run; `--report <path>` additionally writes the JSON. The
JSON is written only once the run reaches its checks — a setup failure, such as no target app on
`PATH`, reports on stderr and writes no file.

| Field | Description |
|---|---|
| `backend` | The backend exercised. |
| `version` | The version the server reported over MCP. |
| `app` | The candidate app selected, so reports from different hosts are comparable. |
| `checks[]` | One entry per check: `step`, `name`, `status`, and `detail`. |

`detail` is a single line stating what the check observed. In the markdown table a `|` in a detail
is escaped and newlines are collapsed, so an arbitrary error string cannot break the table.

The heading above the markdown table is `PASS` or `FAIL` and matches the process exit code.
