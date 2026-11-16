# Your first drive: build, capture, click and verify

In this tutorial you will use an agent to build and launch a small X11 app, inspect its window,
send input and verify that the app received it. Glass creates a private headless display, so the
window and input stay off your desktop.

The app is `glass-testapp`, included in this repository. It draws four coloured quadrants and logs
received events. It needs no GUI toolkit or GPU. This lesson uses pixels and logs; you can try
semantic controls afterwards.

## Before you start

Use a Linux host, a clone of this repository, Rust installed through rustup, and an MCP client that
can display images. Run the shell commands below from the repository root.

On Debian or Ubuntu, install the build and runtime prerequisites:

```bash
sudo apt-get install -y build-essential cmake xvfb bubblewrap dbus-bin at-spi2-core
```

Rustup installs the toolchain pinned in `rust-toolchain.toml`. The first build can take several
minutes. Run the commands on your development machine, where bubblewrap can create unprivileged
user namespaces.

## 1. Build Glass and check the host

```bash
cargo build --locked -p glass-mcp
./target/debug/glass-mcp doctor
```

Resolve any failed X11 or containment checks before continuing; `doctor` prints a remedy beside
each failure. The [Linux setup guide](../how-to/setup-linux.md) covers host restrictions.

## 2. Connect your agent

Add this server to your MCP client's configuration, replacing `/absolute/path/glass` with your
checkout's absolute path:

```json
{
  "mcpServers": {
    "glass": {
      "command": "/absolute/path/glass/target/debug/glass-mcp",
      "args": ["--tool-profile", "lean"]
    }
  }
}
```

Reload the client's MCP connection. Ask your agent to list the Glass tools; you should see
`glass_start`, `glass_screenshot`, `glass_do` and `glass_logs`.
[Connect your agent](../how-to/connect-an-agent.md) gives client-specific configuration steps.

The examples below name an MCP tool followed by its JSON arguments. Ask your agent to make these
calls; they are not shell commands. Only selected result fields are shown. Actual replies include
an `ok`/`tool`/`result` envelope and mark app-derived content as untrusted data.

## 3. Build and launch the test app

Run your agent from this checkout. Ask it to use Glass to build and launch `glass-testapp`:

```jsonc
glass_start {
  "build": "cargo build --locked -p glass-testapp",
  "run": ["target/debug/glass-testapp"],
  "backend": "x11",
  "a11y": false
}
```

Glass runs the build, starts a private Xvfb display and launches the app under default containment.
The result reports `width:320` and `height:240`. The build runs on the host with your toolchain;
containment applies to the launched app.

The fixture has no accessibility tree, so this launch explicitly skips the private accessibility
bus. Ordinary launches enable it by default.

## 4. See the window and save a baseline

Wait for the initial paint, then capture the window:

```jsonc
glass_do { "actions": [{ "action": "settle", "timeout_ms": 2000 }] }
glass_screenshot {}
```

You should see a 320×240 image: red at the top left, green at the top right, blue at the bottom
left and white at the bottom right. Check that the settle step reports `settled:true`.

Save this image for comparison:

```jsonc
glass_baseline_save { "name": "before-input" }
```

## 5. Send input and verify its delivery

Click inside the red quadrant and press the letter `a`:

```jsonc
glass_do {
  "actions": [
    { "action": "click", "x": 80, "y": 60 },
    { "action": "key", "chord": "a" }
  ]
}
```

`ok:true` means both steps completed. To check that the application actually received the click,
wait for its log entry:

```jsonc
glass_logs {
  "contains": "EVENT button=1 x=80 y=60",
  "cursor": 0,
  "wait": { "timeout_ms": 3000 }
}
```

The result reports `matched:true`, with the event in a separate untrusted text block. Read the key
event too:

```jsonc
glass_logs { "contains": "EVENT keysym=97", "cursor": 0 }
```

`97` is the X11 keysym for lowercase `a`. The fixture logs input without changing its drawing.
The app's own output confirms delivery.

## 6. Compare the pixels

```jsonc
glass_diff { "name": "before-input", "mode": "exact" }
```

Expect `changed_pixels:0` and `bbox:null`: receiving input did not change the quadrants. This result
uses text only. A pixel comparison answers whether the picture changed; the log answered whether
this app received the input.

## 7. Stop the app

```jsonc
glass_stop {}
```

Glass closes the app and tears down its private display. Your agent has built, seen, driven and
verified an application through MCP.

To try a code-fixing exercise next, use the [GTK Tasks demo](../../examples/tasks-demo/README.md).
For named controls in your own app, see [semantic targeting](../reference/tools.md#selector-action-contract)
and the optional [glass-drive skill](../how-to/drive-glass-well.md).
