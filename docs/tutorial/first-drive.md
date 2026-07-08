# Your first drive: build, drive, and debug a real UI

In this tutorial you'll watch an AI agent do something it can't normally do: **build a GUI app, look at
it, act on it, and see the result change — with no human confirming anything.** You'll set glass up,
point your agent at a small demo UI, and drive the whole **build → see → interact → debug** loop end to
end. It takes about ten minutes.

We'll use the **Linux X11** backend because it needs no display setup — glass spawns its own private
headless screen, so nothing lands on your desktop and every step is repeatable. The target is
`glass-fixture-egui`, a small [egui](https://github.com/emilk/egui) app that ships with glass: a window
with a **Text** field, a **Value** slider, and an **Apply** button. Unlike a static page it *responds*,
so you'll see the screen actually change when the agent acts — and watch glass report exactly what
changed.

## Before you start

Install the prerequisites (Debian/Ubuntu shown; other distros are in
[how-to/setup-linux.md](../how-to/setup-linux.md)):

```bash
sudo apt-get install -y xvfb bubblewrap libgl1-mesa-dri libegl1   # display · sandbox · software GL
# and Rust, via https://rustup.rs
```

The egui app renders with OpenGL, so the headless display needs a software GL driver
(`libgl1-mesa-dri` / `libegl1`); the plain X11 fixtures don't, but this one does.

## Step 1 — Build glass

From a clone of the glass repo, build the server:

```bash
cargo build --release -p glass-mcp
```

This produces `target/release/glass-mcp`. The first build pulls the pinned nightly toolchain
automatically, so it may take a few minutes; later builds are fast. You don't build the demo app
yourself — in Step 3 your agent builds it *through* glass, which is the whole point.

Confirm the environment is ready:

```bash
./target/release/glass-mcp doctor
```

You'll see a list of `✓` checks ending in **OK** — the X11 backend, `Xvfb`, and bubblewrap are all in
place.

## Step 2 — Point your agent at glass

Register the server with your MCP client (full details in
[how-to/connect-an-agent.md](../how-to/connect-an-agent.md)). For Claude Code:

```bash
claude mcp add glass --scope user -- "$PWD/target/release/glass-mcp"
```

Restart your agent so it picks up the new tools. From here on, **you talk to your agent in plain
language** and it calls the glass tools. We'll show each tool call and the result it returns, so you can
follow along.

## Step 3 — Build and launch the app (build →)

Run your agent from your glass checkout (so the relative paths resolve), and ask it: **"Use glass to
build and launch the egui demo — build it with `cargo build --release --manifest-path
crates/glass-fixture-egui/Cargo.toml` and run
`crates/glass-fixture-egui/target/release/glass-fixture-egui` on the x11 backend."** It calls:

```jsonc
glass_start {
  "build": "cargo build --release --manifest-path crates/glass-fixture-egui/Cargo.toml",
  "run": ["crates/glass-fixture-egui/target/release/glass-fixture-egui"],
  "backend": "x11"
}
// → { "x": 0, "y": 0, "width": 400, "height": 300 }
```

glass ran the build, spun up a private headless display, launched the freshly-built app inside the
sandbox, and found its window. The `400×300` geometry confirms it compiled and came up. This is the
**build** phase — your agent compiled the app itself, then drove it. (The first build pulls in egui's
dependencies, so it can take a minute; the build always runs unsandboxed with your full toolchain, while
only the launched app is contained.)

## Step 4 — See it ( → see)

Ask: **"Take a screenshot and tell me what's in the window."**

```jsonc
glass_screenshot
// → an image, plus { "width": 400, "height": 300 }
```

The image goes to your **agent**, not to you directly — and that's the point: the agent looks at the UI
for itself, so it never has to ask *you* "does this look right?". It reports back a small form: a
**Text:** field at the top, a **Value** slider, and an **Apply** button. (Image-rendering clients like
Claude Code also show the returned screenshot in your chat, so you can see it too — but the agent is the
one acting on it.)

Save this frame as a baseline, so we can measure what changes next:

```jsonc
glass_baseline_save { "name": "before" }
// → saved baseline 'before'
```

## Step 5 — Interact ( → interact)

Ask: **"Click the Text field and type 'hello glass' into it."** The agent batches the focus-then-type as
one known sequence with `glass_do` — the `settle` lets the field take focus before the keys land:

```jsonc
glass_do {
  "actions": [
    { "action": "click", "x": 150, "y": 30 },
    { "action": "settle" },
    { "action": "type", "text": "hello glass" }
  ]
}
// → { "executed": 3 }
```

The **Text:** field now reads `hello glass`.

## Step 6 — Debug: what changed, and did it really happen? ( → debug)

First the cheap check — ask: **"Diff against the 'before' baseline."**

```jsonc
glass_diff { "name": "before" }
// → { "changed_pct": 0.6, "changed_pixels": 737,
//     "bbox": { "x": 8, "y": 26, "width": 280, "height": 19 }, "total_pixels": 120000 }
```

This is what the whole loop is built for: glass tells the agent — **as text, for zero vision cost** —
that about 0.6% of the window changed, inside a `bbox` that is exactly the Text field. The agent knows
*what* moved and *where* without decoding a single image. (Your exact `changed_pct` and pixel count
will vary by a hair — the blinking text cursor and anti-aliasing shift a few pixels — but the `bbox`
pins the field.)

Now the ground truth. The fixture logs every change it makes, so ask: **"Show me the glass logs
containing `text=`."**

```jsonc
glass_logs { "contains": "text=" }
// → [fixture] text=hello glass
```

**The app itself recorded the value it now holds — `hello glass`, the exact text we asked for.** The
agent built the app, saw it, drove it, and confirmed the result against the app's own record, with
nobody in the loop. The build → see → interact → debug cycle is closed.

## Step 7 — Stop

Ask: **"Stop glass."**

```jsonc
glass_stop
// → stopped
```

glass tears down the app and the private display it created.

## What you just did

You watched an agent build, launch, see, drive, and verify a real GUI app entirely on its own — and,
crucially, watched glass report *exactly* what changed on screen as cheap text. That's the loop that
lets an agent debug UI code without stopping to ask you "does this look right?". You did it against a
demo app, but nothing above changes when you point glass at your own: swap in your app's `build` and
`run` commands and ask away.

Three things to do next:

- **Give your agent this loop permanently.** Install the [glass-drive skill](../how-to/drive-glass-well.md)
  so it arrives already knowing the cheap-verify habits you just saw.
- **Address elements by name, not pixels.** This egui app exposes an accessibility tree, so relaunch it
  with `glass_start`'s `a11y: true` and try `glass_a11y_snapshot` — you'll get the **Text** field, the
  **Value** slider, and the **Apply** button as addressable elements. Then `glass_set_value` the slider
  or `glass_click_element` the button by `#id`, no coordinates needed. See the
  [tool reference](../reference/tools.md).
- **Drive your own app.** Give `glass_start` your project's `build` command and binary, and read
  [explanation/the-loop.md](../explanation/the-loop.md) for the thinking behind the text-first workflow.
