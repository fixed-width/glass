# Backends and display isolation

glass presents the same tools on every platform, but *where* the app runs and *how* it is captured
differ underneath. That variation is hidden behind a `Platform` seam in `glass-core`: the core stays
platform-agnostic, and each backend maps the common operations — capture, input, window enumeration —
to one OS's mechanisms. This is why the MCP tools behave identically across backends and only the
setup differs.

## The backend is chosen per launch

The backend is selected **per `glass_start`**, via its `backend` argument. When omitted it falls back
to `GLASS_BACKEND`, then to the host default (`windows` on a Windows host, otherwise `x11`). Because
the choice is per-call, an agent can drive an X11 app and a Wayland app in the same session with no
server restart. The backend is built when the app is launched, so the server boots even on a host with
no display or compositor at all.

## The five backends

- **X11 (Linux)** — spawns its own private headless `Xvfb`, or attaches to a display you name.
- **Wayland (Linux)** — spawns a private headless `sway` (a wlroots compositor) per session. sway also
  launches an Xwayland server, so X11-only apps run under this backend too.
- **Windows** — drives the app on the interactive desktop (Windows.Graphics.Capture, SendInput, UI
  Automation), so it needs a logged-in session to render and capture.
- **Android** — drives a native app in an AVD emulator over `adb`; host-OS-agnostic, since it just
  shells out to `adb`. The VM *is* the sandbox.
- **macOS** — drives the logged-in Aqua session (ScreenCaptureKit capture, CGEvent input, AXUIElement
  windows), gated by macOS's privacy permissions.

The setup for each lives in the how-to guides; the support matrix is in
[reference/platforms.md](../reference/platforms.md).

## Display isolation: keeping the app off your desktop

A recurring goal is that the app glass drives should not land on *your* screen, fighting you for the
cursor. How well that is achieved is a property of the backend, and it is the sharpest difference
between them.

On **Linux**, isolation is the default and it is real: each session gets its **own** headless display
— a private `Xvfb` (X11) or a private `sway` (Wayland) that glass spawns and tears down. The app
renders into a framebuffer that only glass reads. Nothing appears on your desktop, and there is
nothing to set up or keep running.

This is why the X11 backend **never reads the ambient `$DISPLAY`**. It takes its display only from
`GLASS_DISPLAY`, so the environment you launch from can't accidentally aim glass at your live desktop.
Three cases follow from that one rule: unset means a fresh private `Xvfb`; `:N` attaches to a
persistent display you manage (handy for watching over VNC); and `:0` deliberately drives your real
desktop — which only ever happens because you asked for it by name.

On **Android**, the emulator provides the same isolation for free: glass can boot a headless AVD that
is entirely separate from your session.

On **Windows**, isolation is weaker. There is one interactive desktop, and even a virtual-display
driver only adds a monitor to *that* desktop rather than walling the app off — for full isolation you
run glass inside a VM. On **macOS**, glass drives the real Aqua session today; display isolation is
planned.

## The live-desktop non-goal on Wayland

On X11, pointing glass at your running desktop is a one-line opt-in (`GLASS_DISPLAY=:0`). The Wayland
analog — driving your existing live compositor session — is a deliberate **non-goal**. Wayland has no
equivalent ambient-display handle; reaching a live session means going through the XDG desktop portal,
which raises an interactive consent dialog. That is fundamentally unsuited to the unattended,
agent-driven use glass is built for, so the Wayland backend always spawns its own headless sway
instead. The upshot is a feature, not a limitation: because the app runs inside the compositor glass
spawns, the Wayland backend works on **any** Linux host — including GNOME and KDE desktops, where the
host's own compositor is simply irrelevant.
