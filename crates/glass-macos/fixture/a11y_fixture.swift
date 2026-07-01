// a11y_fixture.swift — glass-macos accessibility-tree test fixture (Plan 6 Task 3).
//
// A minimal Cocoa app whose window exposes a real NSAccessibility tree with three named
// controls, for the macOS a11y reader's later on-box tests (Plan 6) to drive by name:
//
//   - an NSButton titled "Save" — prints `SAVE_CLICKED` to stdout (flushed) when clicked,
//     so a later bounds-agreement test can grep the app's captured stdout for the marker.
//   - an NSButton checkbox labelled "Enable".
//   - an editable NSTextField labelled "Note", initial value "hello".
//
// Each control sets an explicit `accessibilityLabel` so the reader can find it by name
// regardless of its visible title/string value. Sibling to `quadrants.swift` (the
// capture/input fixture) in this same directory; kept separate because it exercises a
// different concern (accessibility-tree contents, not pixels or raw input events).
//
// Build: swiftc -parse-as-library a11y_fixture.swift -o a11y_fixture
//   (`-parse-as-library` is required because this file uses a top-level `@main` type
//   rather than unadorned top-level statements — the same gotcha documented in
//   quadrants.swift's build comment.)

import AppKit

final class AppDelegate: NSObject, NSApplicationDelegate {
    let window = NSWindow(
        contentRect: NSRect(x: 0, y: 0, width: 400, height: 200),
        styleMask: [.titled], backing: .buffered, defer: false)

    func applicationDidFinishLaunching(_ notification: Notification) {
        let save = NSButton(title: "Save", target: self, action: #selector(onSave))
        save.frame = NSRect(x: 20, y: 140, width: 80, height: 32)
        save.setAccessibilityLabel("Save")

        let enable = NSButton(checkboxWithTitle: "Enable", target: nil, action: nil)
        enable.frame = NSRect(x: 20, y: 100, width: 120, height: 24)
        enable.setAccessibilityLabel("Enable")

        let note = NSTextField(string: "hello")
        note.frame = NSRect(x: 20, y: 60, width: 200, height: 24)
        note.setAccessibilityLabel("Note")

        let contentView = NSView(frame: window.contentView!.bounds)
        contentView.addSubview(save)
        contentView.addSubview(enable)
        contentView.addSubview(note)
        window.contentView = contentView
        window.title = "glass a11y fixture"
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }

    // `print` is fully buffered (not line-buffered) once stdout is a pipe rather than a
    // TTY — and a later task's bounds-agreement test captures this app's stdout through a
    // pipe and greps it for the marker, so the write must be flushed explicitly or it may
    // never arrive before the test gives up. The brief's original
    // `FileHandle.standardOutput.synchronizeFile()` calls `fsync(2)` under the hood,
    // which fails — and raises an uncatchable Objective-C exception, crashing the
    // process — on a non-seekable fd such as a pipe. `fflush(stdout)`, the convention
    // already used throughout `quadrants.swift`, is the safe equivalent here.
    @objc func onSave() {
        print("SAVE_CLICKED")
        fflush(stdout)
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
