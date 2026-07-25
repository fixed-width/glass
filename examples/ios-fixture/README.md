# ios-fixture

The app the `glass-ios` on-box tests drive. Four SwiftUI elements, each verifiable from an
accessibility snapshot alone:

| Identifier | What it is | What it proves |
|---|---|---|
| `statusLabel` | Shows `READY`, flips to `TAPPED` | A tap landed on `tapButton`, not merely somewhere |
| `tapButton` | A button | The point→pixel scale chain maps a snapshot's bounds to a real touch |
| `inputField` | A text field | Typed text (raw keys, and the `set_value` clear-then-type) reaches the field |
| `echoLabel` | Mirrors the field, or `(empty)` | The field's value changed, read back from a fresh snapshot |

It also prints `GLASS_FIXTURE_LAUNCHED` from `App.init` — before the first frame — which is
what the launch-time log capture test asserts on.

## Build

Requires the full Xcode and an iOS Simulator runtime (macOS only):

```bash
./build.sh   # → build/GlassFixture.app
```

## Run the on-box tests against it

With a Simulator booted and `idb_companion` on `PATH`:

```bash
export GLASS_IOS_APP="$PWD/examples/ios-fixture/build/GlassFixture.app"
export GLASS_IOS_STARTUP_MARKER=GLASS_FIXTURE_LAUNCHED
cargo test -p glass-ios -- --ignored --test-threads=1
```

`GLASS_IOS_UDID` / `GLASS_IOS_DEVICE` / `GLASS_IDB_COMPANION` select the Simulator and the
companion binary the same way they do for `glass-mcp`; see
[Set up the iOS backend](../../docs/how-to/setup-ios.md).

For driving an app by hand instead, start with [`ios-greeter/`](../ios-greeter/) and
[Drive a native iOS app](../../docs/how-to/drive-an-ios-app.md).
