// inject_input.swift — glass macOS validation, step 3 (CGEvent input).
//
// Proves mouse + keyboard injection via CGEvent/CGEventPost, focus-before-inject.
// Activates a target app, posts a left click at a window-relative point, then types a
// string. Pair with capture_window to confirm the app reacted (e.g. text appears in
// TextEdit). Keyboard injection needs Accessibility consent (TCC) for the responsible
// process (Terminal) — the FIRST run triggers that prompt; grant it and re-run.
//
//   Build: swiftc -O -parse-as-library inject_input.swift -o inject_input
//   Run:   ./inject_input <app-substring> "text to type"
//          e.g. open -a TextEdit && ./inject_input TextEdit "glass input ok"
//
// Requires macOS 14+. Coordinates are WINDOW-RELATIVE (0,0 = window top-left), mapped to
// global here — mirroring the backend's coordinate contract.

import Foundation
import AppKit
import CoreGraphics
import ScreenCaptureKit

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write(Data((msg + "\n").utf8))
    exit(1)
}

// Map an ASCII character to a (CGKeyCode, needsShift) on the US layout — enough for a
// validation string. The real backend uses a full keymap (glass_core::keys).
func keyCode(for ch: Character) -> (CGKeyCode, Bool)? {
    let map: [Character: CGKeyCode] = [
        "a":0,"s":1,"d":2,"f":3,"h":4,"g":5,"z":6,"x":7,"c":8,"v":9,"b":11,"q":12,"w":13,
        "e":14,"r":15,"y":16,"t":17,"1":18,"2":19,"3":20,"4":21,"6":22,"5":23,"=":24,"9":25,
        "7":26,"-":27,"8":28,"0":29,"]":30,"o":31,"u":32,"[":33,"i":34,"p":35,"l":37,"j":38,
        "k":40,"n":45,"m":46,".":47," ":49,
    ]
    if let k = map[ch] { return (k, false) }
    let lower = Character(ch.lowercased())
    if let k = map[lower] { return (k, true) }   // uppercase → shift
    return nil
}

func postKey(_ code: CGKeyCode, shift: Bool, down: Bool) {
    let src = CGEventSource(stateID: .hidSystemState)
    guard let ev = CGEvent(keyboardEventSource: src, virtualKey: code, keyDown: down) else { return }
    if shift { ev.flags = .maskShift }
    ev.post(tap: .cghidEventTap)
}

func typeString(_ s: String) {
    for ch in s {
        guard let (code, shift) = keyCode(for: ch) else {
            FileHandle.standardError.write(Data("  (skipping unmappable char: \(ch))\n".utf8))
            continue
        }
        postKey(code, shift: shift, down: true)
        postKey(code, shift: shift, down: false)
        usleep(12_000)
    }
}

func clickGlobal(_ p: CGPoint) {
    let src = CGEventSource(stateID: .hidSystemState)
    let down = CGEvent(mouseEventSource: src, mouseType: .leftMouseDown, mouseCursorPosition: p, mouseButton: .left)
    let up   = CGEvent(mouseEventSource: src, mouseType: .leftMouseUp,   mouseCursorPosition: p, mouseButton: .left)
    down?.post(tap: .cghidEventTap)
    usleep(30_000)
    up?.post(tap: .cghidEventTap)
}

@main
struct Main {
    static func main() async {
        _ = NSApplication.shared   // window-server connection (see capture_window.swift)
        let args = CommandLine.arguments
        guard args.count >= 3 else { fail("usage: inject_input <app-substring> \"text\"") }
        let needle = args[1].lowercased()
        let text = args[2]

        // Report Accessibility trust; prompt if absent (keyboard posting needs it).
        let trusted = AXIsProcessTrustedWithOptions(
            ["AXTrustedCheckOptionPrompt": true] as CFDictionary)
        print("AXIsProcessTrusted = \(trusted)")
        if !trusted {
            print("Grant Accessibility to Terminal (System Settings > Privacy & Security >")
            print("Accessibility), then re-run. Posting now would be silently dropped.")
        }

        // Find the window frame via ScreenCaptureKit (also tells us where to click).
        guard let content = try? await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true) else {
            fail("SCShareableContent failed — grant Screen Recording and re-run.")
        }
        guard let win = content.windows.first(where: { w in
            guard w.isOnScreen else { return false }
            let t = (w.title ?? "").lowercased(); let a = (w.owningApplication?.applicationName ?? "").lowercased()
            return t.contains(needle) || a.contains(needle)
        }) else { fail("no on-screen window matching \"\(needle)\"") }

        let f = win.frame
        print("target: \(win.owningApplication?.applicationName ?? "?") frame=\(f)")

        // Activate the owning app (focus-before-inject).
        if let pid = win.owningApplication?.processID,
           let app = NSRunningApplication(processIdentifier: pid) {
            app.activate()
            usleep(300_000)
        }

        // Click near the top-left of the content area (window-relative ~ (40,80)), mapped global.
        clickGlobal(CGPoint(x: f.origin.x + 40, y: f.origin.y + 80))
        usleep(150_000)

        print("typing: \"\(text)\"")
        typeString(text)
        print("done — capture the window to verify the text rendered.")
        exit(0)
    }
}
