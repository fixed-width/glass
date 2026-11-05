# Develop applications on a separate Windows desktop

This preview lets Glass run application windows and ordinary input on a local child desktop while
you keep using your signed-in desktop. It uses your Windows account. Choose the application's
containment level separately; a child desktop does not provide a separate-account file boundary.

The distributed Windows executable includes its desktop host. Setup does not compile code or
import a type library on your machine. It uses Windows' installed Remote Desktop control and
.NET Framework. Availability is checked through Windows APIs and actual startup, without an
edition-name allowlist.

## Prepare the desktop before agent work

Open Glass from a terminal on your signed-in Windows desktop:

```powershell
glass-mcp desktop open
```

In **Application setup…**, choose **Off your desktop…** and **Continue setup**. Choose
**Start Environment…** in the window or system-tray menu. Windows may ask
for administrator consent to enable local child sessions, followed by a native Windows sign-in
dialog. Complete these locally; keep passwords out of agent messages and tool arguments. Glass
shows **Ready** only after the child and its application worker are connected and verified.

The child-session switch is machine-wide; Glass's saved desktop selection is per account. Builds,
applications and the worker run unelevated. Setup does not enable network Remote Desktop access
or change firewall rules. Glass preserves an already enabled setting when you later disable setup.

A standard Windows account can use both desktop choices. If setup or restoration needs a machine
setting changed, complete Windows' prompt with an administrator account. The resident, worker,
per-user startup task and private state continue to belong to the signed-in standard account.

Copy the **MCP endpoint** from Glass and register it as an HTTP MCP connection in your agent.
Use the normal `glass_start`, capture, accessibility, input, logs and stop tools. Application stops
and MCP client reconnects retain the authenticated desktop. Closing the Glass window hides it;
Glass continues running in the system tray. **Quit Glass** stops its work and owned desktop.

To run the resident app with a chosen endpoint or bearer-token file:

```powershell
glass-mcp serve --tray --addr 127.0.0.1:7300 --token-file C:\path\to\token
```

`--tray` implies HTTP serving. The default endpoint is `http://127.0.0.1:7300/`. Configure the same
bearer token in your MCP client when using one. **Start with Windows** installs a per-user,
unelevated sign-in startup task and opens quietly in the system tray. Authenticated startup uses
the supplied token-file path; an environment-only token is not saved for startup. A child sign-in
preserves the existing resident instead of starting another Glass instance inside the child.

If the desktop is not ready or has ended, application requests return `environment_not_ready`
with an instruction to finish Application setup. Tools never open a sign-in dialog or recreate the
desktop. A separate stdio server cannot borrow the resident app's child; use the tray app's endpoint.
`desktop enable` can configure the Windows setting alone; complete preparation in Glass afterward.

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

This ends the owned environment and retains setup. In the resident app, choose **Start
Environment…** to prepare another environment. An agent request never restarts it or switches silently
to shared desktop input after a stop or selection change.

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

Preview testing covers a local account on Windows 11 Pro with RDP and physical-console parents.
The physical-console test exercised default and strict Sandboxie while the user typed and moved
the mouse on the parent desktop. Checks covered file redirection, protected state access, private
clipboard roundtrips and strict network denial. Windows Home and managed policies still need
qualification.

A genuine standard account has also been checked with an RDP parent on that host. Default and
strict Sandboxie apps passed input, capture, private-clipboard and protected-state checks in both
desktop modes. Setup and restoration used another account's administrator credentials in Windows'
native dialog; the resident, worker, tasks and private state retained the standard account's
ownership. Actual Windows sign-out/sign-in started Glass quietly and reached Ready on the main
desktop. Disabling startup and stopping the child also passed. A separate setup check confirmed
that enabling and disabling Glass preserves a machine setting already enabled by an administrator.
This does not qualify managed Sandboxie configuration restrictions or a standard console account
without Remote Desktop logon rights.

On the same Windows 11 Pro host, a strict Sandboxie app survived a parent lock/unlock and an
ordinary RDP disconnect/reconnect. Three input, fresh-capture and private-clipboard rounds passed
after each transition on the original app process and window. Semantic pointer requests while
the parent was locked or disconnected returned `transport_failure` before dispatch. Keep the
parent unlocked and connected for GUI input. The lifecycle checks passed retention and resumption;
continuing GUI input during a lock or disconnection is outside the preview's requirements.

The resident tray flow has also been tested on that host: preparation before app work, refusals
before preparation and after Stop, retained strict Sandboxie apps across MCP client reconnects,
and a fresh desktop after an explicit Stop/Start in the same resident app. Native Show, Hide,
Quit and detached launching were exercised as well.

The common setup flow has been checked with both desktop choices. Strict-contained app interaction,
capture, private clipboard and protected-state access checks passed on and off desktop. With
**On your desktop** and **Start with Windows** saved, an actual Windows restart and sign-in
started Glass quietly, preserved its HTTP endpoint and reached Ready automatically. A strict-contained
app then passed pointer input, typing, capture, private clipboard and protected-state checks.
Disabling startup kept the running resident, changed startup definitions were retained, and
switching modes preserved the HTTP endpoint.

**Off your desktop** has also been checked across an actual restart with an active child.
Glass started quietly with the saved choice and endpoint, refused app work before preparation,
then recovered the old ownership records and prepared a fresh child after **Start Environment…**
and Windows sign-in. An app with strict containment then passed three rounds of input, fresh
capture, private clipboard and protected-state checks in that child.

An intermittent teardown process-query error was fixed by waiting on the verified helper's
process handle before recording completion. Native handle tests, five finalizing-helper runs
and a real **Stop Environment** check passed without a teardown retry. Identity-check failures
and exit-wait timeouts still preserve incomplete cleanup instead of reporting success.

An unavailable or failed environment returns its diagnostic instead of falling back to the user's
desktop. RDP-to-console transfers are not part of setup or recovery.
