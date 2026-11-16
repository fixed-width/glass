# Run the macOS native diagnostics

Use these contributor diagnostics to inspect event construction and owned-window lifecycle.
They do not inject input or establish display isolation. You need the build prerequisites from
[Build from source](build-from-source.md) and the native permissions described below.
Cross-host checks additionally need a Darwin C++ toolchain and SDK; see
[Verify a change](verify-a-change.md#cover-the-macos-code-from-linux).

The read-only macOS input diagnostics use a nondefault build feature. To compile their binary and
tests from Linux:

```bash
cargo clippy --target x86_64-apple-darwin -p glass-macos --all-targets \
  --features native-diagnostics --locked -- -D warnings
```

On a Mac with a logged-in WindowServer, run:

```bash
cargo run -p glass-macos --features native-diagnostics \
  --bin glass-macos-window-input-readiness --locked
```

The diagnostic reads its own kernel process identity, constructs unposted click/scroll events,
checks event field and local-coordinate round trips, and reads existing permission grants without
requesting them. It posts no input, captures no screen content and activates no app. These checks establish event round trips and live process facts, not actual input delivery or
headless/display isolation. The feature adds no MCP parameter or input route; ordinary builds and
session capability reports retain their existing behavior. Missing private
symbols or incomplete identity/ABI readings return an error.

The same feature provides an owned-target inspector and an opt-in lifecycle test. On a Mac with
an existing Accessibility grant, build the nonactivating fixture and run the test:

```bash
swiftc crates/glass-macos/fixture/window_target.swift -o /tmp/glass-window-target-fixture
swiftc -D DEVELOPMENT_REBUILD crates/glass-macos/fixture/window_target.swift \
  -o /tmp/glass-window-target-rebuilt
cargo test -p glass-macos --features native-diagnostics \
  --test window_target --locked --no-run
# Use the executable path printed by Cargo above.
GLASS_WINDOW_TARGET_FIXTURE_BIN=/tmp/glass-window-target-fixture \
  /path/to/window_target-test-binary --ignored /tmp/glass-window-target-rebuilt
```

When the grant belongs to a signed app bundle, build with `--no-run` and run the resulting test
binary through that bundle's established signing and GUI-session procedure. The test requires
the grant; it never requests one. It launches fresh direct children with existing containment,
without the clipboard shim, and reads exact AX/WindowServer ownership and point geometry.
Child standard input/output is discarded. Post-read failures retain AX dispatch evidence even
though no input event was posted. The test covers geometry refresh, closure, replacement, competing windows,
child exit and cleanup. It reads no control values and posts no input. Successful observations report `window_identity_revalidated: true` for fresh checks of the original
process/window/connection/PSN tuple and retained AX reference. The live process image identity comes
from the kernel. No application, compiler or OS-build allowlist is involved. There is no executable
file hash/format/size requirement, and replacement or deletion of a file on disk does not invalidate
an unchanged running process. Caller cwd/environment and development loader settings are preserved.

The inspector is a diagnostic for an already built direct child with one reported window. Build and
window-hint requests are outside this helper's scope; the normal application launcher retains its
existing build/window-selection behavior. A failed observation invalidates the inspector, except
while waiting for its first window. Observed closure/replacement or unavailable ownership facts
refuse. Unobserved window-ID reuse in the same process/connection and changes between checks remain
possible. Point geometry and the AX subtree are observations, not a complete pixel mapping or a
supported input route. No operator input is needed and no physical interference verdict follows.

The two Swift commands compile ordinary sample apps locally. The `DEVELOPMENT_REBUILD` variant
changes the UI and reports its launch arguments, cwd and a caller environment value through an AX
identifier for test assertions. Supply that second binary after `--ignored`. The test checks
custom arguments/cwd/env and relative launch paths,
executable replacement/deletion and cwd rename during a live process, and relaunch after replacing
its binary with the changed build. Expected app behavior stays in tests; no binary/source identity
is embedded in the inspector. Neither these tests nor the diagnostic provide headless macOS support.
