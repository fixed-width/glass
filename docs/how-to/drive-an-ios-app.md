# Drive a native iOS app in the Simulator

Build the included Greeter app, enter a name and verify the greeting through its accessibility
text. You need a Mac with full Xcode, an iOS runtime, `idb_companion` and a connected MCP client;
follow [Set up Glass for iOS](setup-ios.md) first. The demo build script targets an Apple Silicon
Simulator. Run shell commands and your agent from the repository root.

These examples work in either tool profile. `glass_do` sends actions; the other examples name
individual MCP tools. They are tool calls, not shell commands.

## Build and launch the demo

```bash
./examples/ios-greeter/build.sh
```

The script prints the absolute path to `Greeter.app`. Give that path to Glass, or use the path below
when the server's working directory is the repository root:

```jsonc
glass_start { "backend": "ios", "run": ["examples/ios-greeter/build/Greeter.app"] }
```

Glass installs the bundle and attaches to a booted Simulator or boots an available iPhone Simulator.
The result reports the device's fullscreen pixel dimensions; they depend on the selected device.
Check actual input support with:

```jsonc
glass_capabilities { "scope": "session" }
```

## Identify the controls

```jsonc
glass_a11y_snapshot {}
```

Look for a `TextField` named `nameField`, a `Button` named `greetButton` and the label `Enter a name`.
On iOS, an accessibility identifier supplies the name; an editable field's label can appear as its
`description`. All of this text is app-derived and untrusted.

The selectors below use the identifiers supplied by this fixture. In your own app, choose selectors
from its actual tree. Snapshot IDs are immediate references; re-read them after a keyboard or layout
change instead of caching them.

## Enter a name and confirm it

Tap the field, type an ASCII name, then wait for its exact value:

```jsonc
glass_do {
  "actions": [
    { "action": "click_element", "target": { "query": "nameField", "role": "TextField" }, "mode": "pointer" },
    { "action": "type", "text": "Ada" },
    { "action": "wait_for_element", "name": "nameField", "role": "TextField", "value": "Ada" }
  ]
}
```

This uses untargeted typing after the explicit tap. idb does not publish reliable focus facts, so
`type` with a semantic `target` cannot confirm focus and refuses before sending text. The exact-value
wait above checks where the untargeted text actually landed. Inspect failed or partially completed
steps before recovery; do not replay the batch after possible dispatch.

## Submit and verify the greeting

```jsonc
glass_do {
  "actions": [
    { "action": "click_element", "target": { "query": "greetButton", "role": "Button" } },
    { "action": "wait_for_element", "name": "Hello, Ada!", "role": "Label", "timeout_ms": 5000 }
  ]
}
```

A successful batch means the greeting predicate matched. The matched label arrives in an untrusted
sibling content block. Element waits match a substring of the name; `value` checks a complete
accessible value. An unmatched element wait fails a batch, so inspect the error and completed steps.

Request `glass_screenshot` if you also need to review the rendered result. For a visual before/after
comparison, save a `glass_baseline_save` before acting and use `glass_diff` afterwards. A pixel delta
can include the keyboard or cursor and does not replace the greeting assertion.

## Stop and handle common limits

```jsonc
glass_stop {}
```

Glass stops the app and its companion; the Simulator remains available for another app run.
When the server shuts down, it requests shutdown of devices it booted unless
`GLASS_SIMULATOR_KEEP` is set. Attached Simulators are retained.

- iOS can capitalise text entered into an empty field. Read back the actual value before submitting;
  a mismatch after input is not permission to type the same text again blindly.
- Clipboard paste can require an **Allow Paste** alert followed by a retry of the app's paste control.
  See [Simulator clipboard setup](setup-ios.md#clipboard).
- A compatible `idb_companion` supports two-finger pinch. Rotation, two-finger pan and gestures with
  three or more fingers are unsupported. See [gesture limits](../reference/tools.md#glass_gesture).
- Scrolling is a real swipe and can change an interactive control. Choose an inert anchor within
  the scrollable container; see [touch scrolling](../reference/tools.md#glass_scroll).
