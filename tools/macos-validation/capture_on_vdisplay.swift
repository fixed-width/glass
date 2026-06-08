// capture_on_vdisplay.swift — glass macOS validation, advanced probes (Sequoia baseline).
//
// Three things the basic kit never tested, all on a window placed ON the virtual display
// (the real product topology — the basic capture ran on the base display):
//   A. capture a window that lives on the CGVirtualDisplay, confirm non-blank
//   B. HiDPI/Retina: report pointPixelScale + captured pixel dims (run virtualdisplay with
//      hiDPI=1: `./virtualdisplay 1920 1080 1`). NOTE: on Sequoia 15.6.1 the `hiDPI` flag
//      alone does NOT yield a 2x backing (active mode reports scale 1.0 regardless of mode
//      dims) — so expect pointPixelScale=1.0. For more pixels, provision a higher-res 1x
//      display instead (e.g. `./virtualdisplay 3840 2160 0`).
//   C. capture latency baseline: time N consecutive SCK captures
//
//   Build: swiftc -O -parse-as-library capture_on_vdisplay.swift -o capture_on_vdisplay
//   Run:   ./virtualdisplay 1920 1080 1 &        # keep a (HiDPI) virtual display open
//          ./capture_on_vdisplay TextEdit 30     # 30 = bench iterations (0/omit to skip)
//
// Needs Screen Recording + Accessibility consent (granted to Terminal). Run from a GUI
// Terminal in the Aqua session. Moves the target window via AXUIElement onto the virtual
// display's global bounds, then captures it there.

import Foundation
import AppKit
import CoreGraphics
import ScreenCaptureKit
import ApplicationServices
import ImageIO
import UniformTypeIdentifiers

func fail(_ msg: String) -> Never {
    FileHandle.standardError.write(Data((msg + "\n").utf8)); exit(1)
}

func axCopy(_ el: AXUIElement, _ attr: String) -> AnyObject? {
    var v: AnyObject?
    return AXUIElementCopyAttributeValue(el, attr as CFString, &v) == .success ? v : nil
}
func axPosition(_ w: AXUIElement) -> CGPoint {
    guard let v = axCopy(w, kAXPositionAttribute as String) else { return .zero }
    var p = CGPoint.zero; AXValueGetValue(v as! AXValue, .cgPoint, &p); return p
}
func axSetPosition(_ w: AXUIElement, _ p: CGPoint) -> Bool {
    var p = p; let v = AXValueCreate(.cgPoint, &p)!
    return AXUIElementSetAttributeValue(w, kAXPositionAttribute as CFString, v) == .success
}

func writePNG(_ image: CGImage, to path: String) {
    guard let dest = CGImageDestinationCreateWithURL(URL(fileURLWithPath: path) as CFURL,
            UTType.png.identifier as CFString, 1, nil) else { return }
    CGImageDestinationAddImage(dest, image, nil); CGImageDestinationFinalize(dest)
}
func isLikelyBlank(_ image: CGImage) -> Bool {
    let w = image.width, h = image.height
    guard w > 1, h > 1 else { return true }
    let bpr = w * 4
    var px = [UInt8](repeating: 0, count: bpr * h)
    guard let ctx = CGContext(data: &px, width: w, height: h, bitsPerComponent: 8, bytesPerRow: bpr,
            space: CGColorSpaceCreateDeviceRGB(), bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue)
    else { return false }
    ctx.draw(image, in: CGRect(x: 0, y: 0, width: w, height: h))
    var lo = 255, hi = 0
    for sy in 0..<16 { for sx in 0..<16 {
        let x = (w - 1) * sx / 15, y = (h - 1) * sy / 15
        let i = y * bpr + x * 4
        let luma = (Int(px[i]) * 30 + Int(px[i+1]) * 59 + Int(px[i+2]) * 11) / 100
        lo = min(lo, luma); hi = max(hi, luma)
    } }
    return (hi - lo) < 8
}

@main
struct Main {
    static func main() async {
        _ = NSApplication.shared
        let args = CommandLine.arguments
        guard args.count >= 2 else { fail("usage: capture_on_vdisplay <app-substring> [bench-iters]") }
        let needle = args[1].lowercased()
        let benchIters = args.count >= 3 ? (Int(args[2]) ?? 0) : 0

        guard AXIsProcessTrusted() else { fail("Accessibility not granted — grant Terminal and re-run") }

        // Find the virtual display = the active display that isn't the main one.
        var count: UInt32 = 0
        CGGetActiveDisplayList(0, nil, &count)
        var ids = [CGDirectDisplayID](repeating: 0, count: Int(count))
        CGGetActiveDisplayList(count, &ids, &count)
        let mainID = CGMainDisplayID()
        guard let vdisp = ids.first(where: { $0 != mainID }) else {
            fail("only one active display — run `./virtualdisplay 1920 1080 1 &` first and keep it open")
        }
        let vb = CGDisplayBounds(vdisp)
        print("virtual display id=\(vdisp) bounds=\(vb)  (main id=\(mainID))")

        // Locate the target window.
        guard let content = try? await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true) else {
            fail("SCShareableContent failed — grant Screen Recording and re-run")
        }
        guard let win = content.windows.first(where: { w in
            guard w.isOnScreen else { return false }
            let t = (w.title ?? "").lowercased(); let a = (w.owningApplication?.applicationName ?? "").lowercased()
            return t.contains(needle) || a.contains(needle)
        }), let pid = win.owningApplication?.processID else { fail("no on-screen window matching \"\(needle)\"") }

        // A. Move the window onto the virtual display's global bounds via AX.
        NSRunningApplication(processIdentifier: pid)?.activate()
        usleep(200_000)
        let axApp = AXUIElementCreateApplication(pid)
        guard let axWin = (axCopy(axApp, kAXWindowsAttribute as String) as? [AXUIElement])?.first else {
            fail("no AX window for pid \(pid)")
        }
        _ = axSetPosition(axWin, CGPoint(x: vb.origin.x + 60, y: vb.origin.y + 60))
        usleep(400_000)
        let pos = axPosition(axWin)
        let onVDisplay = pos.x >= vb.minX && pos.x < vb.maxX && pos.y >= vb.minY && pos.y < vb.maxY
        print("moved window to \(pos) — on virtual display: \(onVDisplay ? "YES" : "NO")")

        // Re-query the window (its frame moved) and build the per-window filter.
        guard let content2 = try? await SCShareableContent.excludingDesktopWindows(false, onScreenWindowsOnly: true),
              let win2 = content2.windows.first(where: { $0.windowID == win.windowID }) else { fail("re-query failed") }
        let filter = SCContentFilter(desktopIndependentWindow: win2)
        let scale = CGFloat(filter.pointPixelScale)
        let cfg = SCStreamConfiguration()
        cfg.width = max(1, Int(filter.contentRect.width * scale))
        cfg.height = max(1, Int(filter.contentRect.height * scale))
        cfg.showsCursor = false

        // B. HiDPI signal.
        print("pointPixelScale=\(filter.pointPixelScale)  contentRect=\(filter.contentRect)  -> capture \(cfg.width)x\(cfg.height)")

        guard let img = try? await SCScreenshotManager.captureImage(contentFilter: filter, configuration: cfg) else {
            fail("captureImage failed on the virtual display")
        }
        if isLikelyBlank(img) {
            print("WARNING: capture on virtual display is BLANK/UNIFORM (\(img.width)x\(img.height))")
        } else {
            print("OK: non-blank \(img.width)x\(img.height) capture of a window on the virtual display")
        }
        writePNG(img, to: "/tmp/shot_vdisplay.png")
        print("wrote /tmp/shot_vdisplay.png")

        // C. Latency baseline.
        if benchIters > 0 {
            var ms: [Double] = []
            for _ in 0..<benchIters {
                let t0 = ProcessInfo.processInfo.systemUptime
                _ = try? await SCScreenshotManager.captureImage(contentFilter: filter, configuration: cfg)
                ms.append((ProcessInfo.processInfo.systemUptime - t0) * 1000)
            }
            ms.sort()
            let mean = ms.reduce(0, +) / Double(ms.count)
            let p50 = ms[ms.count / 2]
            let p95 = ms[min(ms.count - 1, Int(Double(ms.count) * 0.95))]
            print(String(format: "bench %d captures (%dx%d): mean=%.1fms p50=%.1fms p95=%.1fms min=%.1f max=%.1f",
                  benchIters, cfg.width, cfg.height, mean, p50, p95, ms.first!, ms.last!))
        }
        exit(0)
    }
}
