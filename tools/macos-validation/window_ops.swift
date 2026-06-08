// window_ops.swift — glass macOS validation, step 4 (AXUIElement window ops).
//
// Proves window enumerate / move / resize / focus via the Accessibility API
// (AXUIElement) — the backend's window-management path. Reuses the Accessibility TCC
// consent granted for inject_input. Enumerates the target app's windows, then moves +
// resizes the first one and re-reads to confirm the change took.
//
//   Build: swiftc -O -parse-as-library window_ops.swift -o window_ops
//   Run:   ./window_ops <app-substring>
//          e.g. open -a TextEdit && ./window_ops TextEdit
//
// Requires Accessibility consent for the responsible process (Terminal).

import Foundation
import AppKit
import ApplicationServices

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write(Data((msg + "\n").utf8))
    exit(1)
}

func axCopy(_ el: AXUIElement, _ attr: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(el, attr as CFString, &v) == .success ? v : nil
}

func axWindows(_ app: AXUIElement) -> [AXUIElement] {
    guard let raw = axCopy(app, kAXWindowsAttribute as String) else { return [] }
    return (raw as? [AXUIElement]) ?? []
}

func axTitle(_ w: AXUIElement) -> String { (axCopy(w, kAXTitleAttribute as String) as? String) ?? "" }

func axPosition(_ w: AXUIElement) -> CGPoint {
    guard let v = axCopy(w, kAXPositionAttribute as String) else { return .zero }
    var p = CGPoint.zero; AXValueGetValue(v as! AXValue, .cgPoint, &p); return p
}
func axSize(_ w: AXUIElement) -> CGSize {
    guard let v = axCopy(w, kAXSizeAttribute as String) else { return .zero }
    var s = CGSize.zero; AXValueGetValue(v as! AXValue, .cgSize, &s); return s
}

func axSet(_ w: AXUIElement, _ attr: String, _ value: AXValue) -> Bool {
    AXUIElementSetAttributeValue(w, attr as CFString, value) == .success
}
func axSetPosition(_ w: AXUIElement, _ p: CGPoint) -> Bool {
    var p = p; let v = AXValueCreate(.cgPoint, &p)!; return axSet(w, kAXPositionAttribute as String, v)
}
func axSetSize(_ w: AXUIElement, _ s: CGSize) -> Bool {
    var s = s; let v = AXValueCreate(.cgSize, &s)!; return axSet(w, kAXSizeAttribute as String, v)
}

let args = CommandLine.arguments
guard args.count >= 2 else { fail("usage: window_ops <app-substring>") }
let needle = args[1].lowercased()

guard AXIsProcessTrusted() else {
    fail("AXIsProcessTrusted = false — grant Accessibility to Terminal and re-run.")
}

guard let running = NSWorkspace.shared.runningApplications.first(where: {
    ($0.localizedName ?? "").lowercased().contains(needle)
}) else { fail("no running app matching \"\(needle)\"") }

print("app: \(running.localizedName ?? "?") pid=\(running.processIdentifier)")
let axApp = AXUIElementCreateApplication(running.processIdentifier)

let wins = axWindows(axApp)
guard !wins.isEmpty else { fail("no AX windows for \(running.localizedName ?? "?")") }
print("enumerated \(wins.count) window(s):")
for (i, w) in wins.enumerated() {
    print("  [\(i)] \"\(axTitle(w))\"  pos=\(axPosition(w)) size=\(axSize(w))")
}

// Focus the app, then move + resize window 0 and confirm the change.
running.activate()
let w0 = wins[0]
let before = (axPosition(w0), axSize(w0))
let target = (CGPoint(x: 120, y: 120), CGSize(width: 640, height: 520))
let movedOK = axSetPosition(w0, target.0)
let sizedOK = axSetSize(w0, target.1)
usleep(200_000)
let after = (axPosition(w0), axSize(w0))

print("move: set=\(movedOK) \(before.0) -> \(after.0) (target \(target.0))")
print("resize: set=\(sizedOK) \(before.1) -> \(after.1) (target \(target.1))")

func close(_ a: CGFloat, _ b: CGFloat) -> Bool { abs(a - b) <= 4 }
let posOK = close(after.0.x, target.0.x) && close(after.0.y, target.0.y)
let sizeOK = close(after.1.width, target.1.width) && close(after.1.height, target.1.height)
if posOK && sizeOK {
    print("OK: window moved AND resized to target via AXUIElement")
    exit(0)
} else {
    print("PARTIAL/FAIL: pos matched=\(posOK) size matched=\(sizeOK)")
    exit(1)
}
