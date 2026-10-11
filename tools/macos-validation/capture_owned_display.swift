// Captures only freshly launched children on a specified owned display. No input is posted.
import AppKit
import ApplicationServices
import CoreGraphics
import Darwin
import Foundation
import ImageIO
import ScreenCaptureKit
import UniformTypeIdentifiers

struct ProbeError: Error, CustomStringConvertible {
    let description: String
    init(_ description: String) { self.description = description }
}

func rectangle(_ value: CGRect) -> [Double] {
    [value.minX, value.minY, value.width, value.height]
}

func inventory() throws -> [[String: Any]] {
    var ids = [CGDirectDisplayID](repeating: 0, count: 64)
    var count: UInt32 = 0
    guard CGGetOnlineDisplayList(64, &ids, &count) == .success, count > 0, count < 64 else {
        throw ProbeError("display inventory unavailable")
    }
    return try ids.prefix(Int(count)).sorted().map { id in
        guard let mode = CGDisplayCopyDisplayMode(id) else { throw ProbeError("display mode unavailable") }
        return ["id": id, "bounds": rectangle(CGDisplayBounds(id)),
                "mode": [mode.width, mode.height, mode.pixelWidth, mode.pixelHeight],
                "refresh": mode.refreshRate, "mirrors": CGDisplayMirrorsDisplay(id)]
    }
}

@MainActor
func context() throws -> [String: Any] {
    guard let event = CGEvent(source: nil) else { throw ProbeError("cursor unavailable") }
    return ["main": CGMainDisplayID(), "displays": try inventory(),
            "foreground_pid": NSWorkspace.shared.frontmostApplication?.processIdentifier ?? 0,
            "cursor": [event.location.x, event.location.y]]
}

func attribute(_ element: AXUIElement, _ name: CFString) throws -> CFTypeRef {
    var value: CFTypeRef?
    let error = AXUIElementCopyAttributeValue(element, name, &value)
    guard error == .success, let value else { throw ProbeError("AX attribute failed: \(name), \(error.rawValue)") }
    return value
}

func frame(_ window: AXUIElement) throws -> CGRect {
    let position = try attribute(window, kAXPositionAttribute as CFString)
    let size = try attribute(window, kAXSizeAttribute as CFString)
    guard CFGetTypeID(position) == AXValueGetTypeID(), CFGetTypeID(size) == AXValueGetTypeID() else {
        throw ProbeError("AX frame types unavailable")
    }
    var point = CGPoint.zero
    var dimensions = CGSize.zero
    guard AXValueGetValue(position as! AXValue, .cgPoint, &point),
          AXValueGetValue(size as! AXValue, .cgSize, &dimensions) else { throw ProbeError("AX frame unavailable") }
    return CGRect(origin: point, size: dimensions)
}

func windowNumber(_ window: AXUIElement) throws -> CGWindowID {
    typealias ReadWindow = @convention(c) (AXUIElement, UnsafeMutablePointer<CGWindowID>) -> AXError
    guard let library = dlopen("/System/Library/Frameworks/ApplicationServices.framework/ApplicationServices", RTLD_NOW) else {
        throw ProbeError("ApplicationServices unavailable")
    }
    defer { dlclose(library) }
    guard let symbol = dlsym(library, "_AXUIElementGetWindow") else { throw ProbeError("AX window ID reader unavailable") }
    let read = unsafeBitCast(symbol, to: ReadWindow.self)
    var id: CGWindowID = 0
    guard read(window, &id) == .success, id != 0 else { throw ProbeError("AX window ID unavailable") }
    return id
}

func observedButtons(_ window: AXUIElement) throws -> [String] {
    var queue = [window]
    var identifiers = [String]()
    var visited = [AXUIElement]()
    while !queue.isEmpty {
        let node = queue.removeFirst()
        guard !visited.contains(where: { CFEqual($0, node) }) else { continue }
        visited.append(node)
        guard visited.count < 256 else { throw ProbeError("AX tree exceeded probe bound") }
        if (try attribute(node, kAXRoleAttribute as CFString)) as? String == "AXButton" {
            if let id = try? attribute(node, kAXIdentifierAttribute as CFString) as? String { identifiers.append(id) }
        }
        if let children = try? attribute(node, kAXChildrenAttribute as CFString) as? [AXUIElement] {
            queue.append(contentsOf: children)
        }
    }
    return identifiers
}

func writeImage(_ image: CGImage, _ path: URL) throws -> Int {
    guard let destination = CGImageDestinationCreateWithURL(path as CFURL, UTType.png.identifier as CFString, 1, nil) else {
        throw ProbeError("image destination unavailable")
    }
    CGImageDestinationAddImage(destination, image, nil)
    guard CGImageDestinationFinalize(destination) else { throw ProbeError("image write failed") }
    var pixels = [UInt8](repeating: 0, count: image.width * image.height * 4)
    return try pixels.withUnsafeMutableBytes { bytes in
        guard let canvas = CGContext(data: bytes.baseAddress, width: image.width, height: image.height,
            bitsPerComponent: 8, bytesPerRow: image.width * 4, space: CGColorSpaceCreateDeviceRGB(),
            bitmapInfo: CGImageAlphaInfo.premultipliedLast.rawValue) else { throw ProbeError("image decoder unavailable") }
        canvas.draw(image, in: CGRect(x: 0, y: 0, width: image.width, height: image.height))
        var minimum = 255, maximum = 0
        for y in stride(from: 0, to: image.height, by: max(1, image.height / 32)) {
            for x in stride(from: 0, to: image.width, by: max(1, image.width / 32)) {
                let offset = (y * image.width + x) * 4
                let luma = (Int(bytes[offset]) * 30 + Int(bytes[offset+1]) * 59 + Int(bytes[offset+2]) * 11) / 100
                minimum = min(minimum, luma)
                maximum = max(maximum, luma)
            }
        }
        guard maximum - minimum >= 8 else { throw ProbeError("owned window capture is uniform") }
        return maximum - minimum
    }
}

@MainActor
func exercise(_ binary: String, _ variant: String, _ display: CGDirectDisplayID, _ root: URL) async throws -> [String: Any] {
    let baseline = try context()
    let child = Process()
    child.executableURL = URL(fileURLWithPath: binary)
    child.arguments = ["development", "extra"]
    child.currentDirectoryURL = root
    child.environment = ProcessInfo.processInfo.environment.merging([
        "DEVELOPMENT_MARKER": "custom", "DYLD_LIBRARY_PATH": root.path
    ]) { _, supplied in supplied }
    let log = root.appendingPathComponent("\(variant)-app.log")
    _ = FileManager.default.createFile(atPath: log.path, contents: nil)
    let output = try FileHandle(forWritingTo: log)
    defer { try? output.close() }
    child.standardOutput = output
    child.standardError = output
    try child.run()
    defer { if child.isRunning { child.terminate() }; child.waitUntilExit() }
    let app = AXUIElementCreateApplication(child.processIdentifier)
    guard AXUIElementSetMessagingTimeout(app, 0.5) == .success else { throw ProbeError("AX timeout setup failed") }
    var selected: AXUIElement?
    for _ in 0..<50 {
        guard child.isRunning else { throw ProbeError("owned app exited before its window") }
        if let windows = try? attribute(app, kAXWindowsAttribute as CFString) as? [AXUIElement] {
            guard windows.count <= 1 else { throw ProbeError("owned app has competing windows") }
            if let window = windows.first { selected = window; break }
        }
        try await Task.sleep(for: .milliseconds(40))
    }
    guard let window = selected else { throw ProbeError("owned app window unavailable") }
    let id = try windowNumber(window)
    let original = try frame(window)
    let displayBounds = CGDisplayBounds(display)
    var point = CGPoint(x: displayBounds.minX + 40, y: displayBounds.minY + 40)
    guard let position = AXValueCreate(.cgPoint, &point),
          AXUIElementSetAttributeValue(window, kAXPositionAttribute as CFString, position) == .success else {
        throw ProbeError("owned window placement refused")
    }
    try await Task.sleep(for: .milliseconds(200))
    let placed = try frame(window)
    guard displayBounds.contains(placed), try windowNumber(window) == id else {
        throw ProbeError("owned window did not remain within the specified display")
    }
    let content = try await SCShareableContent.excludingDesktopWindows(true, onScreenWindowsOnly: true)
    guard content.displays.contains(where: { $0.displayID == display }),
          let candidate = content.windows.first(where: {
              $0.windowID == id && $0.owningApplication?.processID == child.processIdentifier
          }), candidate.isOnScreen, displayBounds.contains(candidate.frame) else {
        throw ProbeError("ScreenCaptureKit disagrees with owned placement/identity")
    }
    let identifiers = try observedButtons(window)
    let wanted = variant == "original" ? "observed-button" : "changed-custom-development-extra-\(root.path)"
    guard identifiers.contains(wanted) else { throw ProbeError("test-side changed UI/launch marker missing") }
    let filter = SCContentFilter(desktopIndependentWindow: candidate)
    let config = SCStreamConfiguration()
    config.width = Int(candidate.frame.width.rounded(.up))
    config.height = Int(candidate.frame.height.rounded(.up))
    config.showsCursor = false
    config.ignoreShadowsSingleWindow = true
    let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
    let range = try writeImage(image, root.appendingPathComponent("\(variant).png"))
    let after = try context()
    guard child.isRunning, try windowNumber(window) == id,
          NSDictionary(dictionary: baseline).isEqual(to: after) else {
        throw ProbeError("display/foreground/cursor context changed during owned app observation")
    }
    return ["variant": variant, "pid": child.processIdentifier, "window_id": id,
            "original_frame": rectangle(original), "placed_frame": rectangle(placed),
            "sck_frame": rectangle(candidate.frame), "display": display,
            "image_size": [image.width, image.height], "luminance_range": range,
            "observed_button_ids": identifiers, "context_preserved": true]
}

@main
struct Main {
    @MainActor
    static func main() async {
        let args = CommandLine.arguments
        guard args.count == 5, let display = CGDirectDisplayID(args[1]) else { exit(2) }
        _ = NSApplication.shared
        NSApplication.shared.setActivationPolicy(.accessory)
        let root = URL(fileURLWithPath: args[4]).standardizedFileURL.resolvingSymlinksInPath()
        var result: [String: Any] = ["input_posted": false, "permission_requested": false]
        var status: Int32 = 1
        do {
            guard AXIsProcessTrusted(), CGPreflightScreenCaptureAccess() else {
                throw ProbeError("existing Accessibility/Screen Recording grant unavailable")
            }
            let ids = try inventory().compactMap { $0["id"] as? CGDirectDisplayID }
            guard ids.contains(display), display != CGMainDisplayID() else { throw ProbeError("specified secondary display unavailable") }
            result["before"] = try context()
            result["apps"] = try await [exercise(args[2], "original", display, root),
                                         exercise(args[3], "rebuilt", display, root)]
            result["after"] = try context()
            result["passed"] = true
            status = 0
        } catch { result["passed"] = false; result["error"] = String(describing: error) }
        do {
            let data = try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys, .prettyPrinted])
            try data.write(to: root.appendingPathComponent("capture-result.json"), options: .atomic)
        } catch { FileHandle.standardError.write(Data("result write failed: \(error)\n".utf8)) }
        exit(status)
    }
}
