# Audit log

Glass can append JSONL records of application actuations. It is **opt-in**: pass
`--audit-log <path>` or set `GLASS_AUDIT_LOG=<path>` (see [reference/environment.md](environment.md)).
The hook lives in the core actuation path. `glass-mcp doctor` reports
whether auditing is on, the path, and the content mode.

## What is recorded

**Actuations** — launch/stop, type, key, click, drag, scroll, gesture, `set_value`, clipboard writes,
element clicks, and window focus/resize/move. A `glass_do` batch records its actuations through the
same paths; it does not add a record for a read-only wait or terminal observation.

**Not recorded** — pointer moves, geometry queries, screenshots, diffs, accessibility snapshots,
and log/clipboard reads. For requested observations as well as actions, use a [session trace](session-trace.md).

## Record schema

One JSON object per line, with these fields:

| Field | Meaning |
|---|---|
| `v` | Record version, currently `1` |
| `seq` | Monotonic sequence number within this sink; restarts begin a new sequence |
| `ts` | UTC RFC 3339 timestamp with milliseconds |
| `session` | Opaque launch-session identifier, or `null` before a launch |
| `action` | The actuation (e.g. `click`, `type`, `launch`) |
| `target` | Attribution metadata — the active window's title, an element's role/name |
| `args` | The action's arguments |
| `result` | `ok`, `duration_ms`, optional `error` and optional `dispatch` (`not_dispatched` or `may_have_dispatched`) |
| `content` | Optional content descriptor (see below); omitted in `none` mode |

Launch records intentionally omit `env` and `cwd`.

## Content redaction

Typed / clipboard / launch content is **redacted by default** so the log is not a secret sink. The
mode is set by `GLASS_AUDIT_CONTENT`:

- `redacted` (default) — store `len` (UTF-8 bytes), `sha256`, and an optional plaintext `prefix`.
- `full` — store `len`, `sha256` and verbatim `text`.
- `none` — store no content.

`GLASS_AUDIT_PREFIX_LEN=<n>` sizes the redacted-mode prefix in characters (default `8`; `0` disables it).

## What is plaintext regardless of mode

**Target metadata** — the active window's title or an element's role/name — remains plaintext in
every content mode. So do non-content arguments, including a launched program's path, coordinates
and key chords. `none` removes the content descriptor; it does not redact those fields.

A window title or field label can itself be sensitive, so treat the log as confidential.

Opening the audit sink fails server startup if the file cannot be opened. A later write failure
is reported on stderr and does not repeat or undo the action; this is not a durable delivery guarantee.
