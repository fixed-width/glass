# glass Windows backend — validation probes (`winval`)

Throwaway Rust probes that exercise the risky pieces of the Windows backend design
([`../../docs/superpowers/specs/2026-06-05-windows-backend-design.md`](../../docs/superpowers/specs/2026-06-05-windows-backend-design.md))
so the make-or-break gate in the
[validation plan](../../docs/superpowers/specs/2026-06-05-windows-validation-plan.md)
can be checked on the box with minimal manual work.

Each `winval` subcommand maps to a validation item and **prints `PASS` / `FAIL` / a
clear note** so you don't have to eyeball pixels. The probes lean on the same WGC
capture path (`xcap`) and Win32 APIs the real backend will use.

> Cross-checked from Linux against `x86_64-pc-windows-gnu` (`cargo check`/`clippy`), so
> the code compiles before it reaches the box. Build it there with the **MSVC** toolchain.

## 0. One-time box prep

Run `setup-box.ps1` (elevated) once — it enables auto-login (so a monitorless reboot
brings up a composited interactive session), disables sleep, and enables Remote Desktop.
Then install a **signed IddCx virtual display driver** for the headless capture test:

- Parsec VDD — <https://github.com/nomi-san/parsec-vdd> (MIT, signed), or
- Virtual-Display-Driver — <https://github.com/VirtualDrivers/Virtual-Display-Driver> (MIT, signed)

Install the Rust **MSVC** toolchain + "Desktop development with C++" (for the linker), then:

```powershell
cargo build --release
# the binary is target\release\winval.exe ; examples below use `cargo run --release --`
```

Keep three target apps handy: **Notepad** (plain Win32), an **Electron app or Chrome**
(GPU/Chromium), and a **Java** Swing/JavaFX app.

## 1. Run the gate

`winval windows` lists window titles + pids; use a title substring as the arg below.

| Item | Command | Pass looks like |
|---|---|---|
| **1** WGC capture (real desktop) | `cargo run --release -- capture Notepad shot1.png` | `PASS: captured NON-BLANK …`; `shot1.png` shows Notepad |
| **2** WGC headless on virtual display *(make-or-break)* | `winval displays` (confirm the virtual display), drag the app onto it, then `… -- capture <title> shot2.png` with **no monitor** | `PASS: captured NON-BLANK …` on the virtual display |
| **3** SendInput lands *(make-or-break)* | `cargo run --release -- input Notepad` | click hits the centre + `glass` typed (confirm with `capture`) |
| **4** child-process discovery *(make-or-break)* | `cargo run --release -- discover --spawn -- "C:\path\app.exe"` (Electron/Java) | `PASS: a visible window is owned by a DESCENDANT pid …` |
| **5** PMv2 coords @150/200% *(make-or-break)* | set scaling to 150% then 200%, `cargo run --release -- dpi Notepad` | `PASS: PMv2 context active`; dpi 144 / 192 reported |
| **6** Job kill-tree *(make-or-break)* | `cargo run --release -- killtree --spawn -- "C:\path\electron-app.exe"` | `PASS: every process in the tree is gone …` |
| **7** WGC minimized/border | minimize the app, then `… -- capture <title>` | prints the `MINIMIZED` stale-frame warning |
| **8** PrintWindow black on GPU | `… -- printwindow Chrome pw.png` and `… -- printwindow Notepad pw2.png` | Chrome → `BLACK …`; Notepad → `CONTENT …` |

Record outcomes in the results table in the validation plan (note the Windows build
via `winver` and the display mode), then flip the design spec's status to
"validated — proceed" and turn its phasing into a writing-plans plan.

## Subcommands

```
winval displays                      list displays (confirm a virtual display)
winval windows                       list top-level app windows + pids
winval capture <title> [out.png]     item 1/2/7: WGC capture, assert non-blank
winval printwindow <title> [out.png] item 8: PrintWindow (black on GPU apps)
winval input <title>                 item 3: focus + click + type via SendInput
winval dpi [title]                   item 5: confirm PMv2 + per-window DPI/bounds
winval discover --pid <N>            item 4: find the app window via descendant pids
winval discover --spawn -- <cmd...>  item 4: spawn then discover (Electron/Java)
winval killtree --spawn -- <cmd...>  item 6: Job-Object kill-tree teardown
```

## Notes / caveats

- These probes prove **capture / input / discovery / teardown / DPI**. They are not the
  backend — they hard-code US-centric assumptions and skip error-path polish on purpose.
- The capture probes use `xcap` (WGC under the hood) instead of hand-rolling the D3D11
  frame-pool readback, to keep the probe small; the real backend will do the readback
  itself to control the error path (minimized → error, etc.).
- `discover --spawn`/`killtree --spawn` take the literal command after `--`. The Job is
  assigned right after spawn, so a child spawned in the very first instant could escape —
  if `killtree` reports survivors, re-run; a consistent survivor is a real breakaway.
- `winval` sets Per-Monitor-V2 awareness at startup via `SetProcessDpiAwarenessContext`
  (the real backend uses a manifest); item 5 confirms it took effect.
