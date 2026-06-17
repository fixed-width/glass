Running glass on a Linux host.

← [Back to README](../README.md)

## Prerequisites

### Rust

Install via [rustup](https://rustup.rs). glass pins a nightly toolchain in
`rust-toolchain.toml`; rustup installs it automatically on the first build.

### Display dependency

**X11 (default):** install the headless X server:

```bash
sudo apt-get install -y xvfb
```

(Fedora: `sudo dnf install xorg-x11-server-Xvfb`; Arch: `sudo pacman -S xorg-server-xvfb`.)

glass spawns its own private display, so Xvfb is all you need — no desktop or window
manager.

**Wayland:** a discoverable `sway ≥ 1.12` plus Mesa software GL:

```bash
sudo apt-get install -y libegl1 libgl1-mesa-dri   # Debian/Ubuntu
```

Most distros don't yet ship sway 1.12; build one with
[sway-build](https://github.com/fixed-width/sway-build) (`./build.sh && ./build.sh
install`, which installs to `~/.local/share/glass/sway/`).

### Containment runtime

glass **sandboxes every launched app by default** via
[bubblewrap](https://github.com/containers/bubblewrap), and that default is
*fail-closed*: with no sandbox available, `glass_start` errors rather than running the
app unconfined. Install bubblewrap:

```bash
sudo apt-get install -y bubblewrap   # Fedora/Arch: bubblewrap
```

Bubblewrap also needs **unprivileged user namespaces** enabled. Ubuntu 23.10+ restricts
them via AppArmor; allow with:

```bash
sudo sysctl -w kernel.apparmor_restrict_unprivileged_userns=0
```

Persist by writing the setting to a file under `/etc/sysctl.d/`. Alternatively, set
`GLASS_SANDBOX=off` to run apps unconfined (no bubblewrap required).

`glass-mcp doctor` checks both bubblewrap and user-namespace availability and prints the
exact remedy.

---

## Running on X11 (the default)

The X11 backend chooses its display from **`GLASS_DISPLAY`** — it never reads
ambient `$DISPLAY`, so the environment you launch from can't accidentally aim
glass at your live desktop:

- **`GLASS_DISPLAY` unset (default)** — glass spawns its **own private headless
  `Xvfb`** on a free display, logs the chosen number to stderr (`glass: spawned a
  private headless X11 display :N`), and tears it down on exit. Zero setup, fully
  isolated. Requires `Xvfb` installed (`sudo apt-get install -y xvfb`); override
  the size with `GLASS_XVFB_SCREEN` (default `1280x800x24`).
- **`GLASS_DISPLAY=:42`** (or bare `42`) — attach to a display *you* manage, e.g.
  a persistent sandbox you want to keep watching over VNC (see below).
- **`GLASS_DISPLAY=:0`** — deliberately drive your **real desktop**. The agent
  moves your actual cursor and pops real windows; useful for driving live apps,
  but it competes with you for input. This only happens when you ask for it
  explicitly.

To watch the default headless display live, point a VNC viewer at the logged
number: `x11vnc -display :N` + any VNC viewer (or `Xephyr` for a window).

### Optional: a persistent display you control

If you'd rather run your own display — to keep a VNC view pinned across server
restarts, say — start one and set `GLASS_DISPLAY` to it. A helper manages a
sandbox `Xvfb` (defaults to `:42`; override the number with `GLASS_DISPLAY`, the
size with `GLASS_XVFB_SCREEN`):

```bash
./scripts/sandbox-xvfb.sh start      # also: status | stop | restart
```

Then register glass with `"env": { "GLASS_DISPLAY": ":42" }`. Watch it with
`x11vnc -display :42` + any VNC viewer, or run a windowed `Xephyr :42`.

#### Make that display persistent (survive logout)

Run the `Xvfb` at login via a **systemd user service**:

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
(Adjust the `Xvfb` path to `command -v Xvfb`.) Or, for desktop-only autostart,
drop an equivalent `Exec=Xvfb :42 -screen 0 1280x800x24` into a
`~/.config/autostart/glass-xvfb.desktop` entry.

Requires `Xvfb` installed (`sudo apt-get install -y xvfb` on Debian/Ubuntu).

---

## Running on Wayland (sway)

Select it **per launch** with `glass_start`'s `backend: "wayland"`, or make it the
default for every launch with `GLASS_BACKEND=wayland` (e.g.
`"env": { "GLASS_BACKEND": "wayland" }` in the MCP config). Unlike X11, this
backend doesn't attach to an ambient display — for each session it spawns a
**private headless [`sway`](https://swaywm.org) instance** (sway is the
third-party wlroots-based Wayland compositor) and runs the target app inside it. The app's windows float at their natural size;
`glass_list_windows`/`glass_select_window` enumerate and switch between them over
sway IPC. Capture goes through `wlr-screencopy` of the active window's output
region, and input through the `wlr-virtual-pointer` and `zwp_virtual_keyboard`
protocols.

glass needs a **sway ≥ 1.12 / wlroots ≥ 0.20** it can discover (no env var): on
`PATH` (once your distro ships one that new), or installed to
`~/.local/share/glass/sway/` by the [sway-build](https://github.com/fixed-width/sway-build) tool, or in a
`sway/` dir beside the `glass-mcp` binary. It also needs the host's Mesa software GL so GPU-less hosts can
render:

```bash
sudo apt-get install -y libegl1 libgl1-mesa-dri   # Debian/Ubuntu
```

Because sway is headless and per-session, there's **nothing to set up or keep
running** — no persistent display, no `$DISPLAY`/`$WAYLAND_DISPLAY`. sway also
launches an Xwayland server, so X11-only apps run under this backend too.

Override the headless output size with **`GLASS_WAYLAND_SCREEN`** (default
`1280x800`, matching the X11 backend). This is the Wayland analog of X11's
`GLASS_XVFB_SCREEN`, but the format is `WxH` (no depth field) — a headless
wlroots output has no caller-chosen color depth.

Because the target app runs inside the headless sway that glass spawns (not the
host's compositor), this backend works on **any** Linux host — **including GNOME and
KDE** desktops, where the host desktop is simply irrelevant. Driving the user's
**existing live desktop** session — the Wayland analog of X11 `GLASS_DISPLAY=:0`
— is a separate, deliberate **non-goal**: it requires the XDG-portal path with an
interactive consent dialog, unsuited to unattended use.

---

## Android on Linux

The Android backend is **host-OS-agnostic** — it shells out to `adb`, so it runs from
a Linux host as well as Windows (macOS is planned — glass-mcp doesn't build on macOS
yet; see [running-on-macos.md](running-on-macos.md)). This section covers what's
Linux-specific about the setup.

### Install the Android SDK tools

You need `adb` and `emulator` from the Android SDK. Two routes:

**Via Android Studio or the command-line tools** (canonical):

```bash
# After installing the SDK command-line tools, e.g. to ~/android-sdk:
sdkmanager "platforms;android-34" "platform-tools" "emulator"
```

**Via a distro package** (convenience; version varies):

```bash
sudo apt-get install -y android-tools-adb    # Debian/Ubuntu — platform-tools only
# or
sudo apt-get install -y android-sdk-platform-tools
```

For the emulator you'll typically want the SDK route above regardless.

Point glass at `adb` with **`GLASS_ADB`** (or put it on `PATH`):

```bash
export GLASS_ADB=~/android-sdk/platform-tools/adb
```

### Create an AVD

If you don't have an emulator image yet, install a system image and create one (named `glass`
here, which `GLASS_AVD=glass` then selects):

```bash
sdkmanager "system-images;android-34;google_apis;x86_64"
avdmanager create avd -n glass -k "system-images;android-34;google_apis;x86_64" --device pixel_6
```

### Managed AVD (attach-or-boot)

Like Android Studio, glass prefers to attach: if an emulator is already online it uses
it (**`GLASS_ANDROID_SERIAL`** picks one when several are running). If none is running,
glass boots a **headless** AVD itself and stops it on shutdown — choose it with
**`GLASS_AVD`** (needed only when you have more than one AVD). Force attach-only with
**`GLASS_ANDROID_LIFECYCLE=attach`**.

The `emulator` binary resolves from **`GLASS_EMULATOR`** / **`ANDROID_SDK_ROOT`** /
`ANDROID_HOME`; pass extra boot flags via **`GLASS_EMULATOR_ARGS`**; keep a
glass-booted emulator alive past shutdown with **`GLASS_EMULATOR_KEEP`**.

Set `ANDROID_SDK_ROOT` (or `ANDROID_HOME`) so glass can find the emulator alongside
`adb`:

```bash
export ANDROID_SDK_ROOT=~/android-sdk
```

### Optional on-device agent (clipboard + high-fidelity input)

Over plain `adb`, glass types with `input text`/`keyevent` and can't reach the system
clipboard. A small companion — **[glass-android-agent](https://github.com/fixed-width/glass-android-agent)**,
a separate Apache-2.0 repo — closes both gaps: it runs on the device as a shell-uid
`app_process` server and gives glass real `MotionEvent`/`KeyEvent` injection (faithful
Unicode, plus multi-touch gestures via `glass_gesture`) and clipboard get/set.

Point **`GLASS_ANDROID_AGENT_JAR`** at its `glass-agent.jar`:

- Download the prebuilt jar from the agent repo's [Releases](https://github.com/fixed-width/glass-android-agent/releases).
- Or build it yourself: `./gradlew dex` in the agent repo produces `glass-agent.jar`.

glass pushes, launches, and tears the agent down for you. Without it, glass uses the
`adb` input path and `glass_clipboard_*` report unsupported. Set
**`GLASS_ANDROID_AGENT=off`** to force the `adb` paths even when the jar is present.

### Optional on-device a11y service (Compose-rich tree + high-fidelity `set_value`)

A second optional companion — also from **[glass-android-agent](https://github.com/fixed-width/glass-android-agent)** —
sharpens semantic addressing. `glass_a11y_snapshot` works over plain `adb` via `uiautomator`,
but `uiautomator` tends to flatten Jetpack Compose UIs, and `glass_set_value` falls back to
keystroke simulation. The on-device **AccessibilityService** reads the live
`AccessibilityNodeInfo` tree (so Compose semantics come through) and sets editable fields via
the real `ACTION_SET_TEXT`.

Point **`GLASS_ANDROID_A11Y_APK`** at its `glass-a11y.apk`:

- Download the prebuilt APK from the agent repo's [Releases](https://github.com/fixed-width/glass-android-agent/releases).
- Or build it yourself: `./gradlew :a11y:assembleDebug` in the agent repo.

glass installs the APK, enables the service, connects, and restores the device's prior
accessibility state on teardown — all automatically. Without it, glass uses the `uiautomator`
reader. Set **`GLASS_ANDROID_A11Y=off`** to force `uiautomator` even when the APK is present.

Scope: the service backs the **accessibility tree + `glass_set_value`**. Element *clicks* stay
coordinate taps (precise, using the service's bounds) — Android's `ACTION_CLICK` is unreliable
on Compose, so glass doesn't route clicks through it.

### Check the setup

```bash
GLASS_BACKEND=android glass-mcp doctor
# or with --deep to actually launch + ping the agent:
GLASS_BACKEND=android glass-mcp doctor --deep
```

Reports `adb`, the emulator + AVDs, the online/attachable device, and the agent + a11y-service status.
