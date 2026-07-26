# `smoke` checks and report

What `glass-mcp smoke` asserts, and the shape of what it emits. For the flags, see
[CLI](cli.md#smoke); to run it and act on a red result, see
[how-to/check-a-build.md](../how-to/check-a-build.md).

**Experimental** — the checks, their numbering, and the report shape are not covered by the
[1.x compatibility promise](stability.md#experimental-subcommands) and may change in any minor
release.

## What it runs

`smoke` spawns the exact binary you invoke it from, via the running process's own path
(`std::env::current_exe()`), and drives it over stdio. It therefore always tests the build you
have in hand — never a different `glass-mcp` that happens to be earlier on `PATH`. The spawned
server is given `GLASS_BACKEND` set to the backend under test, overriding any ambient value.

## Target apps

A real run drives a stock app installed on the host. Each backend has a candidate list; the run
probes it in order and takes the first one runnable on `PATH`, recording the choice in the report's
`app` field. `--app <name>` selects a specific candidate instead.

| Backend | Candidates, in probe order |
|---|---|
| `x11` | `xed`, `gnome-text-editor`, `zenity`, `xterm` |
| `wayland` | `xed`, `gnome-text-editor`, `zenity` |

A wayland run launches its target with `GDK_BACKEND=wayland`, so a toolkit that cannot use the
Wayland backend fails the launch rather than falling back to Xwayland. `xterm` is an X11-only
client, which under the Wayland backend would reach the screen through Xwayland, so it is not a
wayland candidate.

## Checks

| # | Name | What it asserts |
|---|---|---|
| 1 | `start` | `glass_start` launches the target app and returns window geometry. |
| 2 | `capabilities+doctor` | `glass_capabilities` reports the backend under test as the active one, and `glass_doctor`'s `overall` verdict for that backend is not `fail`. An `overall` that is absent, or a value other than `ok`, `warn` or `fail`, also fails: there is no verdict to grade, which is not the same as "not `fail`". |
| 3 | `screenshot` | `glass_screenshot` returns an image block and its dimensions. |
| 4 | `a11y snapshot` | `glass_a11y_snapshot` returns a non-empty accessibility tree, delivered as untrusted app text. |
| 5 | `interaction` | `glass_set_value` writes to an editable element and `glass_wait_for_element` reports the written value on an element of that role. A match naming a different element id fails; a match naming no id is recorded as landed but unconfirmed. The pixel path (`glass_click`, `glass_key`) is exercised, then the window list is re-read, so an app dismissed by the pixel path is caught here. `skip` when check 4 read no tree, when the tree exposes no editable element, or when the element's role is one glass cannot address by name. |
| 6 | `logs` | `glass_logs` returns the app's output as untrusted app text, and the wrapped body parses as the `{"lines":[…]}` document `glass_logs` emits. The wrapper is required even when the app logged nothing. |
| 7 | `error honesty` | `glass_click_element` on a nonexistent id returns an error about that id which names a remedy, not only a cause. `skip` when the server answered about something else — no active session, or no snapshot yet — since that is not the error this check provokes. |
| 8 | `stop` | `glass_stop` ends the session cleanly. |

Envelope discipline — that every tool result follows the `{ok, tool, result}` shape described in
[Tools](tools.md#result-envelope) — is not a check of its own: it is asserted inside every check
above.

Under `--dry-run` none of this is asserted: every check is a `skip` whose `detail` says what it would
have done. `start`'s `detail` is where a missing target app is reported.

## Statuses

Every check reports one of five statuses. The text report and the JSON spell each one identically.

| Status | Glyph | Meaning | Effect on the run |
|---|---|---|---|
| `pass` | `✓` | The check succeeded. | None. |
| `fail` | `✗` | The check found a real problem. | Any `fail` makes the run `FAIL`, exit code 1. |
| `xfail` | `⚠` | A known limitation failed as expected. | None; the run still exits 0. |
| `xpass` | `⚠` | A check recorded as a known limitation passed. | None; reported so a limitation that has been fixed does not go unnoticed in the support matrix. |
| `skip` | `–` | The check was deliberately not run. `detail` says why. | None. |

The glyphs are [`glass-mcp doctor`](cli.md#doctor)'s. `xfail` and `xpass` share `⚠` — neither fails a
run, and both are a recorded limitation to look at — so the text report prints the status word
beside the glyph as well.

Only `fail` fails a run. A `skip` is not a `pass`: a check that skipped produced no evidence about
what it would have asserted.

## Report

Every run prints a text report to stdout, in the same shape as `glass-mcp doctor`'s. `--report
<path>` additionally writes the same run as JSON; the two carry the same facts, and neither is
derived from the other.

The JSON is written only once the run reaches its checks — a setup failure of a real
(non-`--dry-run`) run, such as no target app on `PATH`, reports on stderr and writes no file. A
`--dry-run` always reaches its (all-`skip`) checks, so it writes a report on any host, including one
with no candidate app installed.

### Text

```
glass smoke — <backend> — <verdict>
glass-mcp <version> · app: <app>

  <glyph> <status> <#> <check>: <detail>
  …

Summary: <n> ok, <n> warning(s), <n> failure(s), <n> skipped — <verdict>
```

One line per check, in the order the [Checks](#checks) table lists them, carrying the check's number,
its name and its `detail`. Newlines in any interpolated value are collapsed to spaces, so a check's
`detail` is always exactly one line and the summary can never be pushed out of view.

`<verdict>` appears twice — heading and summary — and is one of:

| Verdict | Meaning | Exit code |
|---|---|---|
| `PASS` | Every check ran that could, and none failed. | 0 |
| `PASS (plan only)` | `--dry-run`: nothing was spawned, launched or called, so nothing could fail. | 0 |
| `FAIL` | At least one check reported `fail`. | 1 |

The summary's counts follow the glyph mapping in [Statuses](#statuses): `pass` counts as ok, `xfail`
and `xpass` as warnings, `fail` as failures, and `skip` as skipped.

`<version>` is the version the server reported, or `(version not reported)` when it reported none.
`<app>` is the label of the app the run selected, or `none available` when it had none — the reason
and the remedy are in the `start` row's `detail`, not in this line.

An `xpass` row is followed by a continuation line, in doctor's `→` style, saying the check is
recorded as a known limitation but passed — the "reported" the `xpass` status promises.

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
