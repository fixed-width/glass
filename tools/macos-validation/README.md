# macOS backend — validation kit

Throwaway, runnable scaffolding to **de-risk the glass macOS backend on a rented Mac
before buying hardware or writing the real backend**. Covers the two highest-risk steps
of the macOS backend validation plan:

1. **`virtual_display.m`** — create an off-screen display via the private
   `CGVirtualDisplay` API (proves a headless Mac can host a capturable display).
2. **`capture_window.swift`** — capture one window's pixels via ScreenCaptureKit
   (proves the per-window capture path returns real, non-blank pixels).

This is **validation code, not the backend** — the real backend is Rust (`objc2` FFI).
These are intentionally single-file ObjC/Swift so they build with just the Xcode Command
Line Tools and one command each.

## Prerequisites

- A Mac running **macOS 14+** — prefer **Sequoia (15)**, not Tahoe (26), for the first
  pass (Tahoe has unresolved capture quirks).
- `xcode-select --install` (gives `clang`, `swiftc`, the SDK).
- For headless/remote: Screen Sharing enabled, auto-login on, FileVault off,
  `caffeinate -dimsu &` running. See the validation plan for why.

## Build & run

```bash
# 1. Create the virtual display; leave this running (it holds the display open).
clang -fobjc-arc -framework Foundation -framework CoreGraphics -o virtualdisplay virtual_display.m
./virtualdisplay              # or ./virtualdisplay 1920 1080 [hidpi 0|1]
# -> expect a new id in the "after: N active display(s)" list
# NOTE: hiDPI=1 does NOT yield a 2x backing on Sequoia 15.6.1 (mode stays scale 1.0);
# for more pixels use a higher-res 1x mode, e.g. ./virtualdisplay 3840 2160 0

# 2. In another shell: launch a GUI app, then capture its window.
#    (-parse-as-library is required: a single-file Swift @main is otherwise
#    parsed as a script and fails to compile.)
swiftc -O -parse-as-library capture_window.swift -o capture_window
open -a TextEdit
./capture_window TextEdit shot.png
# -> first run prompts for Screen Recording (grant it, re-run); then expect
#    "OK: captured non-blank ..." and a real shot.png
```

## Must run inside a GUI (Aqua) session

Both tools need a **logged-in Aqua session** for the user — not just an SSH shell.
Over SSH at the login window, your launchd context is `Background`, the console user
is `_windowserver`, and `CGGetActiveDisplayList` returns **0** — so the virtual
display is created (the API returns an id) but never *attaches* (it stays out of the
active list), and capture has nothing to grab. Symptom: step 1 prints
`OK: created virtual display id=N` yet `after: 0 active display(s)`.

Fix: log into the GUI (Screen Sharing / your provider's VNC console) and run these
from a **Terminal inside that session** — that also puts the Screen Recording /
Accessibility consent prompts on a screen you can click. (To drive from SSH instead,
prefix with `sudo launchctl asuser <uid> …` to bootstrap into the Aqua domain.)

## What pass/fail looks like

- **Step 1 pass:** a new display id appears at the requested resolution with no monitor
  attached. **Fail:** compile error or no new display → the private `@interface` drifted
  on this macOS; cross-check the references named in `virtual_display.m` and adjust.
- **Step 2 pass:** `shot.png` shows the window's real content; tool prints
  `OK: captured non-blank …`. **Fail:** `WARNING: … blank/uniform` → the show-stopper
  (capture path not viable on this OS — note the version and stop).

Record outcomes in the validation plan's results table.

## Tahoe (macOS 26) re-validation — the buy gate

Items 1–5 PASS on **Sequoia 15.6.1** (see the validation plan's results table), but new
Mac minis ship **Tahoe (macOS 26)** and Apple Silicon can't downgrade below its shipping
OS — and Tahoe is exactly where the **item-2 capture blank-frame quirk** lives. So before
buying, re-run this kit on a **rented Tahoe instance** (Scaleway offers a macOS 26 image).
Item 2 is the make-or-break: non-blank → buy; blank/uniform → stop.

One-shot run for items 1–4 with a results summary:

```bash
# On the Tahoe box: get this repo onto it (clone with your git auth, or scp), then:
cd glass/tools/macos-validation

# Enable auto-login + VNC the same way as Sequoia (FileVault must be off):
./set_autologin.sh "$(id -un)"        # writes /etc/kcpassword from SSH (no GUI needed)
# ...then connect Scaleway's VNC and log in once, or reboot so auto-login brings up Aqua.

# From a Terminal INSIDE the VNC/Aqua session:
./validate_all.sh                      # builds all tools, runs items 1-4, prints a table
# First run flags Screen Recording (item 2) + Accessibility (item 3/4) consents — grant
# both to Terminal in System Settings > Privacy & Security, then re-run for an all-green
# pass. The "2 capture" line is the headline: PASS = non-blank, FAIL-BLANK = show-stopper.

# Item 5 (headless E2E): sudo reboot with VNC disconnected, reconnect SSH, confirm
# `launchctl print gui/$(id -u)` is live and `./virtualdisplay` still attaches.
```

## Advanced probes (beyond the 5-item plan)

`capture_on_vdisplay.swift` (+ runner `probe_advanced.sh`) tests what the basic kit didn't,
from a GUI Terminal: capturing a window that lives **on the virtual display** (the real
product topology — the basic capture ran on the base display), the **HiDPI/scale** signal,
and a **capture-latency baseline**:

```bash
./probe_advanced.sh TextEdit 30 2560x1440   # app, bench iters, virtual-display size
# -> moves TextEdit onto the virtual display, captures it there (writes
#    /tmp/shot_vdisplay.png), prints pointPixelScale + mean/p50/p95 capture latency.
```

## Caveat: the private interface drifts

`CGVirtualDisplay` is undocumented; the `@interface` in `virtual_display.m` mirrors the
public reverse-engineered headers but the field set changes across macOS releases. If it
doesn't compile or behave, copy the current declarations from:

- <https://github.com/enfp-dev-studio/node-mac-virtual-display> (MIT)
- <https://github.com/w0lfschild/macOS_headers>
- Chromium `ui/display/mac/test/virtual_display_mac_util.mm` (BSD)
