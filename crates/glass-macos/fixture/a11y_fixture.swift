// a11y_fixture.swift — glass-macos accessibility-tree test fixture.
//
// A minimal Cocoa app whose window exposes a real NSAccessibility tree with known controls, for
// the macOS a11y reader's on-box tests to drive by name:
//
//   - an NSButton titled "Save" — prints `SAVE_CLICKED` to stdout (flushed) when its action
//     fires, so both the bounds-agreement test (a real pointer click) and the native-invoke
//     test (AXPress) can grep the app's captured stdout for the same marker: AXPress on an
//     NSButton runs the identical target/action as a real click, so one marker line proves
//     both paths reach the real handler — no separate "invoke" marker needed.
//   - an NSButton checkbox labelled "Enable".
//   - an NSButton titled "Bold" whose accessibility label differs from its title, so the
//     reader's `AXTitle` names it and its `AXDescription` becomes its description.
//   - an editable NSTextField labelled "Note", initial value "hello".
//   - a non-editable, non-interactive NSTextField label ("Status") — exposes no AXPress
//     action, so the native-invoke test can confirm it is rejected as `AxActionUnavailable`
//     rather than silently doing nothing.
//
// Every control sets an explicit `accessibilityLabel` so the reader can find it by name
// regardless of its visible title/string value — except "Bold", named by its title as above. That
// exception plus two `accessibilityHelp` settings are what pin the reader's secondary-label rule:
// "Save" carries help distinct from its name, and "Status" carries help identical to its name,
// which must be dropped rather than printed twice.
//
// Sibling to `quadrants.swift` (the capture/input fixture) in this same directory; kept separate
// because it exercises a different concern (accessibility-tree contents, not pixels or raw input
// events).
//
// Build: swiftc -parse-as-library a11y_fixture.swift -o a11y_fixture
//   (`-parse-as-library` is required because this file uses a top-level `@main` type
//   rather than unadorned top-level statements — the same gotcha documented in
//   quadrants.swift's build comment.)

import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {
    let window = NSWindow(
        contentRect: NSRect(x: 0, y: 0, width: 500, height: 440),
        styleMask: [.titled], backing: .buffered, defer: false)
    var movingSemantic: NSButton?
    var movementTicks = 0
    var movementTimer: Timer?
    var movementStep: CGFloat = 20

    func applicationDidFinishLaunching(_ notification: Notification) {
        let save = NSButton(title: "Save", target: self, action: #selector(onSave))
        save.frame = NSRect(x: 20, y: 140, width: 80, height: 32)
        save.setAccessibilityLabel("Save")
        // Distinct from the label, so the reader's `description` must come from `AXHelp`
        // specifically — a count of described nodes cannot say which attribute produced them.
        save.setAccessibilityHelp("Saves and closes the sheet")

        let enable = NSButton(checkboxWithTitle: "Enable", target: nil, action: nil)
        enable.frame = NSRect(x: 20, y: 100, width: 120, height: 24)
        enable.setAccessibilityLabel("Enable")

        // An NSSwitch beside the checkbox, because AppKit publishes the two differently: this one
        // reports AXButton with subrole AXSwitch (a SwiftUI switch reports AXCheckBox with the same
        // subrole), so the reader's switch mapping has both forms to be read against on box.
        let active = NSSwitch(frame: NSRect(x: 260, y: 100, width: 60, height: 24))
        active.state = .on
        active.setAccessibilityLabel("Active")

        // Title and label deliberately differ and no help text is set, so `AXTitle` supplies the
        // name and `AXDescription` is left to be the description — the reader's titled branch,
        // which no other control here exercises.
        let bold = NSButton(title: "Bold", target: nil, action: nil)
        bold.frame = NSRect(x: 160, y: 140, width: 80, height: 32)
        bold.setAccessibilityLabel("Bold text style")

        let note = NSTextField(string: "hello")
        note.frame = NSRect(x: 20, y: 60, width: 200, height: 24)
        note.setAccessibilityLabel("Note")

        // `labelWithString:` configures a non-editable, non-selectable, borderless text
        // field — AppKit's standard "static label" idiom, which reports AX role
        // `AXStaticText` (glass's `AxRole::Label`) rather than `AXTextField`, and exposes no
        // `AXPress` action.
        let status = NSTextField(labelWithString: "ready")
        status.frame = NSRect(x: 20, y: 20, width: 120, height: 20)
        status.setAccessibilityLabel("Status")
        // Help text identical to the label, so the reader must drop it rather than print the
        // same string twice on one outline line.
        status.setAccessibilityHelp("Status")

        let semanticSave = NSButton(
            title: "Semantic Save", target: self, action: #selector(onSemanticSave))
        semanticSave.frame = NSRect(x: 20, y: 380, width: 130, height: 32)
        semanticSave.setAccessibilityLabel("Semantic Save")

        let disabledSemantic = NSButton(
            title: "Disabled semantic", target: self, action: #selector(onDisabledSemantic))
        disabledSemantic.frame = NSRect(x: 170, y: 380, width: 150, height: 32)
        disabledSemantic.setAccessibilityLabel("Disabled semantic")
        disabledSemantic.isEnabled = false

        let duplicateOne = NSButton(
            title: "Duplicate semantic", target: self, action: #selector(onDuplicateSemantic))
        duplicateOne.frame = NSRect(x: 20, y: 340, width: 160, height: 32)
        duplicateOne.setAccessibilityLabel("Duplicate semantic")
        let duplicateTwo = NSButton(
            title: "Duplicate semantic", target: self, action: #selector(onDuplicateSemantic))
        duplicateTwo.frame = NSRect(x: 200, y: 340, width: 160, height: 32)
        duplicateTwo.setAccessibilityLabel("Duplicate semantic")

        let moving = NSButton(
            title: "Moving semantic", target: self, action: #selector(onMovingSemantic))
        moving.frame = NSRect(x: 20, y: 300, width: 150, height: 32)
        moving.setAccessibilityLabel("Moving semantic")
        movingSemantic = moving

        let occluded = NSButton(
            title: "Occluded semantic", target: self, action: #selector(onOccludedSemantic))
        occluded.frame = NSRect(x: 20, y: 250, width: 160, height: 32)
        occluded.setAccessibilityLabel("Occluded semantic")
        let occluder = NSButton(title: "Occluder", target: self, action: #selector(onOccluder))
        occluder.frame = NSRect(x: 60, y: 250, width: 90, height: 32)
        occluder.setAccessibilityLabel("Occluder")

        let contentView = NSView(frame: window.contentView!.bounds)
        contentView.addSubview(save)
        contentView.addSubview(enable)
        contentView.addSubview(active)
        contentView.addSubview(bold)
        contentView.addSubview(note)
        contentView.addSubview(status)
        contentView.addSubview(semanticSave)
        contentView.addSubview(disabledSemantic)
        contentView.addSubview(duplicateOne)
        contentView.addSubview(duplicateTwo)
        contentView.addSubview(moving)
        contentView.addSubview(occluded)
        contentView.addSubview(occluder)
        window.contentView = contentView
        window.title = "glass a11y fixture"
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
        window.makeFirstResponder(note)
        if let editor = note.currentEditor() as? NSTextView {
            editor.setSelectedRange(NSRange(location: note.stringValue.utf16.count, length: 0))
        }
        restartMovement()
    }

    func restartMovement() {
        movementTimer?.invalidate()
        movementTicks = 0
        if let moving = movingSemantic {
            let previousX = moving.frame.origin.x
            if previousX > 140 {
                moving.frame.origin.x = 180
                movementStep = -16
            } else {
                moving.frame.origin.x = 60
                movementStep = 20
            }
            if abs(previousX - moving.frame.origin.x) > 12 {
                emit("MOVING_RESET")
            }
        }
        let timer = Timer(
            timeInterval: 0.03,
            target: self,
            selector: #selector(moveSemantic),
            userInfo: nil,
            repeats: true)
        movementTimer = timer
        RunLoop.main.add(timer, forMode: .common)
    }

    // `print` is fully buffered (not line-buffered) once stdout is a pipe rather than a
    // TTY — and a later task's bounds-agreement test captures this app's stdout through a
    // pipe and greps it for the marker, so the write must be flushed explicitly or it may
    // never arrive before the test gives up. Do NOT reach for
    // `FileHandle.standardOutput.synchronizeFile()`: it calls `fsync(2)` under the hood,
    // which fails — and raises an uncatchable Objective-C exception, crashing the
    // process — on a non-seekable fd such as a pipe. `fflush(stdout)`, the convention
    // already used throughout `quadrants.swift`, is the safe equivalent here.
    @objc func onSave() {
        emit("SAVE_CLICKED")
    }

    func emit(_ marker: String) {
        print(marker)
        fflush(stdout)
    }

    @objc func onSemanticSave() {
        restartMovement()
        emit("SEMANTIC_SAVE")
    }
    @objc func onDisabledSemantic() { emit("DISABLED_SEMANTIC") }
    @objc func onDuplicateSemantic() { emit("DUPLICATE_SEMANTIC") }
    @objc func onMovingSemantic() {
        emit(movementTimer == nil ? "MOVING_CLICKED_SETTLED" : "MOVING_CLICKED_MOVING")
        emit("MOVING_CLICKED")
    }
    @objc func onOccludedSemantic() { emit("OCCLUDED_CLICKED") }
    @objc func onOccluder() { emit("OCCLUDER_CLICKED") }

    @objc func moveSemantic() {
        guard let moving = movingSemantic else { return }
        movementTicks += 1
        moving.frame.origin.x += movementStep
        if movementTicks == 1 {
            emit("MOVING_STARTED")
        }
        if movementTicks >= 10 {
            movementTimer?.invalidate()
            movementTimer = nil
            emit("MOVING_SETTLED")
        }
    }
}

@main
struct Main {
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let delegate = AppDelegate()
        app.delegate = delegate
        app.run()
    }
}
