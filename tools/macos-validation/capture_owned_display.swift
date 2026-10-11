// Captures freshly launched children on a specified owned display without posting input.
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

// SAFETY: The lock serializes progress and child ownership, and only the winning finalizer cancels the timer.
final class ProbeLifetime: @unchecked Sendable {
    private let lock = NSLock()
    private var child: Process?
    private var finishing = false
    private var cleanupVerified = true
    private var progress: [String: Any] = ["input_posted": false, "permission_request_api_called": false]
    private let root: URL
    private var watchdog: DispatchSourceTimer?

    init(root: URL) { self.root = root }

    func startDeadline() {
        let timer = DispatchSource.makeTimerSource(queue: .global(qos: .userInitiated))
        timer.schedule(deadline: .now() + 25)
        timer.setEventHandler { [self] in
            finish(nil, status: 1, deadline: true)
        }
        watchdog = timer
        timer.resume()
    }

    func record(_ result: [String: Any]) {
        lock.lock()
        progress = result
        lock.unlock()
    }

    func launch(_ process: Process) throws {
        lock.lock()
        defer { lock.unlock() }
        guard !finishing, child == nil else { throw ProbeError("probe no longer accepts a child") }
        child = process
        do { try process.run() } catch { child = nil; throw error }
    }

    func stopChild() {
        lock.lock()
        defer { lock.unlock() }
        guard let process = child else { return }
        for signal in [SIGTERM, SIGKILL] {
            if !process.isRunning { break }
            kill(process.processIdentifier, signal)
            let end = ProcessInfo.processInfo.systemUptime + 2
            while process.isRunning && ProcessInfo.processInfo.systemUptime < end { usleep(10_000) }
        }
        if process.isRunning { cleanupVerified = false } else { child = nil }
    }

    func finish(_ supplied: [String: Any]?, status: Int32, deadline: Bool = false) -> Never {
        lock.lock()
        if finishing {
            lock.unlock()
            while true { pause() }
        }
        finishing = true
        var result = supplied ?? progress
        lock.unlock()
        watchdog?.cancel()
        stopChild()
        lock.lock()
        let cleaned = cleanupVerified
        result["child_cleanup_verified"] = cleaned
        lock.unlock()
        result["deadline_exceeded"] = deadline
        if deadline {
            result["render_capture_passed"] = false
            result["error"] = "25-second capture deadline exceeded"
            result["final_context_unavailable"] = true
        }
        var finalStatus = cleaned ? status : 1
        do {
            let data = try JSONSerialization.data(withJSONObject: result, options: [.sortedKeys, .prettyPrinted])
            try data.write(to: root.appendingPathComponent("capture-result.json"), options: .atomic)
        } catch {
            FileHandle.standardError.write(Data("result write failed: \(error)\n".utf8))
            finalStatus = 1
        }
        exit(finalStatus)
    }
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
    // SAFETY: The loaded symbol has this C ABI and is called before its library handle is closed.
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
func exercise(_ binary: String, _ variant: String, _ display: CGDirectDisplayID, _ root: URL,
              _ lifetime: ProbeLifetime) async throws -> [String: Any] {
    let baseline = try context()
    let child = Process()
    child.executableURL = URL(fileURLWithPath: binary)
    child.arguments = []
    child.currentDirectoryURL = root
    child.environment = ProcessInfo.processInfo.environment
    let log = root.appendingPathComponent("\(variant)-app.log")
    _ = FileManager.default.createFile(atPath: log.path, contents: nil)
    let output = try FileHandle(forWritingTo: log)
    defer { try? output.close() }
    child.standardOutput = output
    child.standardError = output
    try lifetime.launch(child)
    defer { lifetime.stopChild() }
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
    let filter = SCContentFilter(desktopIndependentWindow: candidate)
    let config = SCStreamConfiguration()
    config.width = Int(candidate.frame.width.rounded(.up))
    config.height = Int(candidate.frame.height.rounded(.up))
    config.showsCursor = false
    config.ignoreShadowsSingleWindow = true
    let image = try await SCScreenshotManager.captureImage(contentFilter: filter, configuration: config)
    let range = try writeImage(image, root.appendingPathComponent("\(variant).png"))
    let after = try context()
    guard child.isRunning, try windowNumber(window) == id else { throw ProbeError("owned window identity changed during capture") }
    return ["variant": variant, "pid": child.processIdentifier, "window_id": id,
            "original_frame": rectangle(original), "placed_frame": rectangle(placed),
            "sck_frame": rectangle(candidate.frame), "display": display,
            "image_size": [image.width, image.height], "luminance_range": range,
            "observed_button_ids": identifiers, "context_preserved": NSDictionary(dictionary: baseline).isEqual(to: after),
            "context_before": baseline, "context_after": after]
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
        let lifetime = ProbeLifetime(root: root)
        lifetime.startDeadline()
        var result: [String: Any] = ["input_posted": false, "permission_request_api_called": false]
        var status: Int32 = 1
        do {
            guard AXIsProcessTrusted(), CGPreflightScreenCaptureAccess() else {
                throw ProbeError("existing Accessibility/Screen Recording grant unavailable")
            }
            let ids = try inventory().compactMap { $0["id"] as? CGDirectDisplayID }
            guard ids.contains(display), display != CGMainDisplayID() else { throw ProbeError("specified secondary display unavailable") }
            let before = try context()
            result["before"] = before
            lifetime.record(result)
            var apps = [[String: Any]]()
            for (binary, variant) in [(args[2], "original"), (args[3], "rebuilt")] {
                apps.append(try await exercise(binary, variant, display, root, lifetime))
                result["apps"] = apps
                lifetime.record(result)
            }
            let after = try context()
            result["after"] = after
            result["sequence_context_preserved"] = NSDictionary(dictionary: before).isEqual(to: after)
            result["render_capture_passed"] = true
            status = 0
        } catch {
            result["render_capture_passed"] = false
            result["error"] = String(describing: error)
            if let after = try? context() { result["after"] = after }
            else { result["final_context_unavailable"] = true }
        }
        lifetime.finish(result, status: status)
    }
}
