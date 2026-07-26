# Exercise a build with `smoke`

> **Setting glass up, or something isn't working?** Run
> [`glass-mcp doctor`](../reference/cli.md#doctor). It checks the environment glass needs — display
> server, containment runtime, tool paths — and prints how to fix whatever is missing. That is the
> command for "is my install healthy?", and unlike this one it is stable and needs nothing installed
> to answer.

`glass-mcp smoke` answers a narrower question: does **this build** drive a real app end to end? It
launches a stock app, captures a screenshot, reads the accessibility tree, writes to a field and
confirms the value landed, then shuts the session down — through glass's own MCP tools, over a real
MCP connection.

Two moments make it worth running — after building glass from source, and while changing glass
itself, when you want one command that exercises the whole loop rather than a suite that stubs it.
It drives [the binary you invoke it from](../reference/smoke.md#what-it-runs), so "this build" means
the one in your hand, not whichever `glass-mcp` is on `PATH`.

> **Experimental.** `smoke` is not covered by the
> [1.x compatibility promise](../reference/stability.md#experimental-subcommands) — its flags and
> report shape may change in a minor release.

## What it needs

`doctor` inspects the host; `smoke` drives an app on it, so a real run needs two things `doctor`
does not:

- **A target app.** The smoke runner drives x11, where it probes for the first of `xed`,
  `gnome-text-editor`, `zenity`, `xterm` present on `PATH` — install any one of them.
- **A running accessibility bus.** Checks 4 and 5 read and drive the accessibility tree, which on
  Linux means AT-SPI must be running (the `at-spi2-core` package on Debian/Ubuntu). Without it
  check 4 goes red however healthy the rest of the install is.

Neither is needed for `--dry-run` — see below.

Run `doctor` first if you haven't. Against a broken environment `smoke` just fails at check 1 or 2
and repeats, with less detail, what `doctor` would have told you outright.

## Run it

See the plan without driving anything — which app would be picked, which checks would run. This
works even before you've installed a target app: if none of the candidates is on `PATH` yet, the
plan's `start` row says what to install instead of the command failing, so it's safe as the very
first thing you run after building:

```bash
glass-mcp smoke --dry-run
```

A plan heads its report `PASS (plan only)`, not `PASS`. Nothing was exercised, so it is not evidence
that anything works — only that the run would be well-formed.

Then the real thing, which does need a target app installed:

```bash
glass-mcp smoke --backend x11
```

Two flags matter while you're working; [CLI](../reference/cli.md#smoke) lists them all.

- `--app <name>` — drive a specific candidate instead of the first one probing finds. It selects
  among the candidates above; it does not make an app appear. A real run naming one you don't have
  fails at check 1; add `--dry-run` and the plan tells you up front instead.
- `--report <path>` — also write the run as JSON, which is what to attach when filing an issue.

## Read the result

The report reads like `glass-mcp doctor`'s: a heading, one line per check, and a `Summary:` line.
Both the heading and the summary end in `PASS`, `PASS (plan only)` or `FAIL`, and the exit code is 0
for the first two and 1 for the last.

```
glass smoke — x11 — PASS
glass-mcp 1.1.0 · app: zenity

  ✓ pass  1 start: 432x142
  …

Summary: 8 ok, 0 warning(s), 0 failure(s), 0 skipped — PASS
```

Read the exit code as *no check failed* rather than as *everything works*: a run in which every
check skipped — any `--dry-run`, including one on a machine with no target app — also exits 0. Scan
for a `✗` when you want the short answer, and read the text after the colon on any line you care
about; it states what that check observed, or what it would have done.

Two things are worth knowing before you treat a green run as evidence: only `fail` fails a run, and
a `skip` is not a `pass`. If a check you expected to exercise reports `skip`, read the rest of its
line before concluding anything. [smoke checks and report](../reference/smoke.md) has the full table
of what each check asserts and what each status means.

## When it's red

1. Read the failing check's line first — it names what broke, not just that something did.
2. If check 2 (`capabilities+doctor`) failed, run `glass-mcp doctor` directly. It gives fuller
   per-check remedy guidance than `smoke`'s one-line summary.
3. If check 4 (`a11y snapshot`) failed, the accessibility bus is the usual cause — `doctor` reports
   whether it's reachable and how to start it. See [What it needs](#what-it-needs).
4. If neither of those applies, try `--app` with a different candidate you have installed, to see
   whether the failure is specific to the app that got picked rather than to glass itself.
   `glass-mcp smoke --dry-run --app <name>` confirms the one you chose is actually runnable before
   you spend a full run on it.
5. Re-run with `--report <path>` and attach the JSON when
   [filing an issue](https://github.com/fixed-width/glass/issues) — it carries every check's status
   and detail, alongside the backend, version and app the run used. If the run never reached a
   check — no target app, say — it explains itself on stderr and writes no file; paste that message
   instead.
