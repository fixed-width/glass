import AppKit

// Read-only ownership fixture: no activation, input handler or Glass integration.
final class Fixture: NSObject, NSApplicationDelegate {
    var panels: [NSPanel] = []
    let scenario = CommandLine.arguments.dropFirst().first ?? "stable"

    func panel() -> NSPanel {
        let panel = NSPanel(
            contentRect: NSRect(x: 80, y: 80, width: 320, height: 180),
            styleMask: [.titled, .nonactivatingPanel], backing: .buffered, defer: false)
        panel.title = "Read-only target fixture"
        panel.setAccessibilityIdentifier("target-window")
        panel.isReleasedWhenClosed = false
        let button = NSButton(frame: NSRect(x: 30, y: 70, width: 160, height: 40))
        button.title = "Observed control"
        button.setAccessibilityIdentifier("observed-button")
        panel.contentView?.addSubview(button)
        panel.orderFront(nil)
        return panel
    }

    func applicationDidFinishLaunching(_ notification: Notification) {
        panels = [panel()]
        if scenario == "compete" {
            panels.append(panel())
        } else if scenario == "replace" || scenario == "exit" {
            DispatchQueue.main.asyncAfter(deadline: .now() + 5) { [self] in
                if scenario == "exit" {
                    NSApplication.shared.terminate(nil)
                } else {
                    panels[0].close()
                    panels = [panel()]
                }
            }
        }
        // Bound orphan lifetime even if the inspector crashes.
        DispatchQueue.main.asyncAfter(deadline: .now() + 20) {
            NSApplication.shared.terminate(nil)
        }
    }
}

let app = NSApplication.shared
let fixture = Fixture()
app.setActivationPolicy(.accessory)
app.delegate = fixture
app.run()
