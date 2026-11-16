# Measure what the verification loop costs

Measure the actual tool calls and observations for a fixed task, then apply your model's token
accounting to those observations. A screenshot count or byte count alone is not a billable token
count.

For current repeated measurements, use [Measure repeated application interactions](measure-interactions.md).
It covers preflight, fresh attempts, exact outcome checks and portable result validation. For native
forms and application transfers, use [Measure application boundaries](measure-application-boundaries.md).

## Define comparable runs

Keep the application build, initial state, task, final assertion, host and tool profile fixed.
Record the Glass revision, driver configuration, image bounds and repetitions with the results.
Include discovery, action and verification calls in the comparison. Separate setup/build time from
task time, and keep failed attempts in the outcome counts.

To compare semantic and visual verification, use the same assertion in both runs. An acknowledged
click and a changed image do not by themselves prove that a form saved the expected value.
Measure both the value and the activation count where the fixture exposes them.

## Read the measurements

The [interaction runner](../../tools/interaction-bench/README.md) preserves requests, responses,
artifacts and phase costs. Validate the result directory before interpreting its summary:

```bash
python3 tools/interaction-bench/run.py validate /absolute/path/results
python3 tools/interaction-bench/run.py summarize /absolute/path/results
```

Compare request and response text bytes, tool calls, images and their decoded dimensions. If the
run reads output artifacts, count those reads too. Full and lean inventories have different schema
costs; inspect the one used by the run without starting an application:

```bash
glass-mcp --tool-profile full tools --json
glass-mcp --tool-profile lean tools --json
```

`tools_json_bytes` and `instructions_bytes` measure UTF-8 context size. They exclude JSON-RPC
framing and do not measure model tokens.

## Convert observations to model costs

Use the tokenizer and image-accounting rules for the exact model and detail setting used by your
agent. Image accounting can depend on resizing, tiles, patches or detail level; it need not scale
linearly with source pixels. Base64 size and compressed WebP size are not image-token estimates.

A text approximation such as `ceil(text_bytes / 4)` is useful for a rough comparison, but does not
account for a particular tokenizer, cached context or protocol overhead. Keep the raw bytes and
image dimensions so the estimate can be recomputed.

To compare a smaller preview, set the same `max_width`/`max_height` in every measured observation.
Those controls shrink returned images; diff and stability checks still use native pixels.
See [Capture a smaller image](capture-a-smaller-image.md).

## Legacy egui comparison

`scripts/verification-cost.sh` runs the older, opt-in `glass-testapp` verification-cost tests.
It needs Xvfb, a private AT-SPI bus and the workspace-excluded egui fixture. Its semantic arm uses
six calls and no images; its screenshot arm uses eight calls and four images. The screenshot arm
uses an unmetered accessibility snapshot to locate controls and an unmetered log check, so it is a
scripted observation-cost comparison, not a vision agent benchmark.

The legacy invariant test still asserts 400×300 images while the current fixture opens at 700×500.
That assumption needs updating before the complete script can be used as a passing measurement
gate. Historical text-token totals and cost ratios do not describe the current build. Use the
interaction runner for new cohorts.
