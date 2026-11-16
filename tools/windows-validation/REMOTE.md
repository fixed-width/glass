# Run Windows validation over SSH

Use this runbook to build on a Windows host and run native capture/input tests from another
machine. SSH is suitable for compilation. GUI tests must run in a usable signed-in Windows
session; a service or bare SSH process may have no interactive desktop.

## Prepare the Windows host

Install Git, Rust's MSVC toolchain, the Visual Studio C++ build tools and CMake. Clone Glass to a
dedicated validation checkout that the remote script may reset and clean. Configure OpenSSH access
and the checkout's access to the repository remote. Sign in to the account that will run the tests.

For contained-app tests, install Sandboxie and its running service. Clipboard-shim tests also need
the shim DLL and `clipprobe` example built in the test's profile; see
[Verify a change](../../docs/how-to/verify-a-change.md).

A physical monitor, dummy plug or virtual display driver can provide a composited display on a
headless host. A virtual monitor does not isolate input. See
[Windows setup](../../docs/how-to/setup-windows.md#headless-capture--a-virtual-display-driver).

`setup-box.ps1` is an optional machine-configuration script for a designated validation host.
Read it before use: it changes sleep/idle-lock policy, enables Remote Desktop and OpenSSH, and opens
firewall rules. Its optional `-User`/`-Password` path stores auto-login credentials in the registry.
Those changes are not prerequisites for ordinary Glass use or a requirement to enable auto-login.

## Select the graphical session

Glass supports a usable console or RDP session. Session IDs are assigned by Windows; do not assume
the graphical session is always `1`. Keep the intended session unlocked and connected while testing.
Locking or disconnecting an RDP session can block capture/input. The separate-desktop preview's
qualified lock/disconnect behaviour is recorded in
[the preview guide](../../docs/how-to/windows-separate-desktop.md#preview-qualification).

A console-mirroring viewer such as VNC or Sunshine/Moonlight can show a console test without creating
another login. A viewer is needed only for inspection or manual interaction; the test bridge runs
through Task Scheduler. Avoid transferring a live test between RDP and console sessions.

## Configure the remote runner

From the Linux checkout:

```bash
export GLASS_WIN_HOST=user@windows-host
export GLASS_WIN_REPO=C:/Users/user/glass-validation
./scripts/test-windows.sh --dry-run onbox
```

`GLASS_WIN_REPO` defaults to `C:/Users/<ssh-user>/glass`. Host and checkout paths must contain no
spaces. `GLASS_WIN_SSH_OPTS` accepts additional SSH/SCP options.

Inspect the dry run first. A real run pushes the local branch to `origin`, resets and cleans the
remote validation checkout to that commit, then overlays tracked changes and untracked files.
It does not preserve unrelated work in that checkout. Run from a named feature branch.

## Run and inspect results

```bash
./scripts/test-windows.sh onbox          # matching example
./scripts/test-windows.sh                # all onbox examples
./scripts/test-windows.sh --tests onbox  # ignored native tests matching onbox
```

The script builds over SSH, then `run-onbox.ps1` schedules the executable with interactive-logon
Task Scheduler settings. It prints per-target results and an aggregate, then copies captured
artifacts into `.windows-artifacts/` locally. The aggregate and exit status determine the verdict;
a missing completion marker is a failure. With `GLASS_WIN_HOST` unset, the script skips successfully.

For another profile, add `--release`. Native GUI tests should run serially against the intended
session. Inspect the bridge's task identity and interactive session if input or capture is absent;
a successful build does not establish that the test reached a GUI context.
