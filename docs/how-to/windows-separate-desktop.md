# Develop applications on a separate Windows desktop

This preview lets Glass run application windows and ordinary input on a local child desktop while
you keep using your signed-in desktop. It uses your Windows account. Choose the application's
containment level separately; a child desktop does not provide a separate-account file boundary.

The distributed Windows executable includes its desktop host. Setup does not compile code or
import a type library on your machine. It uses Windows' installed Remote Desktop control and
.NET Framework. Availability is checked through Windows APIs and actual startup, without an
edition-name allowlist.

## Enable the preview

Run this from a terminal on your signed-in Windows desktop:

```powershell
glass-mcp desktop enable
```

Windows may ask for administrator consent to enable local child sessions. Builds, applications and
the application worker run unelevated. The Windows child-session switch is machine-wide; Glass's
saved desktop selection is per account. Setup does not enable network Remote Desktop access or
change firewall rules. If child sessions were already enabled, Glass preserves that setting when
you later disable the preview.

Restart your MCP client after setup. New Windows MCP servers use the saved selection. To make
the choice explicit in a registration, launch the server with:

```powershell
glass-mcp --desktop separate
```

Use the existing `glass_start`, capture, accessibility, input, logs and stop tools. The first
application launch starts the child and may open a native Windows sign-in dialog. Complete that
dialog locally; keep passwords out of agent messages and tool arguments. Stopping an application
retains the authenticated desktop for the next build or launch. Exiting the MCP server ends its
environment.

Windows permits one active connected child session at a time. A different child or another server
using the slot produces a busy error. Glass does not take over an existing child or silently route
separate-desktop work onto the ordinary desktop.

## Inspect state and resolve a blocker

```powershell
glass-mcp desktop status
glass-mcp desktop status --json
```

Status is read-only: it does not start a desktop, show a sign-in dialog or change Windows settings.
It distinguishes configuration from the live owner and worker. Disabled functionality is not
reported as proof that your Windows edition lacks it.

Windows may start other programs inside the child. A covering window can prevent an input action
from reaching its target. Inspect the child from the Windows logon that owns it:

```powershell
glass-mcp desktop show
```

Resolve the blocker in the viewer, then close the viewer window or run:

```powershell
glass-mcp desktop hide
```

Closing a ready viewer hides it and retains the environment. Ordinary background work keeps the
viewer hidden. Glass preserves its target and occlusion checks inside that desktop.

The worker uses the MCP server's working directory and PATH when an application does not override
them. Pass other build-specific variables through `glass_start`'s existing `env` argument.
Contained clipboard availability follows the normal Windows containment provider; unavailable
private clipboard support remains an explicit restriction.

## Stop, recover or disable

```powershell
glass-mcp desktop stop
```

This ends the owned environment and retains setup. Restart the MCP server before creating another
environment. An existing server refuses to switch silently to shared desktop input after a stop
or selection change.

If an owner exited with unfinished cleanup, run:

```powershell
glass-mcp desktop recover
```

Recovery checks recorded process and session generations before acting. Unconfirmed cleanup stays
visible as an error, with its state retained for another attempt. Recovery retains application
files, persistent sandbox data and crash logs.

To disable the saved selection and restore a child-session setting changed by setup:

```powershell
glass-mcp desktop disable
```

Windows may request administrator consent for restoration. Glass leaves an already enabled
setting and unrelated sessions intact. Restart MCP servers afterward. To deliberately use the
ordinary desktop, register a fresh server with `--desktop shared`; its input can affect your
cursor and foreground application.

## Preview qualification

The current investigation uses a local account on Windows 11 Pro with an RDP parent. Windows Home,
managed sign-in policies and simultaneous physical-console hardware input still need qualification.
An unavailable or failed environment returns its diagnostic instead of falling back to the user's
desktop. RDP-to-console transfers are not part of setup or recovery.
