# Set up glass on Linux

Linux has two backends — **X11** (the default) and **Wayland** (a headless sway). Both spawn their own
private headless display, so there is no desktop or window manager to configure. This guide covers the
prerequisites and the per-backend specifics; for *why* the display model works this way, see
[explanation/backends.md](../explanation/backends.md).

## Install the binary

Download the latest Linux build from the
[Releases page](https://github.com/fixed-width/glass/releases/latest) and extract it:

```bash
tar xzf glass-mcp-*-x86_64-linux-gnu.tar.gz
cd glass-mcp-*-x86_64-linux-gnu
```

Use the `…-x86_64-linux-musl.tar.gz` build instead if you need a fully static binary with no glibc
dependency — Alpine, or any musl distro. If you are on an architecture with no published asset (an
aarch64 host, say), [build from source](build-from-source.md) instead; that is also the path if you
want to hack on glass. The full asset list is in
[reference/platforms.md](../reference/platforms.md#release-artifacts).

## Prerequisites

glass needs a display dependency for your backend and a containment runtime, both covered below.
Nothing else — the released binary is self-contained.

## X11 (the default)

Install the headless X server:

```bash
sudo apt-get install -y xvfb
# Fedora: sudo dnf install xorg-x11-server-Xvfb   ·   Arch: sudo pacman -S xorg-server-xvfb
```

glass spawns its own private display, so `Xvfb` is all you need. The X11 backend takes its display
from `GLASS_DISPLAY` (never the ambient `$DISPLAY`):

- **unset (default)** — glass spawns a private headless `Xvfb` on a free display, logs the number to
  stderr, and tears it down on exit. Override the size with `GLASS_XVFB_SCREEN` (default
  `1280x800x24`).
- **`:42`** — attach to a display you manage (see below).
- **`:0`** — deliberately drive your real desktop. Only happens when you ask for it explicitly.

To watch the default headless display live, point a VNC viewer at the logged number:
`x11vnc -display :N` plus any VNC viewer (or `Xephyr` for a window).

### Optional: a persistent display you control

To keep a VNC view pinned across server restarts, run your own `Xvfb` and set `GLASS_DISPLAY` to it. A
helper manages a sandbox `Xvfb` (defaults to `:42`; size via `GLASS_XVFB_SCREEN`):

```bash
./scripts/sandbox-xvfb.sh start      # also: status | stop | restart
```

Then register glass with `"env": { "GLASS_DISPLAY": ":42" }`. To survive logout, run the `Xvfb` at
login via a **systemd user service**:

```ini
# ~/.config/systemd/user/glass-xvfb.service
[Unit]
Description=glass sandbox Xvfb display :42
[Service]
ExecStart=/usr/bin/Xvfb :42 -screen 0 1280x800x24
Restart=on-failure
[Install]
WantedBy=default.target
```
```bash
systemctl --user daemon-reload
systemctl --user enable --now glass-xvfb.service
loginctl enable-linger "$USER"   # optional: keep it up without an active login
```
(Adjust the `Xvfb` path to `command -v Xvfb`.)

## Wayland (sway)

Select it per launch with `glass_start`'s `backend: "wayland"`, or default every launch with
`GLASS_BACKEND=wayland`. For each session glass spawns a **private headless
[sway](https://swaywm.org)** and runs the app inside it — nothing to set up or keep running, and it
works on **any** Linux host including GNOME and KDE.

It needs a discoverable **sway ≥ 1.12 / wlroots ≥ 0.20** — on `PATH`, or installed to
`~/.local/share/glass/sway/` by [sway-build](https://github.com/fixed-width/sway-build), or in a
`sway/` dir beside the `glass-mcp` binary — plus the host's Mesa software GL:

```bash
sudo apt-get install -y libegl1 libgl1-mesa-dri   # Debian/Ubuntu
```

Override the headless output size with `GLASS_WAYLAND_SCREEN` (default `1280x800`, format `WxH`). sway
also launches an Xwayland server, so X11-only apps run under this backend too.

## Containment runtime

glass sandboxes every launched app by default via
[bubblewrap](https://github.com/containers/bubblewrap), fail-closed (with no sandbox available,
`glass_start` errors rather than running the app unconfined — see
[explanation/containment.md](../explanation/containment.md)):

```bash
sudo apt-get install -y bubblewrap   # Fedora/Arch: bubblewrap
```

Bubblewrap also needs **unprivileged user namespaces**. Ubuntu 23.10+ restricts them via AppArmor;
allow with:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

Persist it by writing the setting to a file under `/etc/sysctl.d/`. Or set `GLASS_SANDBOX=off` to run
apps unconfined (no bubblewrap required).

## Verify

```bash
glass-mcp doctor          # per-check ✓/⚠/✗ with remedies; exits non-zero if the default backend can't run
```

`doctor` checks both bubblewrap and user-namespace availability and prints the exact remedy for
anything missing. Then [connect your agent](connect-an-agent.md).

## Android

The Android backend runs from a Linux host too — see [setup-android.md](setup-android.md) (it's
host-OS-agnostic; use the `~/android-sdk`-style paths shown there).
