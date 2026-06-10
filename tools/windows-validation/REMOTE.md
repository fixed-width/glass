# Driving the Windows box remotely (from Linux)

All glass Windows dev + validation can be done from another machine — you never have to
sit at the box. This is the runbook. The whole thing hinges on one principle:

> **glass captures and injects into the *active interactive console session*** (the
> logged-in desktop the GPU composes). Your remote tool must **mirror that console
> session**, not spawn a new one.

That splits remote access into two camps:

| Camp | Tools | Use for |
|---|---|---|
| **Console-mirroring** ✅ | **Sunshine + Moonlight**, VNC, Parsec, Steam Remote Play | **Running** the capture/input tests — you drive the real session, so WGC/SendInput behave as in production |
| **Session-spawning** ⚠️ | Microsoft **RDP** | Editing/building only. RDP creates a *separate* session and **locks/blacks the console on disconnect**, which breaks capture + input |

## Recommended stack

| Job | Tool | Notes |
|---|---|---|
| Edit + build + git | **OpenSSH server** (+ optional VS Code **Remote-SSH**) | enabled by `setup-box.ps1`; build with `cargo build` over SSH |
| **Run + watch tests** | **Sunshine** (host) + **Moonlight** (Linux client) | open-source, low-latency, mirrors the console, survives client disconnect |
| Headless capture (item 2) | **Parsec VDD** virtual display | no need to unplug the monitor — capture a window placed on the virtual display |

## One-time setup

1. **On the box (elevated PowerShell):**
   ```powershell
   powershell -ExecutionPolicy Bypass -File .\setup-box.ps1
   ```
   This enables OpenSSH, Remote Desktop, no-sleep, and no idle-lock. *(Auto-login is
   already configured on this box, so omit `-User/-Password` — the script skips that step.)*
   It prints the `ssh user@<ip>` line to use.

2. **SSH from Linux** (edit/build):
   ```bash
   ssh <user>@<box-ip>
   # optional: ssh-copy-id for key auth; then point VS Code "Remote-SSH" at the same host
   ```

3. **Sunshine + Moonlight** (run/watch tests):
   - Install **Sunshine** on the box (<https://github.com/LizardByte/Sunshine>); open its
     web UI at `https://localhost:47990`, set a username/password, and note the PIN-pair flow.
   - Install **Moonlight** on Linux (`flatpak install flathub com.moonlight_stream.Moonlight`
     or your package manager); add the box, enter the PIN from Sunshine's web UI, then launch
     the **Desktop** entry. You're now looking at the real console session.

4. **Virtual display driver** for item 2 (headless): install **Parsec VDD**
   (<https://github.com/nomi-san/parsec-vdd>, MIT/signed).

## The one gotcha: build over SSH, *run probes in the mirrored session*

A process launched **over SSH runs non-interactively** (the sshd service's session, not the
console) — so `winval capture`/`input` started from an SSH shell will capture nothing / inject
nowhere. This is the Windows equivalent of "a bare SSH shell isn't the GUI session."

- **`cargo build`** over SSH — fine (session-agnostic). ✅
- **`winval capture` / `input` / `discover` / `killtree`** — run them from a terminal **inside
  the Moonlight/VNC view** (the console session). ✅
- If you must launch from SSH, relaunch into the console session: `PsExec -i 1 -d winval.exe …`
  (Sysinternals), or a Scheduled Task set to run in the interactive session.

## The dev loop

```bash
# One command from Linux (set the env once, see below):
./scripts/test-windows.sh onbox_handoff     # one example
./scripts/test-windows.sh                    # all onbox_* examples
./scripts/test-windows.sh --tests clip       # ignored tests matching "clip"

# It pushes your branch (and ships any uncommitted changes), syncs the box, builds, runs each
# target in the interactive session via the schtasks /it bridge, prints "N PASS / M FAIL", and
# pulls WebP captures into ./.windows-artifacts/. Exit code is the verdict.
```

Configure the box once (nothing box-specific is committed):

```bash
export GLASS_WIN_HOST=user@box-ip          # required; unset => the script skips cleanly
export GLASS_WIN_REPO=C:/Users/user/glass  # optional; defaults to C:/Users/<user>/glass
```

Under the hood it still obeys the session rule below — `run-onbox.ps1` uses a `schtasks /it`
scheduled task to execute in the interactive console session (the manual `winval`-in-Moonlight
step is only needed for live watching).

Once the make-or-break gate passes, the same box becomes the dev + integration-test machine
for the real `glass-windows` crate (its `#[ignore]`d E2E suite runs in this mirrored session,
exactly like the X11/Wayland suites run under Xvfb/sway).

## Gaming-rig notes

- Real GPU + monitor → items **1, 3, 4, 5, 6, 8 work immediately**; you also get genuine
  GPU-accelerated capture coverage (Chrome/Electron/games) for the `PrintWindow`-black vs
  WGC-handles-it contrast.
- **Item 2 without unplugging:** install the virtual display, move a window onto it, capture
  it there (`winval displays` shows it; drag the window over; `winval capture <title>`).
- **Coexists with gaming** — glass drives target apps as a black box in the same session;
  nothing is dedicated or wiped. The `setup-box.ps1` lock/sleep changes are reversible.
- Game **overlays** (Steam / GeForce Experience / RGB / Discord) inject their own windows and
  hooks — harmless, but they're the likely culprit if a probe ever reports a surprise window.
- **Recovery:** auto-login means a reboot returns to a usable session unattended; add
  Wake-on-LAN if the box ever drops off the network.
