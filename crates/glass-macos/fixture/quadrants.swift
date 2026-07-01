// quadrants.swift — glass-macos capture fixture (Plan 2, Task 6).
//
// A minimal Cocoa app: a single 400x400 window whose entire content view paints 4 known
// solid colors into the window's four VISUAL quadrants — i.e. the quadrants as they
// appear on screen, not raw view-local coordinates. `NSView` is bottom-left-origin,
// y-up when unflipped (the default, and what this view uses): a rect drawn at view
// y:[200,400) appears at the TOP of the window on screen, and y:[0,200) at the BOTTOM.
// So:
//
//   visual top-left     (screen upper-left)  = red   #FF0000
//   visual top-right    (screen upper-right) = green #00FF00
//   visual bottom-left  (screen lower-left)  = blue  #0000FF
//   visual bottom-right (screen lower-right) = white #FFFFFF
//
// Colors are set via `NSColor(deviceRed:green:blue:alpha:)` (device RGB, no ColorSync
// matching) so the captured framebuffer bytes land as close as possible to the exact
// values above.
//
// The window is deliberately BORDERLESS (no title bar chrome): a titled window's full
// frame (what ScreenCaptureKit's per-window capture returns) would include a title-bar
// strip of a height that varies by macOS version, which would break exact-quadrant
// pixel math in the capture test. A borderless window's frame equals its content view
// exactly, so the captured frame is exactly this view's 400x400 (at whatever backing
// scale) with no chrome to account for. `window.title` is still set for identification
// in logs/debugging even though it isn't drawn as chrome.
//
// Also spawns a background thread that reads stdin lines and echoes `got: <line>` to
// stdout, flushing after each — unused by this task's capture test, but exercised by the
// future input plan (Plan 3) so the same fixture serves both.
//
// Build: swiftc -O -parse-as-library quadrants.swift -o quadrants
//   (`-parse-as-library` is required because this file uses a top-level `@main` type
//   rather than unadorned top-level statements — the same gotcha as
//   glass/tools/macos-validation/capture_window.swift.)

import Cocoa

let windowSize = NSSize(width: 400, height: 400)

/// A borderless window that can still become key/main — not needed for this task's pure
/// capture test, but keeps the fixture usable for Plan 3's input work without surprises.
final class FixtureWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

final class QuadrantView: NSView {
    override func draw(_ dirtyRect: NSRect) {
        let half = bounds.width / 2 // == bounds.height / 2 for a 400x400 view
        let quadrants: [(NSRect, NSColor)] = [
            // view-local rect                                            color   visual quadrant
            (NSRect(x: 0, y: half, width: half, height: half), red), // top-left
            (NSRect(x: half, y: half, width: half, height: half), green), // top-right
            (NSRect(x: 0, y: 0, width: half, height: half), blue), // bottom-left
            (NSRect(x: half, y: 0, width: half, height: half), white), // bottom-right
        ]
        for (rect, color) in quadrants {
            color.setFill()
            NSBezierPath(rect: rect).fill()
        }
    }

    private let red = NSColor(deviceRed: 1, green: 0, blue: 0, alpha: 1)
    private let green = NSColor(deviceRed: 0, green: 1, blue: 0, alpha: 1)
    private let blue = NSColor(deviceRed: 0, green: 0, blue: 1, alpha: 1)
    private let white = NSColor(deviceRed: 1, green: 1, blue: 1, alpha: 1)
}

/// Reads stdin lines on a background thread and echoes `got: <line>` to stdout — for
/// Plan 3's input-driving test, unused by Task 6's capture test.
enum StdinEcho {
    static func start() {
        Thread.detachNewThread {
            while let line = readLine(strippingNewline: true) {
                print("got: \(line)")
                fflush(stdout)
            }
        }
    }
}

final class AppDelegate: NSObject, NSApplicationDelegate {
    var window: FixtureWindow!

    func applicationDidFinishLaunching(_ notification: Notification) {
        let rect = NSRect(origin: .zero, size: windowSize)
        window = FixtureWindow(
            contentRect: rect,
            styleMask: [.borderless],
            backing: .buffered,
            defer: false
        )
        window.title = "glass-fixture"
        window.isOpaque = true
        window.hasShadow = false
        window.level = .normal
        window.contentView = QuadrantView(frame: rect)
        window.makeKeyAndOrderFront(nil)
        NSApp.activate(ignoringOtherApps: true)
    }
}

@main
struct Main {
    static func main() {
        let app = NSApplication.shared
        app.setActivationPolicy(.regular)
        let delegate = AppDelegate()
        app.delegate = delegate
        StdinEcho.start()
        app.run()
    }
}
