# Record a workflow video

Use a named recording for a failed workflow or a selected successful demonstration. Recording is
available in both tool profiles on every backend. The result's `scope` identifies the surface:

| Backend/environment | Scope | Included content |
| --- | --- | --- |
| Wayland, Glass-owned Xvfb | `owned_display` | Entire private display, including separate dialogs and resized windows |
| X11 attached display | `window` | Initial selected window rectangle, including overlapping content inside it |
| macOS | `application_windows` | Launched application's windows and dialogs on their initial display; other apps excluded |
| Windows private child environment | `owned_display` | Entire private monitor, including separate dialogs and system UI |
| Windows shared desktop | `window` | Initial selected window; independent dialogs outside it are excluded |
| Android, iOS Simulator | `device_display` | Device display, including dialogs, notifications, keyboards and system UI |

Recording never falls back to capturing the host desktop. Changing the selected window does not
retarget an outstanding movie. Private display clips can contain other content in that environment.

## Enable retained video

Create an operator-owned directory and pass its physical absolute path to the server:

```sh
mkdir -m 700 "$HOME/glass-videos"
glass-mcp --record-dir /absolute/path/glass-videos
```

The flag also works with `serve --http`. On Windows use an owner-and-SYSTEM protected DACL,
as shown for the trace root in [Record session evidence](record-session-evidence.md).
Symlinks, missing directories, filesystem roots, and roots writable by another user are refused.
Recording is separate from `--trace-dir`; enabling one does not enable the other.

## Record through the verified outcome

Launch the application with `glass_start`, then call `glass_record`:

```json
{"op":"start","name":"Submit account form","max_duration_ms":30000}
```

Start waits for video data before returning. Drive the application with the ordinary Glass tools
and verify its outcome. Capture runs independently while an action, drag or wait is in progress.
Then classify the workflow:

```json
{"op":"stop","outcome":"failure"}
```

Failures and cancellations are retained by default. A successful workflow is discarded unless
selected explicitly:

```json
{"op":"stop","outcome":"success","keep":true}
```

Use `keep:false` to discard any outcome. There is one outstanding recording at a time, including a
recording that has reached its limit and is waiting for disposition. Inspect it with
`{"op":"status"}` before starting another.

## Read the clip

A retained result includes `artifact.uri`, server-local `artifact.path`, MIME type, bytes and a
SHA-256 digest. Read the URI with MCP `resources/read` to receive a binary blob. The captured video
is untrusted application evidence. Remote clients must use the URI rather than opening the
server-local path on the client machine.

Desktop and Android retain H.264 in MP4; iOS retains H.264 in a QuickTime movie. Desktop capture attempts run at most 15 times per second; native mobile frame rates may
vary; use the trace and app-side verification for timing and correctness. The URI lasts for the
server process. Movies and JSON metadata under the configured directory survive server exit.

## Keep recent desktop history

Start an explicit rolling buffer after launching a desktop app:

```json
{"op":"buffer_start","history_ms":10000}
```

Capture runs independently while ordinary Glass tools drive the app. Recent encoded frames stay
in memory; buffering writes no movie. The default history is 10 seconds, configurable from 1 to
30 seconds. Android and iOS Simulator report `buffer_supported:false` and retain their existing
scoped recording support.

After detecting an incident, mark the available preceding history and a short result tail:

```json
{"op":"mark","name":"Unexpected validation error","outcome":"failure","tail_ms":2000}
```

The tail defaults to 2 seconds, with a 0–5 second range. Failure and cancellation marks keep by
default; successful marks need `keep:true`. An ignored mark leaves any pending kept clip unchanged.
Overlapping marks merge into one pending clip, retaining their names and outcomes. A disjoint mark,
more than 32 marks, or a merged window beyond 60 seconds is refused; save the pending clip first.
Failure takes precedence over cancellation and success in the merged outcome.

```json
{"op":"save"}
```

Save waits for the remaining tail, persists one immutable movie through the same protected store,
and leaves buffering active. Subsequent marks create another group; their lookback may overlap
an explicitly saved movie. Arbitrary tool errors do not automatically mark or save anything.

Status includes `buffer`, its actual retained duration and bytes, and any pending clip. History
evicts complete keyframe groups, so boundaries follow available keyframes. Warm-up, byte pressure,
capture failure, and a shortened tail are disclosed rather than claiming missing history. The
unmarked history and the pending clip each have an 8 MiB encoded allowance; `pending_bytes` includes
the bounded extra samples held for possible merging. Both also have finite sample counts.

```json
{"op":"buffer_stop"}
```

Stopping saves any marked kept clip and discards unmarked history. Use `keep:false` to discard the
pending mark too. Application stop, replacement and server shutdown attempt the same marked-only
finalization before teardown. A tail cut short by stopping is reported. Scoped recording and
buffering are mutually exclusive; stopping one never implicitly starts the other.

## Limits and cleanup

The default clip limit is 30 seconds; choose between 1 and 60 seconds. Each clip is bounded to
8 MiB. The server reserves a maximum clip's allowance before starting capture, with a 256 MiB
allowance and at most 1024 retained clips per server process. Repeated server runs accumulate files
until the operator removes them. A limit stops capture, not application work. Status reports the
limit or recording failure; inspect it before another recording.

Stop recording before stopping the application for the best chance of retaining the complete
outcome. `glass_stop`, application replacement, and server shutdown attempt to finalize and keep an
outstanding clip as interrupted evidence before device teardown. Process shutdown has a short
shared cleanup deadline; a stalled recorder or disconnected device may leave no retained movie.
Recording failure never repeats input or prevents application cleanup.
If the Android device becomes unreachable, its recorder may continue until the native duration
limit because Glass cannot deliver the stop signal over a disconnected transport.

Contained desktop targets cannot access the configured root, including older clips. Sandbox-off
targets and other host processes running as the operator are outside that protection. Mobile
applications run behind their existing device isolation boundary.

Desktop video uses a fixed canvas with neither dimension exceeding 1280 pixels. Resized content
fits the canvas with black padding rather than being cropped. Sample timing follows monotonic
capture intervals, including a final acquisition and a hold through stop. A failed capture stops
the recorder and reports failed status; a valid video prefix can still be retained. Software
encoding runs inside the capture worker and starts no native encoder service. Build-from-source
users need a C++ compiler for the bundled OpenH264 dependency.
