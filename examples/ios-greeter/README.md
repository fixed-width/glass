# ios-greeter

A tiny SwiftUI app for driving glass in the iOS Simulator: a name field, a **Greet** button, and a
label that updates to `Hello, <name>!`. Its controls carry accessibility identifiers (`nameField`,
`greetButton`, `greeting`) so an agent can drive and verify it semantically.

## Build

Requires the full Xcode and an iOS Simulator runtime (macOS only):

```bash
./build.sh   # → build/Greeter.app
```

Then follow [Drive a native iOS app in the Simulator](../../docs/how-to/drive-an-ios-app.md).
