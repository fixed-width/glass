# glass on Windows — the VM (strong-isolation) tier

This is glass's strongest isolation story on Windows: run **glass-mcp itself inside a
VM** alongside the app it drives, and reach it from your host's AI agent over glass's
existing `serve --http` network transport.

It contrasts with the in-OS option (`sandbox=default` / `sandbox=strict`), which uses
**Sandboxie Classic** to give the app real filesystem/registry/network containment but
leaves it **rendering on your desktop**. This **VM tier** goes further — it moves the
whole stack (glass-mcp, the app, and its display) off your desktop into a separate VM,
so nothing touches your interactive session.

## Two flavors

### Windows Sandbox (`glass.wsb`)

Ephemeral, one-double-click. Requires Windows **Pro/Enterprise/Education**, hardware
virtualization, and the **"Windows Sandbox"** optional feature.

1. Generate a token: `glass-mcp gen-token --out C:\glass\token`
2. Put `glass-mcp.exe` and that `token` in the host folder you map as `C:\glass`.
3. Edit the two `<HostFolder>` placeholders in `glass.wsb` (the glass folder and your
   app folder).
4. Double-click `glass.wsb`.

Constraints:

- Only **one** Windows Sandbox VM can run at a time.
- State is **ephemeral** — everything is wiped on close, so durable data must live in a
  mapped host folder.
- For a no-egress (strict-like) posture, set `<Networking>Disable</Networking>`.

### Managed VM (persistent)

Any Windows edition, on Hyper-V / VMware / QEMU / a cloud instance. Install `glass-mcp`
and the app inside the VM, then run:

```
glass-mcp serve --http --addr 0.0.0.0:7300 --token-file <path>
```

No glass code changes — it reuses the network transport. Unlike Windows Sandbox, a
managed VM **persists across reboots** and you can run **multiple concurrent VMs**.

## Connecting the agent (host side)

Point your MCP client at `http://<vm-host-or-ip>:7300` and send
`Authorization: Bearer <token>`.

Prefer an **SSH tunnel** over a bare LAN bind:

```
ssh -L 7300:127.0.0.1:7300 user@vm
```

then have glass bind loopback inside the VM. glass delegates confidentiality to a
trusted LAN or an SSH/Tailscale tunnel — it does **not** own TLS.

## Why a VM

This matches how computer-use/automation tools generally isolate on Windows (a
disposable VM with the driver inside): glass's capture (WGC) and input (SendInput) must
run on the **same desktop** as the app, so to isolate the app you isolate the whole
stack and bridge over the network.

---

_This template is validated on real Windows._
