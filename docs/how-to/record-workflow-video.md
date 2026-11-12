# Record a workflow video

Use a named recording for a failed workflow or a selected successful demonstration. Recording is
available for Android and iOS Simulator, in both tool profiles. It captures the device display,
including dialogs, notifications, keyboards and system UI, without opening a device window.

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

Android retains H.264 in MP4; iOS retains H.264 in a QuickTime movie. Frame rate is native and may
vary; use the trace and app-side verification for timing and correctness. The URI lasts for the
server process. Movies and JSON metadata under the configured directory survive server exit.

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
applications run behind their existing device isolation boundary. Desktop recording and a rolling
buffer are not implemented by this version.
