# glass examples

Small apps for trying glass's build → see → interact → debug loop. Each lives in its own directory.

- [`tasks-demo/`](tasks-demo/) — a GTK4 to-do app with a one-line bug to find (Linux). Used by the
  [project README's debugging demo](../README.md#try-the-debugging-demo-linux).
- [`ios-greeter/`](ios-greeter/) — a tiny SwiftUI app driven in the iOS Simulator. See
  [Drive a native iOS app](../docs/how-to/drive-an-ios-app.md).
- [`ios-fixture/`](ios-fixture/) — the SwiftUI app the `glass-ios` on-box tests drive: four
  elements with stable accessibility identifiers, plus a launch-time log marker.
- [`android-role-fixture/`](android-role-fixture/) and
  [`ios-role-fixture/`](ios-role-fixture/) — one screen of stock platform controls each, one
  control per question about what that platform's accessibility vocabulary can express. They are
  the readings behind [docs/reference/a11y-roles.md](../docs/reference/a11y-roles.md); the Android
  one builds without Gradle.
- [`web-role-fixture/`](web-role-fixture/) — shared HTML controls for platform accessibility probes.
- [`interaction-fixture/`](interaction-fixture/) — deterministic web tasks for repeated interaction measurements.
- [`electron-interaction-fixture/`](electron-interaction-fixture/) — the packaged Electron form and native dialog.

For repeatable measurements rather than an agent-led demo, use the
[interaction runner](../tools/interaction-bench/README.md). The X11 tutorial uses
[`glass-testapp`](../crates/glass-testapp/) from the main Cargo workspace; the egui fixtures live
in the separately built [`glass-fixture-egui`](../crates/glass-fixture-egui/) project.
