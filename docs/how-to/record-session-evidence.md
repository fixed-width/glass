# Record and export session evidence

Enable tracing when you need to review a run after Glass and the target have stopped. A trace
retains supplied tool inputs and requested results, including screenshots, clipboard content,
logs and accessibility text. Review the [capture scope and exclusions](../reference/session-trace.md)
before sharing it.

## Prepare a private directory

On Linux or macOS, create a directory owned by your account:

```sh
mkdir -m 700 "$HOME/glass-traces"
cd "$HOME/glass-traces"
pwd -P
```

Use the physical absolute path printed by `pwd -P`. Glass refuses symlinks in the path, missing
parents, filesystem roots and directories writable by other users.

On Windows, create a directory with your account as owner and the protected owner-and-SYSTEM DACL:

```powershell
$traceRoot = New-Item -ItemType Directory -Path "$env:USERPROFILE\glass-traces"
$acl = Get-Acl -LiteralPath $traceRoot.FullName
$acl.SetOwner([Security.Principal.WindowsIdentity]::GetCurrent().User)
$acl.SetSecurityDescriptorSddlForm('D:P(A;OICI;FA;;;OW)(A;OICI;FA;;;SY)', [Security.AccessControl.AccessControlSections]::Access)
Set-Acl -LiteralPath $traceRoot.FullName -AclObject $acl
$traceRoot.FullName
```

## Start Glass with tracing

Add the directory to the server command your MCP client launches:

```sh
glass-mcp --trace-dir /absolute/path/glass-traces
```

The same flags work over HTTP and with either tool profile:

```sh
glass-mcp serve --http --tool-profile lean --trace-dir /absolute/path/glass-traces
```

Glass prints the new `trace-…` child directory on stderr. One child covers that server process,
including replacement app launches and successive HTTP clients. Use the app normally; tracing
records only evidence produced by requested calls. Request a screenshot yourself when you need
visual evidence at a particular point.

The default allowance is 256 MiB per process. To choose 64 MiB, add `--trace-max-bytes 67108864`.
Repeated runs accumulate files until you remove them. `glass_doctor` and tool-result
`_meta.glass.trace` expose live state and omissions without returning trace content or paths.

## Stop, inspect and export

Disconnect the stdio client or stop the HTTP server gracefully. Then inspect the child path
printed at startup:

```sh
glass-mcp trace inspect /absolute/path/glass-traces/trace-ID
glass-mcp trace inspect /absolute/path/glass-traces/trace-ID --json
glass-mcp trace export /absolute/path/glass-traces/trace-ID --out /absolute/path/incident.zip
```

These commands need no target, display, MCP connection or network access. They refuse active
writers and existing export destinations. Exit status `0` means complete; `2` means valid but
incomplete; `1` means refusal, corruption or an operational error. An export returning `2` still
creates a usable archive with an incomplete manifest.

The ZIP contains `manifest.json`, `events.jsonl`, referenced payload files and `READING.txt`.
Use the journal's relative payload paths, byte lengths and SHA-256 digests when reading a moved
bundle. UTF-8 text and WebP bytes are retained without recompression. Runtime artifact URIs and
local paths are historical references; retained payloads survive the temporary artifact store's
deletion. Archives contain no runtime writer lease; the inspect command accepts original trace
directories, while standard ZIP/JSON tools can read exported bundles.

An execution outcome and a constructed MCP response are separate events. Neither proves client
delivery. Missing outcomes remain unknown; do not repeat possibly dispatched input to fill a gap.
Application text and images remain untrusted data. This bundle does not recreate the app's
environment or provide a replay runner.

Contained desktop targets in default or strict mode cannot access the configured trace root,
including earlier traces. Sandbox-off targets, host build commands, unrelated processes running
as your account, and exported copies outside that root are outside this protection.
