# Containment and sandboxing

glass launches apps it has been told to build and drive — often half-working, sometimes hostile-by-
accident code under active development. So it **sandboxes every launched app by default**. This page
explains the model; the per-OS install steps live in the setup guides, and the exact variables in
[reference/environment.md](../reference/environment.md).

## Three levels, and why the default is fail-closed

`glass_start`'s `sandbox` argument (or `GLASS_SANDBOX`) selects one of three levels:

- **`default`** — filesystem and process containment; the app may reach the network.
- **`strict`** — the same containment, plus outbound network blocked.
- **`off`** — no containment; the app runs unconfined.

The important design choice is that `default` and `strict` are **fail-closed**. If no containment
runtime is available, `glass_start` **errors** rather than silently launching the app unconfined. A
sandbox that quietly turns itself off when it can't run is not a sandbox — it's a false sense of one.
`off` exists as the single, explicit escape hatch, so opting out is always a visible act.

One boundary is worth stating plainly: the `sandbox` level governs the **launched app only**. The
optional `build` step always runs unsandboxed, with your full developer environment — building is your
code doing what you asked, whereas the launched app is the thing under test.

## Per-OS mechanisms

Containment has a different implementation per OS:

- **Linux** — [bubblewrap](https://github.com/containers/bubblewrap), which also needs unprivileged
  user namespaces enabled.
- **Windows** — [Sandboxie](https://sandboxie-plus.com) (Classic, the free default, or Plus).
- **Android** — the emulator VM itself; there is no separate containment step.
- **iOS** — the Simulator itself is the isolation boundary; like Android, there is no separate
  containment step (and no sandbox crate).
- **macOS** — the OS's built-in **Seatbelt** sandbox (`sandbox_init`); nothing to install.

## The macOS Seatbelt profile

The generated Seatbelt profile is **deny-default**: anything not explicitly allowed is denied. Its
filesystem model is deliberately *not* an allowlist of system directories. Instead the **whole
filesystem is readable** (read-only), with one carve-out: your **home directory** (`/Users`) is
denied, so `~/.ssh` and the rest of your secrets stay hidden. The working directory and the launched
program's own directory are re-allowed for reads even when they live inside your home, so a project
checked out under `~/` still works. **Writes** are confined to the working directory plus the usual
scratch/cache roots (`/private/var/folders`, `/private/tmp`, `/tmp`, `/dev`) — nothing under your home
is ever write-allowed. `mach-register` is allowed so a sandboxed app can still serve its accessibility
tree; an app that can't register with the window server would return an empty AX tree.

## Clipboard isolation

Under `default`/`strict`, the clipboard tools act on a clipboard **isolated from your real one** — the
app copies and pastes normally, but against a private store glass owns, so driving an app never
clobbers what you have on your own clipboard.

How that isolation is achieved varies. On the private Xvfb/sway Linux backends the app simply has its
own display clipboard. On a contained Windows app, glass backs the boxed app's clipboard with its own
store. On **macOS**, isolation depends on how the target is built: for an app **not** built with
Apple's hardened runtime — typically a debug or unsigned build, i.e. the app you are developing — glass
injects a small shim (`DYLD_INSERT_LIBRARIES`) that swizzles `+[NSPasteboard generalPasteboard]` inside
the app and redirects it to a private, per-session pasteboard glass shares. glass confirms the swizzle
actually took (a sentinel written to the private pasteboard) before routing to it, so a silently-failed
injection is never mistaken for a working bridge.

An app that runs under the **hardened runtime** (App Store or notarized apps) can't be injected — the
OS strips `DYLD_INSERT_LIBRARIES` — so the profile denies it the real pasteboard service and the
clipboard tools return `Unsupported`. That is fail-closed at the profile level, not a silent fall-back
to the shared system clipboard. Only shared-desktop modes (`GLASS_DISPLAY=:0`, or the Windows/macOS
backend at `sandbox:off`) touch your real clipboard.

## Known limits

The containment is real but not airtight, and it's honest about the edges:

- **The macOS mach allow-list is broad** (`allow mach-lookup`) — narrowing it to only the services a
  given app needs is a hardening follow-up.
- **The macOS clipboard shim covers `NSPasteboard` only.** An app that reaches the pasteboard through
  lower-level Carbon/Core Services APIs isn't redirected.
- **Electron apps may escape their own in-app sandbox** under this containment. Seatbelt contains the
  process from the outside; an Electron app's internal renderer/main-process sandboxing is a separate
  mechanism this doesn't harden.
- **`sandbox_init` is deprecated** by Apple in favour of the App Sandbox entitlement model, but it
  remains present and functional on every currently-supported macOS release — it underpins App Sandbox
  itself, and Chromium uses it too.

`glass-mcp doctor` reports the live containment-runtime availability for your host.
