# Session trace

Tracing is optional, off by default, and independent of the [audit log](audit-log.md). It retains
evidence on the server host until the operator deletes it. It never uploads data. See
[Record and export session evidence](../how-to/record-session-evidence.md) for setup and commands.

## Configuration

| Flag | Meaning |
| --- | --- |
| `--trace-dir <absolute-directory>` | Existing operator-owned root; creates one random private child per server process. No symlink/reparse traversal or filesystem roots. |
| `--trace-max-bytes <bytes>` | Default `268435456`; inclusive range `1048576`–`2147483648`; requires `--trace-dir`. |

Both flags work with stdio, HTTP, full/lean profiles and macOS menu-bar HTTP. Non-serving commands
ignore recording configuration and create no trace. Offline commands also work in builds without
default features.

## Captured data

- Advertised tool definitions and instructions, server version, OS, architecture, transport and
  tool profile. An unavailable source revision is `null`.
- Local client/call identifiers, receipt and worker execution ordering, elapsed times, known
  backend and app-session context, typed tool arguments, logical results and constructed responses.
- Requested text and WebP images: screenshots, marked observations, snapshots, discoveries, diffs,
  waits, logs and clipboard output, including requested return/then observations.
- Original batch structure, completed/failed/unexecuted steps, dispatch/confirmation details,
  content-block indices, trust labels, untrusted text fences and artifact mappings.
- Observable request abandonment, teardown status, capacity omissions and recording failures.

Arguments are normalized from accepted typed parameters; they are not a copy of the transport
frame. Times identify recording/execution/result boundaries, not exact image acquisition times.
The recorder does not request extra images, accessibility walks, log reads, polling or actions.
It retains no internal baseline image unless a requested result returns it.

The recorder excludes transport credentials, HTTP headers, protocol session tokens, request
`_meta`, the host environment, and both names and values from `glass_start.env`. Only the explicit
environment entry count is retained. Invalid arguments produce a rejection category and compact
decoded argument byte count, without their body or an echoing error message. Existing secure-field
suppression remains in effect. Other supplied arguments and app output can contain secrets;
audit redaction settings do not redact traces.

Worker execution continues to be recorded when a waiting caller is cancelled. Response construction
is recorded separately, with client delivery `unknown`. Trace completeness concerns recorded
evidence, not application correctness or confirmed client receipt.

## Bounds and live status

| Bound | Value |
| --- | --- |
| Total retained bytes | Configurable; counts metadata, journal, payloads and temporary writes |
| Reserved terminal/metadata allowance | 128 KiB within the total limit |
| Individual payload | 16 MiB |
| Pending data | 32 MiB |
| Queued events | 256 |
| Admitted calls | 10,000 |
| Journal events | 100,000, including the terminal event |
| Single journal event | 64 KiB |
| Unique payloads | 25,000 |
| Evidence entries per event | 128, including an omission marker when blocks exceed the bound |
| Offline timeline index | 8 MiB; remaining events are validated but omitted from the index |

Oversized payloads are omitted whole; later smaller evidence can still be retained. Total, call,
event and queue exhaustion stop new recording with state `limited`. I/O failure produces `failed`.
Both preserve earlier evidence, mark the trace incomplete and leave tool execution and `isError`
unchanged. There is no retry or eviction to recover missing evidence. Invalid trace setup refuses
server startup.

Enabled traces add `_meta.glass.trace` to successful protocol tool responses, including tool errors.
The status object contains `enabled`, `state`, `stored_bytes`, `calls`, `limits`, `omissions` and
`errors`. `glass_doctor` also includes this object as `trace`. Status can lag asynchronous writes.
Both additions are absent when tracing is disabled. Protocol-level rejections retain their existing
error shape. No new MCP tool, trace resource or HTTP endpoint is added.

Shutdown tears down the target before waiting up to two seconds for trace finalization, then
cleans temporary artifacts. A crash, unresolved call, omission or writer failure leaves incomplete
evidence. The trace is not an audit durability, power-loss or tamper-proofing guarantee.

## Directory and archive format

The schema is `glass.trace.v1`. A live/stopped trace directory contains:

```text
manifest.json
events.jsonl
blobs/<sha256>.bin
writer.lease
```

The manifest declares identity, scope exclusions, limits, state, completeness and counters.
Finalized manifests include journal byte length and SHA-256. Each ordered JSONL event has `seq`,
`elapsed_us`, `kind`, `call`, `client`, `data`, and optional `evidence`. Evidence descriptors identify
the block, MIME type, trust and either a relative `payload` (`path`, `bytes`, `sha256`) or `omitted`
reason. Artifact mappings additionally retain the historical `source_uri`. Text and image blobs
contain original bytes; JSON argument/envelope payloads contain normalized serialization.
Resource links also retain their full MCP descriptor in a `resource_descriptor` JSON payload,
associated with the same block index as the retained resource body.

Events distinguish `call_received`, `argument_size`, `arguments`, `execution_started`, `session_context`,
`logical_outcome`, `response_constructed`, `request_abandoned`, `router_rejection`,
`worker_unavailable`, and `resource_read`. Inventory, local client creation, shutdown and the
terminal `trace_closed` are separate control events. Execution ordinals are distinct from receipt
ordering. Batch steps remain within their original argument/result structures.

Offline inspection holds an exclusive source lease and validates schema, ordering, relative paths,
lengths and digests. It refuses missing or corrupt committed evidence and unsupported schemas.
An interrupted final line is an uncommitted tail; unreferenced staging files are reported and are
not exported. Human output escapes terminal controls and never prints payload bodies.

Export writes a new private ZIP with stored entries (no recompression), validated manifest/journal,
referenced blobs and `READING.txt`. Runtime leases and staging files are omitted. The destination
must be outside the source and must not already exist. ZIP overhead is separate from the trace's
source-storage limit. An interrupted export declares `recovered_interrupted_prefix` and remains
incomplete. Export never fetches arbitrary URIs, runs recorded commands, extracts an input archive
or renders application content.

| Exit code | Inspection/export result |
| --- | --- |
| `0` | Valid complete trace; export created the ZIP |
| `2` | Valid incomplete trace; export still created the ZIP |
| `1` | Invalid input, active source, corruption or I/O failure; no new destination published |
