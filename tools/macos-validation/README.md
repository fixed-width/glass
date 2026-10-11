# macOS backend validation kit

Standalone Objective-C/Swift probes for display lifecycle, capture and input. They require Xcode
Command Line Tools and a logged-in Aqua session. They do not enable a Glass backend feature.
Use the existing permission and signing procedure described in
[Verify a change](../../docs/how-to/verify-a-change.md); a successful compilation is not a permission grant.

## Bounded display and capture experiment

`display_lease.m` creates owned virtual displays through the private `CGVirtualDisplay` API and pumps
AppKit events until CoreGraphics and `NSScreen` both register them. With one fallback display, it
creates an owned primary at the same resolution followed by a secondary display. With one existing
nonfallback display, it creates only the secondary. Other initial topologies are outside this probe's
scope. The secondary is placed at the main display's lower-right corner using a session-only
configuration of that owned display. No existing display origin/mode is explicitly configured.

The probe prints JSON snapshots of display IDs, modes, bounds, mirroring, foreground, cursor and
window ownership/geometry without window titles. `ready` means the display resources are available;
inspect `foreign_windows_preserved` and the baseline snapshots separately. A fallback-primary
replacement can move existing windows even at the same resolution. This experiment records that
failure and continues measuring rendering; it does not certify desktop isolation or admit input.

Build both probes and the supplied sample apps **before** starting the display holder:

```bash
clang -fobjc-arc -Wall -Wextra -Werror -framework AppKit -framework CoreGraphics \
  display_lease.m -o /path/to/owned-task/display-lease
swiftc -warnings-as-errors -parse-as-library capture_owned_display.swift \
  -o /path/to/owned-task/capture-on-display
swiftc ../../crates/glass-macos/fixture/window_target.swift -o /path/to/owned-task/original
swiftc -D DEVELOPMENT_REBUILD ../../crates/glass-macos/fixture/window_target.swift \
  -o /path/to/owned-task/rebuilt
```

Create a fresh task directory for outputs. Run `display-lease /absolute/task/release` as a background
process and retain its PID. Wait for the JSON `ready` record and take `owned_secondary` from that
record; do not select an arbitrary non-main display. While that holder is alive, run through an
already-granted GUI context:

```bash
/path/to/owned-task/capture-on-display OWNED_SECONDARY_ID \
  /path/to/owned-task/original /path/to/owned-task/rebuilt /absolute/task/output-directory
```

The capture probe launches fresh children, preserving the inherited environment, inspects their sole
AX window, moves only those owned windows and cross-checks exact PID/window ID/geometry against
ScreenCaptureKit. It writes the two PNGs, application logs and `capture-result.json`. It observes
button identifiers without requiring specific identifiers or application identities; sample-specific
assertions belong to a separate test grader. These probes are diagnostics, not the production
application launcher: real integration must use Glass's existing build/run/log/containment pipeline.

The holder releases its displays when the release file appears or after 45 seconds, then checks list
membership during removal. Create the release file and wait for the holder to exit before auditing
cleanup. `display-lease --inspect` reads current inventory without creating a display. Verify every
owned ID is absent; an absent ID's `CGDisplayIsOnline` value is not a reliable membership test.
Check the final display inventory and existing window positions, including after failed trials.

The capture result reports rendering separately from context preservation across the whole sequence.
Neither a nonblank image nor unchanged observations within each individual capture proves unchanged
foreground during child startup/cleanup or between captures. No input is posted and no permission
request API is called. A capture API can still cause macOS UI or notifications; absence of permission
or notification dialogs is not established by these flags. Existing grants and unattended operation
must be verified for the actual Glass runtime.

Virtual displays remain in the same Aqua session; they do not create a private WindowServer or input
state. These experiments alone do not establish Linux-like desktop isolation. Current platform
capability matrices remain authoritative.

## Earlier standalone probes

- `virtual_display.m` creates one display and holds it until the process exits. It does not check
  preservation of the current display arrangement or demonstrate capture by itself.
- `capture_window.swift` captures a matching window. `capture_on_vdisplay.swift` additionally moves
  an existing matching application and activates it; use it only on a designated test desktop.
- `window_ops.swift` and `inject_input.swift` exercise the shared desktop and may affect its user.
- `validate_all.sh`, `probe_advanced.sh` and `run-validation.sh` are older manual validation recipes;
  inspect their launch/input/cleanup behavior before using them.

At a login window with no Aqua login, an API returning a display ID does not prove publication or a
capturable display. Check active/online lists and AppKit registration. Do not change login, encryption
or permission settings automatically to make a probe pass.
