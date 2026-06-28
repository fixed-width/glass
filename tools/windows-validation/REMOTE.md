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
| Headless capture | **MttVDD** virtual display | a window placed on the virtual monitor is capturable without unplugging the real one (Parsec VDD also works) |

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

4. **Virtual display driver** for headless capture: install the community
   **[Virtual-Display-Driver / MttVDD](https://github.com/VirtualDrivers/Virtual-Display-Driver)**
   (MIT, signed) — it auto-provisions a virtual monitor from
   `C:\VirtualDisplayDriver\vdd_settings.xml` with no holder process. **Parsec VDD**
   (<https://github.com/nomi-san/parsec-vdd>, MIT/signed) also works, but needs a
   ping-holder process kept running.

## The one gotcha: SSH is session 0, capture/input need session 1

A process launched **over SSH runs non-interactively** (the sshd service's session, not the
console) — so WGC capture returns nothing and `SendInput` injects nowhere from a bare SSH shell.
This is the Windows equivalent of "a bare SSH shell isn't the GUI session."

`scripts/test-windows.sh` handles this for you:

- **`cargo build`** over SSH is fine (session-agnostic). ✅
- The test/example binary is **bounced into the interactive console session (session 1)** by
  `run-onbox.ps1` via a `schtasks /it` scheduled task, where capture + input work. ✅
- So you drive everything from the SSH shell; the mirrored Moonlight/VNC session is only needed to
  **watch a run live** (or to debug something on the desktop by hand).

## The dev loop

```bash
# One command from Linux (set the env once, see below):
./scripts/test-windows.sh onbox             # one example
./scripts/test-windows.sh                    # all onbox* examples
./scripts/test-windows.sh --tests onbox      # the #[ignore]d tests/onbox.rs suite (in session 1)

# It pushes your branch (and ships any uncommitted changes), syncs the box, builds, runs each
# target in the interactive session via the schtasks /it bridge, prints "N PASS / M FAIL", and
# pulls WebP captures into ./.windows-artifacts/. Exit code is the verdict.
```

Configure the box once (nothing box-specific is committed):

```bash
export GLASS_WIN_HOST=user@box-ip          # required; unset => the script skips cleanly
export GLASS_WIN_REPO=C:/Users/user/glass  # optional; defaults to C:/Users/<user>/glass
```

Under the hood it obeys the session rule above — `run-onbox.ps1` uses a `schtasks /it` scheduled
task to execute in the interactive console session; the mirrored Moonlight/VNC session is only
needed when you want to watch a run live.

This box is the dev + integration-test machine for `glass-windows`: its `#[ignore]`d on-box suite
(`crates/glass-windows/tests/onbox.rs`) runs here via `--tests`, exactly like the X11/Wayland
suites run under Xvfb/sway.
