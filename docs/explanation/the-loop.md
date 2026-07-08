# The build → see → interact → debug loop

glass exists to close a loop that an AI coding agent otherwise can't close on its own: it can write
GUI code, but it can't *see* whether the code did the right thing. Without a way to look at the
running app, an agent has to stop and ask a human "does this look right?". glass removes that stop.

## Driving apps as a black box

glass drives an application the way a person at the keyboard would — it launches the app, captures
what is on screen, moves the mouse and presses keys, reads the app's logs, and notices when the
picture changes. It never asks the app to cooperate. There is no SDK to link, no hook to install, no
accessibility contract the app must honour. Because everything happens from the outside, glass works
with **any** native GUI app regardless of toolkit or language — GTK, Qt, egui, Win32, AppKit, a game's
custom renderer — they all present the same surface to an external driver: pixels, input, and a window.

That external-only stance is a deliberate invariant, not an accident of implementation. The moment
glass required an app to be "glass-aware", it would stop working for the long tail of apps that are
exactly what a developer needs to debug.

## The loop

Point an agent at a GUI app and it runs the whole cycle itself:

- **build** — glass launches the app, optionally running a build command first, inside a sandbox.
- **see** — it captures the window (or a sub-region) as a lossless image.
- **interact** — it clicks, types, drags, scrolls, or drives the accessibility tree.
- **debug** — it reads the app's stdout/stderr, saves a baseline, and diffs later frames against it
  to detect what changed.

The agent stays in control of the sequence. glass supplies the primitives and the observations; the
agent decides what to do next based on what it sees. The concrete tools that make up each phase are in
the [tool reference](../reference/tools.md).

## Why the observe tools return text

Looking at a screenshot costs an agent vision tokens, and that cost scales with the pixel area of the
image. A naive loop — screenshot, act, screenshot, compare — pays that cost on every check, most of
which only need to answer a yes/no question: *did anything change yet?*

glass is built so those checks are cheap. The comparison and waiting tools — `glass_diff`, the
`glass_wait_for_*` family, and `glass_wait_stable` with `include_image:false` — return **text only**:
a changed-percentage, a bounding box, a matched/not-matched flag. An agent can save a baseline, act,
and confirm the result moved without ever decoding a new image. It spends a screenshot only when it
genuinely needs to *read* the pixels — and even then it can crop to a tight region, because a small
region is a large, recurring saving over a full-window capture.

The same instinct drives the wait-for-condition tools: instead of polling with screenshots until a
button enables or a log line appears, an agent issues one blocking text call that returns when the
condition is met (or times out softly). Fewer round-trips, no wasted vision.

## No silent fallbacks

A debugging tool that quietly hands back a stale or blank frame is worse than one that fails — the
agent would reason about a picture that isn't real. So glass treats a failed capture or a failed input
as a structured error, never a substitute result. When the accessibility tree is unavailable, the
a11y tools say so rather than fabricating an empty tree, and the agent falls back to pixels. The
observations glass returns are either true or an honest error.
