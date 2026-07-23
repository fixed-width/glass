# glass documentation

Organized by what you need right now. New to glass? Start with the tutorial. Setting something up?
Head to the how-to guides. Looking up a fact? The reference. Want to understand why glass works the
way it does? The explanations.

> **Driving glass with an agent?** Install the [glass-drive skill](how-to/drive-glass-well.md) so your
> agent arrives already knowing the cheap-verify loop, instead of spending its first turns
> rediscovering it.

## Tutorial — learn by doing

- [Your first drive](tutorial/first-drive.md) — build glass, point an agent at a test window, and
  watch it close the build → see → interact → debug loop (Linux/X11, ~10 minutes).

## How-to guides — get something done

**Set up glass for your host**

- [Linux](how-to/setup-linux.md) — X11 and Wayland
- [Windows](how-to/setup-windows.md)
- [macOS](how-to/setup-macos.md)
- [Android](how-to/setup-android.md) — an AVD emulator, from any host
- [iOS](how-to/setup-ios.md) — the Simulator, macOS host only

**Connect and drive**

- [Connect glass to your agent](how-to/connect-an-agent.md) — stdio and HTTP
- [Run glass over the network](how-to/run-over-the-network.md) — agent and app on different machines
- [Drive glass well — the glass-drive skill](how-to/drive-glass-well.md)

**Contribute**

- [Build from source](how-to/build-from-source.md) — all platforms; macOS signing + LaunchAgent
- [Benchmark and profile](how-to/benchmarking.md)
- [Measure what the verification loop costs](how-to/verification-cost.md) — semantic vs
  screenshot-every-step, round-trips and tokens

## Reference — look up a fact

- [Tools](reference/tools.md) — every `glass_*` tool, its parameters, and platform support
- [Host compatibility](reference/host-compatibility.md) — which MCP hosts are verified, and what glass needs from any host
- [Stability and versioning](reference/stability.md) — the semver promise: what's covered from 1.0
- [Environment variables](reference/environment.md) — every `GLASS_*` variable
- [CLI](reference/cli.md) — `glass-mcp` subcommands
- [Platform support](reference/platforms.md) — the capability matrix and system requirements
- [Audit log](reference/audit-log.md) — the JSONL record schema and redaction

## Explanation — understand why

- [The build → see → interact → debug loop](explanation/the-loop.md)
- [Backends and display isolation](explanation/backends.md)
- [Containment and sandboxing](explanation/containment.md)
- [The macOS permission model](explanation/macos-permissions.md)
- [The Windows access model](explanation/windows-permissions.md)
