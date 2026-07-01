// quadrants.swift — glass-macos capture + input fixture (Plan 2 Task 6, extended by Plan 3
// Task 5, extended again by Plan 4 Task 6).
//
// A minimal Cocoa app: a single 400x400 window whose entire content view paints 4 known
// solid colors into the window's four VISUAL quadrants — i.e. the quadrants as they
// appear on screen, not raw view-local coordinates. `NSView` is bottom-left-origin,
// y-up when unflipped (the default, and what this view uses): a rect drawn at view
// y:[200,400) appears at the TOP of the window on screen, and y:[0,200) at the BOTTOM.
// So, for the default (primary) window:
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
// stdout, flushing after each — used by the capture test only to confirm the process is
// alive, but not for pixel assertions.
//
// This fixture is shared by THREE integration tests: the Plan 2 capture test (relies on
// the 4-quadrant colors + exact 400x400 borderless window size), the Plan 3 input test
// (relies on the event reporting below), and the Plan 4 window test (relies on the
// SECOND window described next). `QuadrantView` is made key/first-responder so it
// actually receives keyboard and mouse events, and reports each one to stdout as a
// single flushed line so the driving test can assert on injected input landing
// correctly:
//
//   key: <characters>      — one line per keyDown, the event's `characters` string.
//   click: <x>,<y>         — one line per (left) mouseDown, in the content view's
//                             coordinate space converted to TOP-LEFT-origin pixels (the
//                             tool boundary's convention) — see `mouseDown` below for the
//                             flip. For a borderless 1x-backing-scale window, view points
//                             == window pixels.
//   scroll: <dx>,<dy>      — one line per scrollWheel, `scrollingDeltaX`/`scrollingDeltaY`
//                             verbatim (sign as macOS reports it), for verifying the
//                             scroll-wheel sign convention against the tool boundary.
//
// Both windows report through these same three prefixes (they're separate instances of
// the same `QuadrantView` class) — a deliberate simplification: only Plan 4 Task 6's
// window test opens a second window, and it never sends key/click/scroll input to
// either window, so there is no ambiguity to resolve today. A future multi-window input
// test would need per-window prefixes; out of scope here.
//
// ## Plan 4 Task 6: a second, optional window
//
// Pass `--windows 2` (an argv pair) or set `GLASS_FIXTURE_WINDOWS=2` (an env var) to
// additionally open a SECOND window, titled "glass-fixture-2", offset from the first by
// (120, 120) points, painted with a DIFFERENT, easily-distinguished 4-color palette
// (cyan/magenta/yellow/black instead of red/green/blue/white) — so a captured frame can
// be identified as "the second window" by its pixel colors alone, the same way the
// capture test identifies the first window's colors. The DEFAULT (no flag/env set)
// still opens exactly one window, titled "glass-fixture", identical in every respect to
// the pre-Plan-4 fixture — the capture/input tests never pass this flag, so they see no
// behavior change.
//
// The primary window is always created (or re-ordered) last and made key/main, so it is
// the frontmost window and therefore the one `SCShareableContent`'s window enumeration
// finds first for this app's pid — preserving `start_app`'s pre-Plan-4 "first window
// discovered becomes the active window" behavior even when a second window exists.
//
// Build: swiftc -O -parse-as-library quadrants.swift -o quadrants
//   (`-parse-as-library` is required because this file uses a top-level `@main` type
//   rather than unadorned top-level statements — the same gotcha as
//   glass/tools/macos-validation/capture_window.swift.)

import Cocoa

let windowSize = NSSize(width: 400, height: 400)

/// Where the second window (if any) is placed, relative to the primary window's `.zero`
/// origin — see the module doc's Plan 4 Task 6 section.
let secondWindowOffset = NSPoint(x: 120, y: 120)

/// How many fixture windows to open this run: 1 (the pre-Plan-4 default, unaffected) or
/// 2 (Plan 4 Task 6's window-integration test). Checks `--windows <n>` in argv first,
/// then `GLASS_FIXTURE_WINDOWS` in the environment; any other/missing value means 1.
/// `n < 1` is treated as 1 (a fixture with zero windows would never appear on-screen at
/// all, defeating every test that uses it).
func requestedWindowCount() -> Int {
    let args = CommandLine.arguments
    if let flagIndex = args.firstIndex(of: "--windows"), flagIndex + 1 < args.count,
        let n = Int(args[flagIndex + 1])
    {
        return max(1, n)
    }
    if let raw = ProcessInfo.processInfo.environment["GLASS_FIXTURE_WINDOWS"], let n = Int(raw) {
        return max(1, n)
    }
    return 1
}

/// A borderless window that can still become key/main — not needed for this task's pure
/// capture test, but keeps the fixture usable for Plan 3's input work without surprises.
final class FixtureWindow: NSWindow {
    override var canBecomeKey: Bool { true }
    override var canBecomeMain: Bool { true }
}

/// One window's 4-color quadrant palette, keyed the same way the class doc above lists
/// them (visual, on-screen corners).
struct QuadrantPalette {
    let topLeft: NSColor
    let topRight: NSColor
    let bottomLeft: NSColor
    let bottomRight: NSColor
}

/// The primary ("glass-fixture") window's palette — unchanged from the pre-Plan-4
/// fixture's hardcoded red/green/blue/white.
let primaryPalette = QuadrantPalette(
    topLeft: NSColor(deviceRed: 1, green: 0, blue: 0, alpha: 1), // red
    topRight: NSColor(deviceRed: 0, green: 1, blue: 0, alpha: 1), // green
    bottomLeft: NSColor(deviceRed: 0, green: 0, blue: 1, alpha: 1), // blue
    bottomRight: NSColor(deviceRed: 1, green: 1, blue: 1, alpha: 1) // white
)

/// The secondary ("glass-fixture-2") window's palette — deliberately a DIFFERENT set of
/// 4 colors (cyan/magenta/yellow/black) so a captured frame can be told apart from the
/// primary window's red/green/blue/white by pixel color alone (see the module doc).
let secondaryPalette = QuadrantPalette(
    topLeft: NSColor(deviceRed: 0, green: 1, blue: 1, alpha: 1), // cyan
    topRight: NSColor(deviceRed: 1, green: 0, blue: 1, alpha: 1), // magenta
    bottomLeft: NSColor(deviceRed: 1, green: 1, blue: 0, alpha: 1), // yellow
    bottomRight: NSColor(deviceRed: 0, green: 0, blue: 0, alpha: 1) // black
)

final class QuadrantView: NSView {
    private let palette: QuadrantPalette

    init(frame: NSRect, palette: QuadrantPalette) {
        self.palette = palette
        super.init(frame: frame)
    }

    @available(*, unavailable)
    required init?(coder: NSCoder) {
        fatalError("QuadrantView does not support NSCoder-based initialization")
    }

    // Required to become first responder at all — NSView defaults to false.
    override var acceptsFirstResponder: Bool { true }

    override func draw(_ dirtyRect: NSRect) {
        // Width and height halves are computed SEPARATELY (not one shared `half`, as the
        // pre-Plan-4 version of this method assumed): the original 400x400 window made
        // `bounds.width/2 == bounds.height/2` true by construction, but Plan 4 Task 6's
        // `WindowOp::Resize` can leave the view non-square (e.g. 550x300) — a single shared
        // `half` would draw the "top"/"bottom" pair too tall or too short by the width/height
        // mismatch, corrupting the quadrant boundaries the capture test's pixel sampling
        // relies on. Recomputed on every `draw(_:)` call (not cached) since `bounds` changes
        // whenever the window is resized (`NSWindow` keeps its `contentView`'s frame equal to
        // the window's content area on every resize — see the class doc above).
        let halfW = bounds.width / 2
        let halfH = bounds.height / 2
        let quadrants: [(NSRect, NSColor)] = [
            // view-local rect                                                color   visual quadrant
            (NSRect(x: 0, y: halfH, width: halfW, height: halfH), palette.topLeft), // top-left
            (NSRect(x: halfW, y: halfH, width: halfW, height: halfH), palette.topRight), // top-right
            (NSRect(x: 0, y: 0, width: halfW, height: halfH), palette.bottomLeft), // bottom-left
            (NSRect(x: halfW, y: 0, width: halfW, height: halfH), palette.bottomRight), // bottom-right
        ]
        for (rect, color) in quadrants {
            color.setFill()
            NSBezierPath(rect: rect).fill()
        }
    }

    // MARK: - Input reporting (Plan 3)

    override func keyDown(with event: NSEvent) {
        print("key: \(event.characters ?? "")")
        fflush(stdout)
    }

    override func mouseDown(with event: NSEvent) {
        // `locationInWindow` is in the window's coordinate space (bottom-left origin);
        // convert into this view's coordinate space (also bottom-left origin, since the
        // view is unflipped — see the quadrant-color comment above), then flip y to match
        // the tool boundary's top-left-origin pixel convention. For a borderless window at
        // 1x backing scale, view points == window pixels.
        let locationInView = convert(event.locationInWindow, from: nil)
        let x = Int(locationInView.x.rounded())
        let y = Int((bounds.height - locationInView.y).rounded())
        print("click: \(x),\(y)")
        fflush(stdout)
    }

    override func scrollWheel(with event: NSEvent) {
        // Reported verbatim (sign as macOS delivers it) so the driving test can check the
        // scroll-wheel sign convention against what the tool boundary sends.
        print("scroll: \(event.scrollingDeltaX),\(event.scrollingDeltaY)")
        fflush(stdout)
    }
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
    /// Strong refs to every open window — `NSWindow` does not otherwise keep itself alive,
    /// and Plan 4 Task 6 needs a second instance to survive alongside the first.
    var windows: [FixtureWindow] = []

    func applicationDidFinishLaunching(_ notification: Notification) {
        let count = requestedWindowCount()

        // Secondary window (if requested) is created/ordered FIRST, so the primary window
        // — created/ordered last, below — ends up frontmost. See the module doc's Plan 4
        // Task 6 section for why this ordering matters (SCShareableContent enumeration
        // order / start_app's "first window discovered becomes active" contract).
        if count >= 2 {
            let secondary = makeWindow(
                origin: secondWindowOffset,
                title: "glass-fixture-2",
                palette: secondaryPalette
            )
            windows.append(secondary)
            secondary.orderFront(nil)
            secondary.makeFirstResponder(secondary.contentView)
        }

        let primary = makeWindow(origin: .zero, title: "glass-fixture", palette: primaryPalette)
        windows.append(primary)
        primary.makeKeyAndOrderFront(nil)
        primary.makeFirstResponder(primary.contentView)

        NSApp.activate(ignoringOtherApps: true)
    }

    private func makeWindow(origin: NSPoint, title: String, palette: QuadrantPalette) -> FixtureWindow {
        let rect = NSRect(origin: origin, size: windowSize)
        let window = FixtureWindow(
            contentRect: rect,
            // `.resizable` (alongside `.borderless`) is required for `AXUIElementSetAttributeValue`
            // to accept `AXSize` at all — a non-resizable window rejects it with a generic
            // `kAXErrorFailure` (-25200), discovered running Plan 4 Task 6's granted window
            // test against a `[.borderless]`-only window. `.resizable` adds no visible chrome
            // to a borderless window (no title bar, so no extra frame reserved for it — see
            // the module doc's exact-400x400 rationale), so this doesn't affect the capture
            // test's pixel-exact quadrant math; it only permits AX/programmatic resize.
            styleMask: [.borderless, .resizable],
            backing: .buffered,
            defer: false
        )
        window.title = title
        window.isOpaque = true
        window.hasShadow = false
        window.level = .normal
        window.contentView = QuadrantView(frame: NSRect(origin: .zero, size: windowSize), palette: palette)
        return window
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
