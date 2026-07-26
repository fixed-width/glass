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
> [1.x compatibility promise](../reference/stability.md#experimental-subcommands) — its flags and
> report shape may change in a minor release.

## Before you run it

`doctor` checks the environment (display server, containment runtime); `smoke` goes further and
actually drives an app, so it needs two things beyond what `doctor` requires:

- **A target app.** On the X11 backend (the only one today) it probes for the first of `xed`,
  `gnome-text-editor`, `zenity`, `xterm` present on `PATH` — install any one of them.
- **A running accessibility bus.** Checks 5 and 6 read and drive the accessibility tree, which on
  Linux means AT-SPI must be running (the `at-spi2-core` package on Debian/Ubuntu). Without it
  check 5 goes red with "accessibility unavailable" however healthy the rest of the install is.

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

- `--app <name>` — pick which of the backend's candidates to drive, instead of taking the first one
  probing finds. It selects among the candidates listed above; it does not make an app appear, and
  it does not check the binary is installed — naming one you don't have just fails at check 2. A
  `--dry-run` without `--app` shows which one probing would pick.
- `--report <path>` — also write the full report as JSON to `path` (the markdown table always goes
  to stdout regardless). Written only once the run reaches its checks: if setup fails first — no
  target app on `PATH`, say — the reason goes to stderr and no report file is created.
- `--expect-version <tag>` — also check the running binary reports exactly this version (a release
  tag) — catches a stale or mismatched artifact before anything else runs. Omit to skip that check.
- `--self-check` — prove the checks themselves can still fail, without driving anything. See
  [reference/cli.md](../reference/cli.md#smoke).

## What each check means

| # | check | what it proves |
|---|---|---|
| 1 | version | The binary reports the version `--expect-version` named. Skipped (not omitted) when the flag isn't given. |
| 2 | start | `glass_start` launches the target app and returns real window geometry. |
| 3 | capabilities+doctor | `glass_capabilities` reports the backend under test as the active one, and `glass_doctor`'s overall verdict for it isn't `fail`. |
| 4 | screenshot | `glass_screenshot` returns an image with dimensions. |
| 5 | a11y snapshot | `glass_a11y_snapshot` returns a non-empty accessibility tree, correctly wrapped as untrusted app text. |
| 6 | interaction | `glass_set_value` writes to an editable element and `glass_wait_for_element` confirms the written value shows up on an element of that role — not just that the call returned `ok`. Where the match reports an element id, a *different* id fails the check; when it reports none, the value is taken as landed but unconfirmed as to which element. Also exercises the pixel path (`glass_click`, `glass_key`) and then re-reads the window list, so a dismissed app is caught here rather than by later checks reporting on a dead session. Skips, with the reason in `detail`, when check 5 read no tree at all, when the tree exposes no editable element, or when the element's role is one glass cannot address by name. |
| 8 | logs | `glass_logs` returns the app's output wrapped as untrusted app text — the wrapper is required even when the app logged nothing. |
| 9 | error honesty | A deliberately invalid call (`glass_click_element` on an id that doesn't exist) returns an error about *that id* which names a remedy, not just a cause. Skips if the server answered about something else (no session, no snapshot yet) — that error would not be the one this check provokes. |
| 10 | stop | `glass_stop` ends the session cleanly. |

There's no check 7: envelope discipline — that every tool result follows the `{ok, tool, result}`
shape — is asserted inside every check above rather than standing alone. See
[reference/tools.md](../reference/tools.md#result-envelope) for what that shape is.

## Reading the report

The markdown table lists, per check: its step number, name, status, and a one-line detail written to
say enough to triage without re-running. The heading above the table is `PASS` or `FAIL`, matching
the process exit code. The markdown and the JSON spell each status the same way, so a status you
read in the table is the string to grep for in `--report`'s file:

- `pass` — the check succeeded.
- `fail` — the check found a real problem. Any `fail` makes the whole run `FAIL` (exit code 1).
- `xfail` — a known limitation failed exactly as expected. Not a failure; the run still exits 0.
- `xpass` — a known limitation that unexpectedly *passed*. Also not a failure, but reported so a
  fixed limitation doesn't quietly rot the support matrix — worth a second look.
- `skip` — deliberately not run: every check under `--dry-run`, `version` without
  `--expect-version`, or one of the `interaction` / `error honesty` cases above. The `detail` says
  which.

Only a hard `fail` fails the run; `xfail`, `xpass`, and `skip` never do. A `skip` is not a pass —
if a check you expected to exercise reports one, read its `detail` before treating the run as
evidence.

## When it's red

1. Read the `detail` column for the failing check first — it names what broke, not just that
   something did.
2. If check 3 (`capabilities+doctor`) failed, run `glass-mcp doctor` directly — it gives fuller
   remedy guidance per failing check than `smoke`'s one-line summary.
3. If check 5 (`a11y snapshot`) failed, the accessibility bus is the usual cause: `glass-mcp doctor`
   reports whether it's reachable and how to start it. On Linux that means AT-SPI — see
   [Before you run it](#before-you-run-it).
4. If check 2 (`start`) or later failed, try `--app` with a different candidate to see whether the
   failure is specific to the app that got picked rather than to glass itself.
5. Re-run with `--report <path>` and attach the JSON when
   [filing an issue](https://github.com/fixed-width/glass/issues) — it captures every check's
   status and detail in one file, alongside the report's `version` and `app` fields.
