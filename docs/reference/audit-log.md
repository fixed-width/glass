# Audit log

glass can append a JSONL record of every actuation it performs. It is **opt-in**: pass
`--audit-log <path>` or set `GLASS_AUDIT_LOG=<path>` (see [reference/environment.md](environment.md)).
The hook lives in the core actuation path, so no actuation can bypass it. `glass-mcp doctor` reports
whether auditing is on, the path, and the content mode.

## What is recorded

**Actuations** — launch/stop, type, key, click, drag, scroll, `set_value`, clipboard writes, element
clicks, window focus/resize/move, and each `glass_do` sub-action.

**Not recorded** — reads: screenshots, diffs, accessibility snapshots, and log/clipboard reads.

## Record schema

One JSON object per line, with these fields:

| Field | Meaning |
|---|---|
| `seq` | Monotonic sequence number |
| `ts` | Timestamp |
| `action` | The actuation (e.g. `click`, `type`, `launch`) |
| `target` | Attribution metadata — the active window's title, an element's role/name |
| `args` | The action's arguments |
| `result` | Outcome |
| `content` | For content-bearing actions, a content descriptor (see below) |

Launch records intentionally omit `env` and `cwd`.

## Content redaction

Typed / clipboard / launch content is **redacted by default** so the log is not a secret sink. The
mode is set by `GLASS_AUDIT_CONTENT`:

- `redacted` (default) — store a descriptor: content length + SHA-256 + a short prefix.
- `full` — store verbatim text.
- `none` — store no content.

`GLASS_AUDIT_PREFIX_LEN=<n>` sizes the plaintext prefix (`0` disables it).

## What is plaintext regardless of mode

Two things are recorded in plaintext whatever `GLASS_AUDIT_CONTENT` is set to:

- the short content **prefix** (default 8 chars — set `GLASS_AUDIT_PREFIX_LEN=0` to drop it), and
- **target metadata** — the active window's title and an element's role/name, which is attribution,
  not actuation content.

A window title or field label can itself be sensitive, so treat the log as confidential.
