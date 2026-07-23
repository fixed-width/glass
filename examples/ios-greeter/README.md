# ios-greeter

A tiny SwiftUI app for driving glass in the iOS Simulator: a name field, a **Greet** button, and a
label that updates to `Hello, <name>!`. The field and button carry accessibility identifiers
(`nameField`, `greetButton`) so an agent can address them by id; the label carries none, so its text
is what a snapshot shows — the value you verify after tapping Greet.

## Build

Requires the full Xcode and an iOS Simulator runtime (macOS only):

```bash
./build.sh   # → build/Greeter.app
```

Then follow [Drive a native iOS app in the Simulator](../../docs/how-to/drive-an-ios-app.md).
