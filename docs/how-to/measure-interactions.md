# Measure repeated application interactions

Use the interaction benchmark to check a Glass build against deterministic web tasks and measure
the cost of completing them through its actual MCP interface. You need Python 3.10+, Linux, Xvfb,
`dbus-daemon`, the AT-SPI prerequisites from [verify a change](verify-a-change.md), and an installed
browser that publishes accessibility controls. The runner uses owned displays and profiles.

1. Build the server before measuring:

   ```bash
   cargo build --release --locked -p glass-mcp
   ```

2. Save a configuration outside the checkout, replacing the browser path:

   ```json
   {
     "browser": "/absolute/path/firefox",
     "browser_family": "firefox",
     "cases": ["large-form", "delayed", "disabled", "duplicate", "moving", "iframe", "artifact"],
     "repetitions": 10,
     "warmups": 1,
     "seed": 41
   }
   ```

   Use `list` to see every case. Begin with `repetitions:1,warmups:0` when checking a new browser.
   The selected matrix is explicit in the manifest; a subset is not full-suite acceptance.
   Commit the source before a measured cohort, or set `allow_dirty:true` for development diagnostics.

3. Check prerequisites, then run:

   ```bash
   python3 tools/interaction-bench/run.py preflight --config /absolute/path/config.json
   python3 tools/interaction-bench/run.py run --config /absolute/path/config.json
   ```

   The runner prints its output directory under `target/interaction-bench/`. It alternates configured
   driver order within seeded case blocks and starts a fresh session for every attempt. Builds and
   downloads are excluded from task time. Discovery and final verification are included.

4. Revalidate and summarize the recorded run:

   ```bash
   python3 tools/interaction-bench/run.py validate /absolute/path/results
   python3 tools/interaction-bench/run.py summarize /absolute/path/results
   ```

   `validate` checks evidence integrity and recomputes facts and costs. A valid report can contain
   failed tasks: inspect outcome counts as well as timings. Missing publication, unproven occlusion,
   wrong values and duplicate activations are evidence of a limitation or regression, not successful
   conformance. Preserve the failed run before changing code or settings.

For packaged Electron, Android and native application tasks, see
[measure application boundaries](measure-application-boundaries.md).
For configuration limits, record formats, metrics, outcome rules and cleanup behavior, see the
[runner reference](../../tools/interaction-bench/README.md). The existing
[verification-cost benchmark](verification-cost.md) remains useful for its native fixture and
semantic-versus-screenshot workflows.
