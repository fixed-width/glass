import SwiftUI
import UIKit
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
        WindowGroup {
            if ProcessInfo.processInfo.arguments.contains("--occlusion") {
                OcclusionContentView().ignoresSafeArea()
            } else {
                ContentView()
            }
        }
    }
}

struct OcclusionContentView: UIViewRepresentable {
    func makeUIView(context: Context) -> OcclusionControls { OcclusionControls() }
    func updateUIView(_ view: OcclusionControls, context: Context) {}
}

final class OcclusionControls: UIView {
    private let counters = UILabel()
    private let cover = UIButton(type: .system)
    private var targetCount = 0
    private var coverCount = 0
    private var positiveCount = 0

    init() {
        super.init(frame: .zero)
        backgroundColor = .systemBackground
        counters.frame = CGRect(x: 40, y: 90, width: 330, height: 40)
        counters.accessibilityIdentifier = "occlusionCounters"
        addSubview(counters)

        let target = UIButton(type: .system)
        target.frame = CGRect(x: 40, y: 180, width: 240, height: 50)
        target.setTitle("Covered", for: .normal)
        target.accessibilityIdentifier = "coveredButton"
        target.backgroundColor = .systemYellow
        target.addTarget(self, action: #selector(onTarget), for: .touchUpInside)
        addSubview(target)

        cover.frame = ProcessInfo.processInfo.arguments.contains("--occlusion-distinct")
            ? CGRect(x: 120, y: 180, width: 80, height: 50) : target.frame
        cover.setTitle("Cover", for: .normal)
        cover.accessibilityIdentifier = "coverButton"
        cover.backgroundColor = .systemBlue
        cover.addTarget(self, action: #selector(onCover), for: .touchUpInside)
        addSubview(cover)

        let positive = UIButton(type: .system)
        positive.frame = CGRect(x: 40, y: 280, width: 240, height: 50)
        positive.setTitle("Positive / remove cover", for: .normal)
        positive.accessibilityIdentifier = "positiveButton"
        positive.addTarget(self, action: #selector(onPositive), for: .touchUpInside)
        addSubview(positive)
        updateCounters()
    }

    required init?(coder: NSCoder) { fatalError("init(coder:) has not been implemented") }
    private func updateCounters() {
        counters.text = "target=\(targetCount) cover=\(coverCount) positive=\(positiveCount)"
    }
    @objc private func onTarget() { targetCount += 1; updateCounters() }
    @objc private func onCover() { coverCount += 1; updateCounters() }
    @objc private func onPositive() {
        positiveCount += 1
        cover.isHidden = true
        updateCounters()
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
