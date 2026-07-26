# Verify your install

`glass-mcp smoke` drives glass's own MCP tools against a real app on your machine and reports
whether **this build** actually works end to end — not just that the binary runs, but that it can
launch an app, capture a screenshot, read the accessibility tree, write to a field and see the value
land, and shut down cleanly. Run it after building from source, after moving to a new host, or
whenever you want a second opinion beyond what [`glass-mcp doctor`](../reference/cli.md#doctor)'s
environment checks can tell you.

> **Experimental.** `smoke` is not covered by the
> [1.x compatibility promise](../reference/stability.md#experimental-subcommands) — its flags and
> report shape may change in a minor release.

## Before you run it

`doctor` checks the environment; `smoke` goes further and actually drives an app, so it needs two
things beyond what `doctor` requires:

- **A target app.** On the X11 backend (the only one today) it probes for the first of `xed`,
  `gnome-text-editor`, `zenity`, `xterm` present on `PATH` — install any one of them.
- **A running accessibility bus.** Checks 5 and 6 read and drive the accessibility tree, which on
  Linux means AT-SPI must be running (the `at-spi2-core` package on Debian/Ubuntu). Without it
  check 5 goes red however healthy the rest of the install is.

If you haven't run `glass-mcp doctor` yet, run it first. A `smoke` run against a broken environment
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

Three flags matter while you're working; [CLI](../reference/cli.md#smoke) lists them all.

- `--app <name>` — drive a specific candidate instead of the first one probing finds. It selects
  among the candidates above; it does not make an app appear, and it does not check the binary is
  installed, so naming one you don't have just fails at check 2.
- `--report <path>` — also write the run as JSON, which is what to attach when filing an issue.
- `--expect-version <tag>` — require the binary to report exactly this release tag, catching a stale
  or mismatched artifact before anything else runs.

## Read the result

The heading above the table is `PASS` or `FAIL` and matches the exit code, so scripting on the exit
code is enough for a yes/no answer. When you need to know *what* happened, read the `detail` column
of the check you care about — it states what that check observed.

Two things are worth knowing before you treat a green run as evidence: only `fail` fails a run, and
a `skip` is not a `pass`. If a check you expected to exercise reports `skip`, read its `detail`
before concluding anything. [smoke checks and report](../reference/smoke.md) has the full table of
what each check asserts and what each status means.

## When it's red

1. Read the `detail` for the failing check first — it names what broke, not just that something did.
2. If check 3 (`capabilities+doctor`) failed, run `glass-mcp doctor` directly. It gives fuller
   per-check remedy guidance than `smoke`'s one-line summary.
3. If check 5 (`a11y snapshot`) failed, the accessibility bus is the usual cause — `doctor` reports
   whether it's reachable and how to start it. See [Before you run it](#before-you-run-it).
4. If check 2 (`start`) or later failed, try `--app` with a different candidate you have installed,
   to see whether the failure is specific to the app that got picked rather than to glass itself.
5. Re-run with `--report <path>` and attach the JSON when
   [filing an issue](https://github.com/fixed-width/glass/issues) — it carries every check's status
   and detail, alongside the backend, version and app the run used.
