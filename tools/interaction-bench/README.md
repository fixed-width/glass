# Interaction benchmark

Measure repeated scripted interactions through an external `glass-mcp` process. The runner captures
wire responses, checks exact application outcomes, preserves failures and resources, and regenerates
reports offline. It does not run an LLM or measure model decision-making.

Start with [Measure repeated application interactions](../../docs/how-to/measure-interactions.md).
Execution currently requires Linux/X11; the measurement and protocol tests require only Python 3.10+.

## Commands

```bash
python3 tools/interaction-bench/run.py list
python3 tools/interaction-bench/run.py preflight --config /absolute/path/config.json
python3 tools/interaction-bench/run.py run --config /absolute/path/config.json
python3 tools/interaction-bench/run.py validate /absolute/path/results
python3 tools/interaction-bench/run.py summarize /absolute/path/results
python3 -m unittest discover -s tools/interaction-bench/tests
```

`preflight` inspects local prerequisites and source identity without launching the target. It does not
prove live accessibility publication. Use a one-iteration diagnostic run for that evidence.

Exit status: `run` returns 0 only when every selected attempt, including warm-ups, satisfies its
declared outcome and cleanup/evidence checks; unsuccessful or skipped attempts return 1. Configuration
and prerequisite errors return 2. Predeclared optional exclusions remain visible without failing the
run; required cells cannot be excluded. `validate` returns 0 when the recorded evidence is consistent,
including correctly recorded failures; it does not turn those failures into conformance passes.
`summarize` validates before printing JSON. Results are never overwritten.

## Configuration

The minimum configuration is `{"browser":"/absolute/path/firefox"}`. Defaults:

| Setting | Default |
|---|---|
| `drivers` | One Glass adapter using `target/release/glass-mcp` |
| `cases` | All cases shown by `list` |
| `repetitions`, `warmups`, `seed` | 10 measured, 1 warm-up, ordering seed 41 |
| `browser_family` | `firefox`; also accepts `chromium` |
| `sandbox` | `off`, for this owned local fixture; recorded in the manifest |
| `display`, `viewport` | 1280×900 Xvfb; observed content viewport 1000×700 at scale 1 |
| `action_timeout_ms`, `attempt_timeout_ms` | 10000 and 180000; cleanup reserves 10 seconds |
| `delay_ms`, `motion_ms` | 3000 each |
| `frame_limit_bytes`, `evidence_limit_bytes` | 64 MiB per frame; 512 MiB wire/archive caps and aggregate acceptance limit |
| `allow_dirty` | `false`; opt in only for labelled development evidence |
| `optional_cases`, `exclusions` | Empty; every scheduled case is required |

`browser_args` appends declared browser launch flags. `app_env` supplies declared target environment
overrides. The runner uses a fresh profile, display, private session bus and runtime directory per
attempt. It does not use your open browser or operator display. It stops after cleanup failure or
interruption and records the remaining scheduled attempts as skipped.

Every driver entry has `id`, `adapter`, and an executable argument array `command`. The public runner
registers only `glass`; another contributor tool can call `main(registry)` with an additional adapter.
Adapters must preserve the case outcomes, use the metered transport, and provide offline decoding.
`identity_files` lists adapter source/lock files to hash and archive. Basenames must be unique per
driver. Optional exclusions have `driver`, `case` and a nonempty `reason`; their case must appear in
`optional_cases`. They are recorded as skipped, never as passes.

## Records and accounting

`run.schema.json` describes the version-1 manifest and its attempt records. A run contains:

- `manifest.json`: frozen configuration/schedule, source and executable identities, host/browser
  identity, fixture hashes, and preparation duration.
- One numbered directory per scheduled attempt, with `result.json`, `timing.json`, raw stdout/stderr,
  exact JSON request/reply files, `calls.json`, and the full paginated tool inventory.
- `evidence/`: recovered resources, with original references and digests in the attempt record.
- `source/`: the runner source used by the run; dirty tracked changes also have `source.patch`.
- `summary.json`: phase-separated summaries; regenerate it from the retained evidence.

Phases are `server_start` (including owned display/bus setup, handshake and inventory), `app_start`
(launch, readiness and viewport verification), `task` (discovery, actions, waits and outcome checks),
`evidence`, and `cleanup`. “Cold” means a new process/profile, not a flushed OS page cache.
All raw received bytes, including notifications and partial output, are preserved in `stdout.bin`.
On a size-limit violation the retained stream ends at the wire cap. Archive writes have their own
cap, and an aggregate size check rejects oversized attempts. These are evidence limits, not a disk
quota for child processes; file-producing adapters must use the owned output directory.
Per-call wire byte counts exclude the framing newline and cover the exact request and matching reply.
Unsolicited messages are preserved separately in the raw stream and are not counted as tool replies.

`response_text_bytes` counts decoded UTF-8 text content. Images have separate counts and encoded/decoded
byte totals; no image-token or exact text-token claim is made. Inventory cost is the combined tools
array serialized as compact UTF-8 JSON with sorted object keys, preserving tool order. It is reported
separately from task text and is not amortized implicitly.

Every dispatched RPC counts even if it times out. A batch is one tool call with a separate constituent
action count. Internal server polling is unknown unless exposed; runner observation calls are counted.
The runner never retries a mutation or a call with a lost reply. Resource reads needed for outcome
proof belong to task time. Pure archival reads belong to evidence time. Stored artifact bodies are
deduplicated; references, reads and their byte costs are not.

Summaries retain warm-ups separately and report every failure/refusal/exclusion count. Successful task
durations, expected-refusal durations and failed-attempt wall times have separate distributions.
Task and refusal costs also stay separate. Percentiles use nearest rank;
p95 is absent below 20 samples. A ten-run result establishes local repeatability, not a general
performance claim. Do not pool different configurations, hosts, browser builds, modes or geometries.

## Case expectations

The form must show the exact submitted value and exactly one submission. Disabled, ambiguous and
occluded pointer targets must be refused without activating either target or cover. A batch's success
or a returned `ok:true` alone cannot pass the case. Motion requires two distinct observed bounds in
the triggered generation. Frames require an inner action and unchanged outer counter.

Occlusion detection depends on what the backend can prove. An `unproven` hit-probe result is not a
strict occlusion pass. If the pointer activates the cover, that attempt fails and remains in the
report; do not silently remove the case or reinterpret it as successful input. The two geometry
variants make that capability boundary visible. The standalone artifact case must recover an
oversized observation containing the form and the final repeated section.
