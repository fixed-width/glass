# glass examples

Small apps for trying glass's build → see → interact → debug loop. Each lives in its own directory.

- [`tasks-demo/`](tasks-demo/) — a GTK4 to-do app with a one-line bug to find (Linux). Used by the
  [project README's quickstart](../README.md#try-it-in-60-seconds).
- [`ios-greeter/`](ios-greeter/) — a tiny SwiftUI app driven in the iOS Simulator. See
  [Drive a native iOS app](../docs/how-to/drive-an-ios-app.md).
- [`ios-fixture/`](ios-fixture/) — the SwiftUI app the `glass-ios` on-box tests drive: four
  elements with stable accessibility identifiers, plus a launch-time log marker.
- [`android-role-fixture/`](android-role-fixture/) — stock `android.widget` controls, one per
  question about what Android's accessibility vocabulary can express. Builds without Gradle.
- [`ios-role-fixture/`](ios-role-fixture/) — the same for UIKit and SwiftUI. Both back the cells
  in [docs/reference/a11y-roles.md](../docs/reference/a11y-roles.md).
