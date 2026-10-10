# Verify a change

What to run so a change is covered before it reaches CI — including code for a platform you are not
on. Every command here is run from the repo root.

## The gates, on any host

These three are what CI fails a PR on, and they are the same everywhere:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

`--workspace` resolves on Linux, macOS and Windows alike: the platform crates gate themselves, so
the ones that do not apply to your host compile to nothing rather than failing.

They need nothing installed. Anything wanting a display, a compositor or a device is
`#[ignore]`d and does not run here — see [below](#integration-suites-linux).

## The gap this guide exists for

`cargo test --workspace` on Linux does not compile `cfg(target_os = "macos")` or `cfg(windows)`
code **at all**. It reports clean without having looked at it. If you changed a platform module,
the gates above are close to vacuous for the thing you changed, and nothing in their output says so.

The fix is to name the target.

## Cover the Windows code from Linux

```bash
rustup target add x86_64-pc-windows-gnu

cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets --locked -- -D warnings
```

That needs no linker and no Windows machine: `cargo check` and `cargo clippy` emit metadata and
never link.

To actually *run* the Windows-target tests you need a cross-linker and wine (Debian/Ubuntu:
`sudo apt install gcc-mingw-w64 wine`):

```bash
CARGO_TARGET_X86_64_PC_WINDOWS_GNU_RUNNER=wine \
  CARGO_TARGET_DIR=/tmp/glass-win \
  cargo test --workspace --target x86_64-pc-windows-gnu --locked --no-fail-fast
```

Three things to know before trusting the result:

- **CI builds `x86_64-pc-windows-msvc`, and that triple cannot be completed from Linux** — the
  `libudis86-sys` build script needs a C toolchain it cannot get here. Use `-gnu` and treat the CI
  run as the first msvc build.
- **wine is strong evidence for a failure, weaker for a pass.** `PathBuf` formatting and the
  executable suffix are compiled in per target rather than emulated, so a path-semantics bug fails
  here for the same reason it fails on Windows. Anything touching a real OS service is a different
  matter.
- **Use a fresh `CARGO_TARGET_DIR`.** A second run against a warm one is a cache hit that prints a
  clean result without compiling anything.

## Cover the macOS code from Linux

```bash
rustup target add x86_64-apple-darwin

cargo clippy --target x86_64-apple-darwin --workspace --lib --locked -- -D warnings
```

This compiles the `cfg(target_os = "macos")` modules for real — a type error inside one fails the
command from Linux.

- **`--lib` is what makes it work, and it is not the same reason the macOS CI job uses `--lib`.**
  Here it is because `--lib` does not build dev-dependencies: `--all-targets` pulls `criterion`,
  which depends unconditionally on `alloca`, whose build script compiles C — and cross-compiling
  that runs the *host* `cc` with `-arch` and `-mmacosx-version-min`, which Linux `gcc` rejects.
  Building it would need a darwin cross-toolchain such as osxcross.
- To include tests and benches for one crate, scope with `-p`:
  `cargo clippy --target x86_64-apple-darwin -p glass-macos --all-targets -- -D warnings` works,
  because `glass-macos` does not dev-depend on `criterion`.
- **You cannot link or run.** That needs the macOS SDK and a Mac. For anything past type-checking,
  push and let the macOS CI job run it, or run it on a Mac.

The read-only macOS input diagnostics use a nondefault build feature. To compile their binary and
tests from Linux:

```bash
cargo clippy --target x86_64-apple-darwin -p glass-macos --all-targets \
  --features native-input-qualification --locked -- -D warnings
```

On a Mac with a logged-in WindowServer, run:

```bash
cargo run -p glass-macos --features native-input-qualification \
  --bin glass-macos-window-input-readiness --locked
```

The diagnostic reads its own kernel process identity, constructs unposted click/scroll events,
checks event field and local-coordinate round trips, and reads existing permission grants without
requesting them. It posts no input, captures no screen content and activates no app. A successful
report always says `input_admitted: false`: these checks establish neither window lifetime nor
control, executable trust or routing qualification. The feature adds no MCP parameter or input route;
ordinary builds and session capability reports retain their existing behavior. Missing private
symbols or incomplete identity/ABI readings return an error.

The same feature provides an owned-target inspector and an opt-in lifecycle test. On a Mac with
an existing Accessibility grant, build the nonactivating fixture and run the test:

```bash
swiftc crates/glass-macos/fixture/window_target.swift -o /tmp/glass-window-target-fixture
cargo test -p glass-macos --features native-input-qualification \
  --test window_target --locked --no-run
# Use the executable path printed by Cargo above. Run outside Cargo, which adds DYLD settings.
env -u DYLD_LIBRARY_PATH -u DYLD_FALLBACK_LIBRARY_PATH -u DYLD_INSERT_LIBRARIES \
  GLASS_WINDOW_TARGET_FIXTURE_BIN=/tmp/glass-window-target-fixture \
  /path/to/window_target-test-binary --ignored
```

When the grant belongs to a signed app bundle, build with `--no-run` and run the resulting test
binary through that bundle's established signing and GUI-session procedure. The test requires
the grant; it never requests one. It launches fresh direct children with existing containment,
without the clipboard shim, reads exact AX/WindowServer ownership and point geometry, and tests
refresh, replacement, competing windows, child exit and cleanup. It reads no control values and
posts no input. All observations retain false lifetime, configuration, pixel-geometry and input
admission flags. Retained AX references detect some replacements; they supply no window lifetime
guarantee. The reported AX subtree does not establish complete WindowServer inventory or a qualified
action region. A failed observation invalidates the inspector, except while waiting for its first
window. These checks require no operator input and establish no physical interference verdict.

## Integration suites (Linux)

`#[ignore]`d, so the ordinary `cargo test` never starts them. Each self-starts what it needs.
On Debian/Ubuntu, install the full AT-SPI suite's fixtures with
`sudo apt install at-spi2-core bubblewrap dbus-bin xvfb python3-gi python3-cairo python3-gi-cairo gir1.2-gtk-3.0 gir1.2-gtk-4.0`.

```bash
./scripts/test-x11.sh [name]        # X11 suite; starts its own Xvfb
./scripts/test-wayland.sh [name]    # Wayland suite; needs sway >= 1.12
./scripts/test-a11y.sh [name]       # AT-SPI suite; starts a private bus + registry
```

Pass a substring to run one test.

The egui text-write recovery test also needs the workspace-excluded fixture:

```bash
cargo build --release --manifest-path crates/glass-fixture-egui/Cargo.toml
cargo test -p glass-a11y-linux --test egui -- --ignored
cargo test -p glass-mcp --lib consecutive_inputs_preserve_clicks_and_final_text_in_batches_and_standalone -- --ignored
cargo test -p glass-mcp --lib wayland_consecutive_inputs_preserve_clicks_and_final_text -- --ignored
```

Set `GLASS_EGUI_FIXTURE` to use an already-built fixture at another path. This test checks that a
missing text-write interface refuses before dispatch, keyboard input recovers, and numeric
accessibility writes still work.

The MCP egui tests check consecutive coordinate clicks and typing followed by Apply, both as
standalone tools and in `glass_do`. They start their own Xvfb display or headless sway compositor
and private accessibility bus. The Wayland test also pauses its owned fixture for 40 ms before
each sequence to check input consumption after a brief app stall.

The same app-outcome regression runs on a Windows host in its interactive desktop session:

```powershell
cargo build --release --manifest-path crates/glass-fixture-egui/Cargo.toml
cargo test -p glass-mcp --lib windows_consecutive_inputs_preserve_clicks_and_final_text -- --ignored --test-threads=1
```

Set `GLASS_EGUI_FIXTURE` to override the fixture executable on Windows as well. Build over SSH
if needed, then run the test executable through the scheduled-task bridge described below so
input and capture have access to the interactive session.

The X11 and Wayland **backend crates** also keep their display-backed unit tests behind
`#[ignore]`, and no harness script runs those — if you changed `glass-x11` or `glass-wayland`,
the command above covers the end-to-end suite but not them:

```bash
cargo test -p glass-x11 -- --include-ignored       # 73 tests; needs xvfb + at-spi2-core
cargo test -p glass-wayland -- --include-ignored   # 61 tests; needs sway >= 1.12, Mesa, Xwayland
```

## What only CI or a real host can do

| | where |
|---|---|
| `x86_64-pc-windows-msvc` build | CI, or a Windows box |
| macOS link, and any test execution | CI, or a Mac |
| macOS capture / input / a11y integration targets | a Mac with the TCC grants — see [build-from-source](build-from-source.md) |
| Windows on-box validation (Sandboxie, clipboard shim) | a Windows box, driven by `scripts/test-windows.sh` |
| Mutation testing | CI only; it is sharded there, and a local sweep saturates the machine |

The Sandboxie clipboard tests also need the logged-in user's **interactive Windows session**,
even though the probe has no GUI. From SSH, use the scheduled-task bridge in
`tools/windows-validation/run-onbox.ps1`; running the test executable directly in SSH session 0
can leave the boxed probe silent. Build `glass-clip-shim-windows` and its `clipprobe` example in
the test's profile, and set `GLASS_CLIP_SHIM_DLL` in the scheduled process to that DLL.

## The scripts

Fourteen run locally. One drives another machine.

| script | what it does |
|---|---|
| `test-x11.sh` | X11 integration suite; self-starts Xvfb |
| `test-wayland.sh` | Wayland suite; needs sway >= 1.12 |
| `test-a11y.sh` | AT-SPI suite; private bus + registry |
| `test-macos.sh` | glass-macos suite; exits 0 off macOS, so it is safe to call anywhere |
| `test-macos-a11y.sh` | compile/link gate for the macOS a11y integration test |
| `test-macos-mcp.sh` | headless stdio MCP smoke — server boots and lists its tools |
| **`test-windows.sh`** | **drives a REMOTE Windows box over SSH**; skips cleanly when none is configured |
| `sandbox-xvfb.sh` | manage the sandbox X display glass-mcp drives |
| `bench.sh` | run the benchmarks, or flamegraph one |
| `coverage.sh` | coverage via cargo-llvm-cov + cargo-nextest |
| `mutants.sh` | mutation-test glass-core (CI does this; see the table above) |
| `build-bundle.sh` | build the distribution bundle |
| `verification-cost.sh` | measure what the verification loop costs |
| `x11-geometry-settle-measurement.sh` | one-off X11 launch-geometry measurement |
| `wayland-geometry-settle-measurement.sh` | the Wayland counterpart |
