# Your first drive: close the loop on a real window

In this tutorial you'll watch an AI agent do something it can't normally do: **look at a running GUI
app, act on it, and check the result — with no human confirming anything.** You'll build glass, point
your agent at a small test window, and drive the whole **build → see → interact → debug** loop end to
end. It takes about ten minutes and ends in a result you can see for yourself.

We'll use the **Linux X11** backend because it needs no display setup at all — glass spawns its own
private headless screen, so nothing lands on your desktop and every step below is perfectly
repeatable. The target is `glass-testapp`, a tiny fixture that ships with glass: a 320×240 window of
four coloured quadrants that echoes every click to its log. It's deliberately dull, which is exactly
what makes it a reliable first drive.

## Before you start

Install the three prerequisites (Debian/Ubuntu shown; other distros are in
[how-to/setup-linux.md](../how-to/setup-linux.md)):

```bash
sudo apt-get install -y xvfb bubblewrap    # private display + the sandbox
# and Rust, via https://rustup.rs
```

## Step 1 — Build glass

From a clone of the glass repo, build the server:

```bash
cargo build --release -p glass-mcp
```

This produces `target/release/glass-mcp`. The first build pulls the pinned nightly toolchain
automatically, so it may take a few minutes; later builds are fast. You don't build the test app
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
language** and it calls the glass tools. We'll show each tool call and the exact result it returns, so
you can follow along.

## Step 3 — Build and launch the app (build →)

Run your agent from your glass checkout (so the relative paths below resolve), and ask it: **"Use
glass to build and launch the test app — build with `cargo build --release -p glass-testapp` and run
`target/release/glass-testapp` on the x11 backend."** It calls:

```jsonc
glass_start {
  "build": "cargo build --release -p glass-testapp",
  "run": ["target/release/glass-testapp"],
  "backend": "x11"
}
// → { "x": 0, "y": 0, "width": 320, "height": 240 }
```

glass ran the build, spun up a private headless display, launched the freshly-built app inside the
sandbox, and found its window. The `320×240` geometry it returns confirms the app compiled and came
up. This is the **build** phase of the loop — your agent compiled the app itself, then drove it. (The
build step always runs unsandboxed with your full toolchain; only the launched app is contained.)

## Step 4 — See it ( → see)

Ask: **"Take a screenshot."**

```jsonc
glass_screenshot
// → an image, plus { "width": 320, "height": 240 }
```

You'll see the window: **red** top-left, **green** top-right, **blue** bottom-left, **white**
bottom-right. Your agent is now looking at the same pixels you would — this is the moment it stops
needing to ask you "does this look right?".

Save this frame as a baseline so we can compare against it later:

```jsonc
glass_baseline_save { "name": "quadrants" }
// → saved baseline 'quadrants'
```

## Step 5 — Interact ( → interact)

Ask: **"Click at x=80, y=60."** That point is inside the red quadrant.

```jsonc
glass_click { "x": 80, "y": 60 }
// → ok
```

## Step 6 — Debug: read what the app saw ( → debug)

How do we know the click actually landed where we aimed? `glass-testapp` prints every event it
receives to its log. Ask: **"Show me the glass logs containing EVENT."**

```jsonc
glass_logs { "contains": "EVENT" }
// → EVENT button=1 x=80 y=60 state=0
```

**Notice that the app recorded a left-button press at exactly `x=80 y=60`** — the coordinates we asked
for. The agent just verified its own action against the app's own record, with nobody in the loop. The
build → see → interact → debug cycle is closed.

## Step 7 — Verify cheaply, with no vision tokens

The loop's real speed comes from checking results as **text** instead of spending a screenshot every
time. Ask: **"Diff against the quadrants baseline."**

```jsonc
glass_diff { "name": "quadrants" }
// → { "changed_pct": 0.0, "changed_pixels": 0, "total_pixels": 76800, "bbox": null }
```

`changed_pct: 0` is the truth: our test window doesn't repaint when clicked, so nothing on screen moved
— and glass told the agent that as a line of text, for zero vision cost. When you point glass at an app
*you're* building, this same call reports a non-zero `changed_pct` and a `bbox` of exactly the region
that changed, so the agent knows what moved without decoding an image. The same instinct powers the
settle check:

```jsonc
glass_wait_stable { "include_image": false }
// → { "settled": true, "saw_motion": false, "width": 320, "height": 240 }
```

Text in, text out — an agent can act, confirm, and move on without ever paying to look. (Every tool
and its options are in the [tool reference](../reference/tools.md).)

## Step 8 — Stop

Ask: **"Stop glass."**

```jsonc
glass_stop
// → stopped
```

glass tears down the app and the private display it created.

## What you just did

You watched an agent build, launch, see, drive, and verify a real GUI app entirely on its own — the
loop that lets it debug UI code without stopping to ask you. You did it against a fixture, but nothing
above changes when you point glass at your own app: swap in your app's `build` and `run` commands and
ask away.

Two things to do next:

- **Give your agent this loop permanently.** Install the [glass-drive skill](../how-to/drive-glass-well.md)
  so it arrives already knowing the cheap-verify habits you just saw, instead of rediscovering them.
- **Drive your own app.** Give `glass_start` your project's `build` command and binary, and read
  [explanation/the-loop.md](../explanation/the-loop.md) for the thinking behind the text-first workflow.
