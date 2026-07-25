import SwiftUI
import os

private let log = Logger(subsystem: "tech.fixedwidth.glassfixture", category: "fixture")

/// The marker the app emits at its earliest launch point. `startup_log_integration`
/// asserts this reaches `drain_logs()`, which only holds if the backend attaches the
/// unified-log stream *before* `simctl launch` — see that test for the race it covers.
private let startupMarker = "GLASS_FIXTURE_LAUNCHED"

@main
struct GlassFixtureApp: App {
    init() {
        // At `App.init`, not `onAppear`: the line must be emitted before the first frame.
        log.notice("\(startupMarker, privacy: .public)")
        print(startupMarker)
    }

    var body: some Scene {
        WindowGroup { ContentView() }
    }
}

/// Four elements with stable accessibility identifiers, each verifiable from a snapshot
/// alone: `statusLabel` (READY, flips to TAPPED), `tapButton`, `inputField`, and
/// `echoLabel` (mirrors the field, or `(empty)`). glass surfaces an iOS element's
/// identifier as its name, so the identifier is how a test addresses each one.
struct ContentView: View {
    @State private var tapped = false
    @State private var text = ""

    var body: some View {
        VStack(spacing: 40) {
            Text(tapped ? "TAPPED" : "READY")
                .font(.system(size: 44, weight: .bold))
                .foregroundStyle(tapped ? .green : .primary)
                .accessibilityIdentifier("statusLabel")

            Button("Tap Me") { tapped.toggle() }
                .font(.title)
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("tapButton")

            TextField("type here", text: $text)
                .textFieldStyle(.roundedBorder)
                .font(.title2)
                .padding(.horizontal, 40)
                .accessibilityIdentifier("inputField")

            Text(text.isEmpty ? "(empty)" : text)
                .font(.title3)
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("echoLabel")
        }
        .padding()
    }
}
