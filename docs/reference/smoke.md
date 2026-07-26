# `smoke` checks and report

What `glass-mcp smoke` asserts, and the shape of what it emits. For the flags, see
[CLI](cli.md#smoke); to run it and act on a red result, see
[how-to/verify-your-install.md](../how-to/verify-your-install.md).

**Experimental** — the checks, their numbering, and the report shape are not covered by the
[1.x compatibility promise](stability.md#experimental-subcommands) and may change in any minor
release.

## What it runs

`smoke` spawns the exact binary you invoke it from, via the running process's own path
(`std::env::current_exe()`), and drives it over stdio. It therefore always tests the build you
have in hand — never a different `glass-mcp` that happens to be earlier on `PATH`. The spawned
server is given `GLASS_BACKEND` set to the backend under test, overriding any ambient value.

## Checks

| # | Name | What it asserts |
|---|---|---|
| 2 | `start` | `glass_start` launches the target app and returns window geometry. |
| 3 | `capabilities+doctor` | `glass_capabilities` reports the backend under test as the active one, and `glass_doctor`'s `overall` verdict for that backend is not `fail`. An `overall` that is absent, or a value other than `ok`, `warn` or `fail`, also fails: there is no verdict to grade, which is not the same as "not `fail`". |
| 4 | `screenshot` | `glass_screenshot` returns an image block and its dimensions. |
| 5 | `a11y snapshot` | `glass_a11y_snapshot` returns a non-empty accessibility tree, delivered as untrusted app text. |
| 6 | `interaction` | `glass_set_value` writes to an editable element and `glass_wait_for_element` reports the written value on an element of that role. A match naming a different element id fails; a match naming no id is recorded as landed but unconfirmed. The pixel path (`glass_click`, `glass_key`) is exercised, then the window list is re-read, so an app dismissed by the pixel path is caught here. `skip` when check 5 read no tree, when the tree exposes no editable element, or when the element's role is one glass cannot address by name. |
| 8 | `logs` | `glass_logs` returns the app's output as untrusted app text, and the wrapped body parses as the `{"lines":[…]}` document `glass_logs` emits. The wrapper is required even when the app logged nothing. |
| 9 | `error honesty` | `glass_click_element` on a nonexistent id returns an error about that id which names a remedy, not only a cause. `skip` when the server answered about something else — no active session, or no snapshot yet — since that is not the error this check provokes. |
| 10 | `stop` | `glass_stop` ends the session cleanly. |

There is no check 7. Envelope discipline — that every tool result follows the `{ok, tool, result}`
shape described in [Tools](tools.md#result-envelope) — is asserted inside every check above rather
than standing alone.

Under `--dry-run` none of this is asserted: every check is a `skip` whose `detail` says what it would
have done. `start`'s `detail` is where a missing target app is reported.

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

Every run prints the markdown report to stdout; `--report <path>` additionally writes the same run
as JSON. The JSON is written only once the run reaches its checks — a setup failure of a real
(non-`--dry-run`) run, such as no target app on `PATH`, reports on stderr and writes no file. A
`--dry-run` always reaches its (all-`skip`) checks, so it writes a report on any host, including one
with no candidate app installed.

### Markdown

```
# glass smoke — <backend> — <verdict>

glass-mcp <version> · app: `<app>`

| # | check | status | detail |
```

The heading's `<verdict>` is one of:

| Verdict | Meaning | Exit code |
|---|---|---|
| `PASS` | Every check ran that could, and none failed. | 0 |
| `PASS (plan only)` | `--dry-run`: nothing was spawned, launched or called, so nothing could fail. | 0 |
| `FAIL` | At least one check reported `fail`. | 1 |

`<app>` is the label of the app the run selected, or `none available` when it had none — the reason and
the remedy are in the `start` row's `detail`, not in this line. A `|` in any of these values is
escaped and newlines are collapsed, so no value can break the table.

When any check is `xpass`, a `## Stale limitations` section follows the table, naming each such check
— the "reported" the `xpass` status promises.

### JSON

| Field | Description |
|---|---|
| `backend` | The backend exercised. |
| `version` | The version the server reported over MCP, or `null` if it reported none. Under `--dry-run` no server is spawned, so it is the version compiled into the binary you invoked. |
| `mode` | `"full"` for a run that drove the checks, `"dry_run"` for a plan. |
| `app.state` | `"selected"` when the run had a target app, `"unavailable"` when it had none. Only a `--dry-run` report can be `"unavailable"`. |
| `app.value` | The candidate's label when `selected`, so reports from different hosts are comparable; when `unavailable`, the same note the `start` row carries. |
| `checks[]` | One entry per check: `step`, `name`, `status`, and `detail`. |

`detail` is a single line stating what the check observed — or, under `--dry-run`, what it would
have done.
