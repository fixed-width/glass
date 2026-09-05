# Interaction benchmark

Measure repeated scripted interactions through an external `glass-mcp` process. The runner captures
wire responses, checks exact application outcomes, preserves failures and resources, and regenerates
reports offline. It does not run an LLM or measure model decision-making.

Start with [Measure repeated application interactions](../../docs/how-to/measure-interactions.md).
For packaged Electron, Android, native forms and cross-application transfer, use
[Measure application boundaries](../../docs/how-to/measure-application-boundaries.md).
Execution supports Linux X11/Wayland, native macOS/Windows, and an iOS publication probe.
The runner and portable tests require Python 3.9+. See [native host execution](../../docs/how-to/measure-native-hosts.md).

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
| `cases` | The twelve web cases; application cases require explicit selection |
| `repetitions`, `warmups`, `seed` | 10 measured, 1 warm-up, ordering seed 41 |
| `browser_family` | `firefox`; also accepts `chromium` |
| `sandbox` | `off`, for this owned local fixture; recorded in the manifest |
| `backend` | `x11`; also `wayland`, `macos`, `windows`, `ios` |
| `display`, `viewport` | 1280×900 Linux display; observed content viewport 1000×700 at scale 1 |
| `action_timeout_ms`, `attempt_timeout_ms` | 10000 and 180000; cleanup reserves 10 seconds |
| `delay_ms`, `motion_ms` | 3000 each |
| `frame_limit_bytes`, `evidence_limit_bytes` | 64 MiB per frame; 512 MiB wire/archive caps and aggregate acceptance limit |
| `allow_dirty` | `false`; opt in only for labelled development evidence |
| `optional_cases`, `exclusions` | Empty; every scheduled case is required |

`browser_args` appends declared browser launch flags. `app_env` supplies declared target environment
overrides. Linux runs use a fresh profile, display, private session bus and runtime directory per
attempt. Native desktop runs launch owned applications in the logged-in GUI session. It stops after cleanup failure or
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


## Application cases (recipe revision 3)

| Case | Required outcome |
|---|---|
| `electron-form` | Shared form saves Ada once; native dialog is observed, selected, focused and confirmed with Return; the main window shows Ada and one confirmation. |
| `android-boundary` | Native Open form leads to the embedded form; it saves Ada once; native Review saved value shows Ada, one submission and one review. |
| `native-form` | The native form saves Ada exactly once. |
| `ios-publication` | Capture the native and web tabs and probe their declared controls. Missing publication is `unsupported`; publication completion makes no typing or boundary-interaction claim. |
| `cross-application` | The source generates one fresh ticket; the destination saves the observed ticket exactly once; the source value and generation count remain unchanged. |

Typing requires confirmed native focus. Form actions use pointer mode on native/Android and native
semantic mode on Electron. The dialog recipe establishes native-window/default-key completion;
it does not claim semantic access to dialog buttons. Observed action order, exact values and quiet
counter checks are required. Android WebView field IDs are names and their human labels are
accessibility descriptions; both are checked without rewriting the observed facts.

Application records add `participants` (`app`, or `source` and `destination`). Each participant has its
own `mcp/`, `evidence/`, inventory, session ownership and call sequence. Top-level events include the
participant, and offline replay reconstructs their order from raw channel timestamps. Parent phase
costs sum all participant RPCs, while phase duration is measured once across the whole attempt.
Inventory bytes are the sum of per-session inventories; the combined digest identifies the mapping of
participant names to inventory digests. It is not the digest of a fabricated merged tools array.

Android emulator boot belongs to `server_start`, installation/readiness to `app_start`, and all native
and embedded form actions to `task`. Preparation commands use the owned ADB server for lifecycle and
device identity; task observations/actions use MCP. Packaged bundles, native executable, APK,
companions, emulator tools and system image files are hashed before/after the run. Fixture and runner
sources are archived. Existing version-1 web evidence remains readable by the revision-2 validator.

Native macOS/Windows fixtures are resized to 600×500 before task timing. macOS omits empty AX
strings in ordinary reads, so its native recipe first writes the empty string with `glass_set_value`
and requires `value_confirmed` for that exact target/value. Actual account entry still uses confirmed
focus and typing. This reset is declared before execution and never retries a failed write.

Windows MCP processes belong to an unnamed Job with kill-on-close ownership; residual descendants
make cleanup unhealthy. POSIX native sessions track an exact per-attempt environment token. iOS
creates, boots, shuts down and deletes only its own Simulator identifier, preserving its original
case for companion compatibility. Its raw lifecycle commands and final device inventory are retained.
Unsupported publication results undergo full channel, request, artifact and lifecycle replay.
