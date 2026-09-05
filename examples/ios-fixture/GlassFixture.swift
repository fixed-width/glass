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

/// Controls with stable accessibility identifiers and snapshot-readable ground truth. glass
/// surfaces an iOS element's identifier as its name, so the identifier is how a test addresses
/// each one.
struct ContentView: View {
    @State private var tapped = false
    @State private var text = ""
    @State private var saveCount = 0
    @State private var movingCount = 0
    @State private var movingOffset: CGFloat = 0

    private var status: String {
        if saveCount > 0 || movingCount > 0 {
            return "SAVED:\(saveCount) MOVED:\(movingCount)"
        }
        return tapped ? "TAPPED" : "READY"
    }

    var body: some View {
        VStack(spacing: 20) {
            Text(status)
                .font(.system(size: 44, weight: .bold))
                .foregroundStyle(status == "READY" ? Color.primary : Color.green)
                .accessibilityIdentifier("statusLabel")

            Button("Tap Me") { tapped.toggle() }
                .font(.title)
                .buttonStyle(.borderedProminent)
                .accessibilityIdentifier("tapButton")

            TextField("type here", text: $text)
                .textFieldStyle(.roundedBorder)
                .font(.title2)
                .padding(.horizontal, 40)
                .textInputAutocapitalization(.never)
                .accessibilityIdentifier("inputField")

            Text(text.isEmpty ? "(empty)" : text)
                .font(.title3)
                .foregroundStyle(.secondary)
                .accessibilityIdentifier("echoLabel")

            Button("Semantic Save") {
                saveCount += 1
                withAnimation(.linear(duration: 0.3)) {
                    movingOffset = movingOffset == 0 ? 90 : 0
                }
            }
            .buttonStyle(.borderedProminent)
            .accessibilityIdentifier("semanticSave")

            Button("Disabled Semantic") {}
                .disabled(true)
                .accessibilityIdentifier("disabledSemantic")

            HStack {
                Button("Duplicate Left") {}
                    .accessibilityIdentifier("duplicateSemantic")
                Button("Duplicate Right") {}
                    .accessibilityIdentifier("duplicateSemantic")
            }

            Button("Moving Semantic") { movingCount += 1 }
                .offset(x: movingOffset)
                .accessibilityIdentifier("movingSemantic")
        }
        .padding()
    }
}
