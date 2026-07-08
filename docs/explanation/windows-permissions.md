# The Windows access model

macOS gates screen capture and input injection behind its privacy system (TCC), so glass on the Mac
ships as a signed app plus a LaunchAgent and walks you through two one-time grants. **Windows works
differently: there are no per-app permissions to grant.** In a normal logged-in session, glass's three
core capabilities all work with no consent prompt and nothing to click:

- **Screen capture** (Windows.Graphics.Capture)
- **Input injection** (SendInput)
- **The accessibility tree** (UI Automation)

This is why Windows glass ships as a plain `glass-mcp.exe` with no onboarding flow: there is no grant
for an onboarding flow to collect. `glass-mcp doctor` verifies *environment* facts (an interactive
session, a supported Windows build, DPI awareness, containment), never a permission grant.

## What actually constrains access on Windows

Three things gate what glass can reach — none of them a privacy-consent dialog.

**The interactive session.** Only the active, logged-in *console* session composes a desktop that
Windows.Graphics.Capture can grab. A process in **Session 0** — a Windows service, or a bare
`ssh user@box` shell — has no rendering desktop, so capture has nothing to read. Run glass in a normal
logged-in session (the doctor's `interactive session` check reports this). See
[how-to/setup-windows.md](../how-to/setup-windows.md).

**Integrity levels (UAC / UIPI).** Windows isolates processes by *integrity level*. A normal
(medium-integrity) glass process cannot inject input into — or fully automate — a window owned by a
**higher-integrity** process, such as an app launched *Run as administrator* or a UAC elevation prompt.
It also cannot capture the **secure desktop** that hosts UAC prompts, the lock screen, and
Ctrl+Alt+Del. This is the closest thing Windows has to a "permission," but it is a launch-context
relationship, not a consent grant: to drive an elevated app, run glass at the same (elevated) integrity
so the two processes match. Most apps run at medium integrity, where the default works with no special
handling.

**SmartScreen and Defender (distribution, not runtime).** These gate *installing* an unknown binary,
not *what a running binary may do*. An unsigned download of `glass-mcp.exe` triggers Microsoft Defender
SmartScreen's "Windows protected your PC" banner — a publisher-trust prompt, not an access grant. A
signed release clears it; an unsigned build runs after **More info → Run anyway**. See the install
steps in [how-to/setup-windows.md](../how-to/setup-windows.md).

## Why there's no grant to click

macOS TCC records each grant in a per-app consent database keyed to the app's code-signing identity, so
the grant must be requested, is snapshotted at launch, and is re-checked on every rebuild. Windows has
no such per-app capability database for capture, input, or UI Automation. Access is decided by *where
the process runs* — its session and its integrity level — not by a stored per-app decision. Two
consequences follow: rebuilding or moving `glass-mcp.exe` never re-prompts (there is no grant to lose),
and glass needs no responsible-process or LaunchAgent gymnastics to hold one — the reasons the macOS
build is packaged the way it is simply don't arise on Windows.

For how launched apps are sandboxed on Windows see [explanation/containment.md](containment.md); for the
capability matrix and requirements see [reference/platforms.md](../reference/platforms.md). The
contrasting macOS model is in [explanation/macos-permissions.md](macos-permissions.md).
