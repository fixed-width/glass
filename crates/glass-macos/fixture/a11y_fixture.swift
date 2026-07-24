// a11y_fixture.swift — glass-macos accessibility-tree test fixture.
//
// A minimal Cocoa app whose window exposes a real NSAccessibility tree with four named
// controls, for the macOS a11y reader's on-box tests to drive by name:
//
//   - an NSButton titled "Save" — prints `SAVE_CLICKED` to stdout (flushed) when its action
//     fires, so both the bounds-agreement test (a real pointer click) and the native-invoke
//     test (AXPress) can grep the app's captured stdout for the same marker: AXPress on an
//     NSButton runs the identical target/action as a real click, so one marker line proves
//     both paths reach the real handler — no separate "invoke" marker needed.
//   - an NSButton checkbox labelled "Enable".
//   - an editable NSTextField labelled "Note", initial value "hello".
//   - a non-editable, non-interactive NSTextField label ("Status") — exposes no AXPress
//     action, so the native-invoke test can confirm it is rejected as `AxActionUnavailable`
//     rather than silently doing nothing.
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

        // `labelWithString:` configures a non-editable, non-selectable, borderless text
        // field — AppKit's standard "static label" idiom, which reports AX role
        // `AXStaticText` (glass's `AxRole::Label`) rather than `AXTextField`, and exposes no
        // `AXPress` action.
        let status = NSTextField(labelWithString: "ready")
        status.frame = NSRect(x: 20, y: 20, width: 120, height: 20)
        status.setAccessibilityLabel("Status")

        let contentView = NSView(frame: window.contentView!.bounds)
        contentView.addSubview(save)
        contentView.addSubview(enable)
        contentView.addSubview(note)
        contentView.addSubview(status)
        window.contentView = contentView
        window.title = "glass a11y fixture"
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
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
