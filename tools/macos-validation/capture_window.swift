// capture_window.swift — glass macOS validation, step 2 (highest risk).
//
// Captures ONE window's pixels via ScreenCaptureKit's per-window filter
// (SCContentFilter desktopIndependentWindow) — the exact capture path in the macOS
// backend design spec. This is the path the macOS-Tahoe blank-frame bug breaks, so run
// it on Sequoia (15) first. Writes a PNG and flags a blank/uniform result.
//
//   Build: swiftc -O capture_window.swift -o capture_window
//   Run:   ./capture_window <window-title-or-app-substring> [out.png]
//          e.g.  open -a TextEdit && ./capture_window TextEdit shot.png
//
// Requires macOS 14+ (SCScreenshotManager). The FIRST run triggers the Screen Recording
// TCC prompt — grant it (over Screen Sharing if you're remote), then re-run. Window
// titles are themselves gated behind Screen Recording consent, so before granting,
// titles may be empty.

import Foundation
import AppKit
import ScreenCaptureKit
import CoreGraphics
import ImageIO
import UniformTypeIdentifiers

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write(Data((msg + "\n").utf8))
    exit(1)
}

func writePNG(_ image: CGImage, to path: String) {
    let url = URL(fileURLWithPath: path) as CFURL
    guard let dest = CGImageDestinationCreateWithURL(url, UTType.png.identifier as CFString, 1, nil) else {
        fail("CGImageDestinationCreateWithURL failed")
    }
    CGImageDestinationAddImage(dest, image, nil)
    if !CGImageDestinationFinalize(dest) { fail("PNG finalize failed") }
}

// Sample a 16x16 grid; if the luma spread is tiny, the frame is blank/uniform — the
// signature of the headless/Tahoe capture failure.
func isLikelyBlank(_ image: CGImage) -> Bool {
    let w = image.width, h = image.height
    guard w > 1, h > 1 else { return true }
    let bytesPerRow = w * 4
    var px = [UInt8](repeating: 0, count: bytesPerRow * h)
    let cs = CGColorSpaceCreateDeviceRGB()
    guard let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8,
                              bytesPerRow: bytesPerRow, space: cs,
                              bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else {
        return false
    }
    ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
    var lo = 255, hi = 0
    let steps = 16
    for sy in 0..<steps {
        for sx in 0..<steps {
            let x = (w - 1) * sx / (steps - 1)
            let y = (h - 1) * sy / (steps - 1)
            let i = y * bytesPerRow + x * 4
            let luma = (Int(px[i]) * 30 + Int(px[i + 1]) * 59 + Int(px[i + 2]) * 11) / 100
            lo = min(lo, luma); hi = max(hi, luma)
        }
    }
    return (hi - lo) < 8
}

@main
struct Main {
    static func main() async {
        // A bare CLI tool has no connection to the window server, so ScreenCaptureKit's
        // capture path aborts with `CGS_REQUIRE_INIT` (did_initialize == false).
        // Touching NSApplication.shared creates the shared app instance, which
        // establishes that connection without turning us into a full GUI app.
        _ = NSApplication.shared

        let args = CommandLine.arguments
        guard args.count >= 2 else { fail("usage: capture_window <window-title-or-app-substring> [out.png]") }
        let needle = args[1].lowercased()
        let outPath = args.count >= 3 ? args[2] : "capture.png"

        let content: SCShareableContent
        do {
            content = try await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true)
        } catch {
            fail("SCShareableContent failed — grant Screen Recording and re-run. (\(error))")
        }

        let match = content.windows.first { w in
            guard w.isOnScreen else { return false }
            let title = (w.title ?? "").lowercased()
            let app = (w.owningApplication?.applicationName ?? "").lowercased()
            return title.contains(needle) || app.contains(needle)
        }
        guard let window = match else {
            let avail = content.windows.compactMap { w -> String? in
                guard w.isOnScreen, let t = w.title, !t.isEmpty else { return nil }
                return "  \(w.owningApplication?.applicationName ?? "?"): \(t)"
            }
            fail("no on-screen window matching \"\(needle)\". On-screen windows:\n" + avail.joined(separator: "\n"))
        }

        print("matched: \(window.owningApplication?.applicationName ?? "?") — \"\(window.title ?? "")\"  frame=\(window.frame)")

        let filter = SCContentFilter(desktopIndependentWindow: window)
        let config = SCStreamConfiguration()
        let scale = CGFloat(filter.pointPixelScale)
        config.width = max(1, Int(filter.contentRect.width * scale))
        config.height = max(1, Int(filter.contentRect.height * scale))
        config.showsCursor = false

        let image: CGImage
        do {
            image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
        } catch {
            fail("SCScreenshotManager.captureImage failed: \(error)")
        }

        if isLikelyBlank(image) {
            print("WARNING: captured image is blank/uniform (\(image.width)x\(image.height)) — this is the headless/Tahoe failure mode")
        } else {
            print("OK: captured non-blank \(image.width)x\(image.height) image")
        }
        writePNG(image, to: outPath)
        print("wrote \(outPath)")
        exit(0)
    }
}
