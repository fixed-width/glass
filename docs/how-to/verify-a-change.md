# Verify a change

What to run so a change is covered before it reaches CI — including code for a platform you are not
on. Every command here is run from the repo root.

## The gates, on any host

With the [build prerequisites](build-from-source.md) installed, run formatting and lint checks:

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

On Linux and Windows, run:

```bash
cargo test --workspace --locked
```

On macOS, match CI's library-test gate:

```bash
cargo build --workspace --all-targets --locked
./scripts/test-macos.sh
cargo test --workspace --lib --locked
```

Several macOS integration targets use `harness = false` and require a granted GUI context.
A plain macOS `cargo test --workspace` attempts to run those targets; `--lib` keeps this gate to
unit tests. Build/clippy still cover all targets.

`--workspace` resolves on Linux, macOS and Windows alike: the platform crates gate themselves, so
the ones that do not apply to your host compile to nothing rather than failing.

These gates need the compiler/toolchain dependencies, but no live display or mobile device.
Ordinary display/device tests are ignored; native integration suites need the additional setup below.

## The gap this guide exists for

`cargo test --workspace` on Linux does not compile `cfg(target_os = "macos")` or `cfg(windows)`
code **at all**. It reports clean without having looked at it. If you changed a platform module,
the gates above are close to vacuous for the thing you changed, and nothing in their output says so.

The fix is to name the target.

## Cover the Windows code from Linux

Desktop recording builds bundled C++ sources, including during metadata checks. Install a
MinGW C++ cross toolchain (Debian/Ubuntu: `g++-mingw-w64-x86-64`) and configure the target tools. Native
Windows builds use the installed Visual C++ toolchain.

```bash
rustup target add x86_64-pc-windows-gnu
export CC_x86_64_pc_windows_gnu=x86_64-w64-mingw32-gcc
export CXX_x86_64_pc_windows_gnu=x86_64-w64-mingw32-g++
export AR_x86_64_pc_windows_gnu=x86_64-w64-mingw32-ar

cargo clippy --target x86_64-pc-windows-gnu --workspace --all-targets --locked -- -D warnings
```

That needs no Windows machine: `cargo check` and `cargo clippy` emit Rust metadata without linking
a final executable, while native dependencies still need their target compiler and archiver.

To actually *run* the Windows-target tests you need a cross-linker and wine (Debian/Ubuntu:
`sudo apt install g++-mingw-w64-x86-64 wine`):

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
- **A cache hit is valid for unchanged inputs.** Cargo recompiles changed inputs. Use a separate
  `CARGO_TARGET_DIR` when isolating a cross-target experiment; deleting the cache is not a verification requirement.

## Cover the macOS code from Linux

The bundled encoder also needs a Darwin C++ cross toolchain and SDK. If those are unavailable,
run the checks on a Mac; adding the Rust target alone is insufficient.

```bash
rustup target add x86_64-apple-darwin

cargo clippy --target x86_64-apple-darwin --workspace --lib --locked -- -D warnings
```

This compiles the `cfg(target_os = "macos")` modules for real — a type error inside one fails the
command from Linux.

The private-session bridge is Objective-C and compiles only on a Mac. Cross-host metadata checks
still check its Rust declarations and callers, but do not compile or validate the native bridge.
Use a native macOS build and tests for changes to `private_session.m` or its ABI.

`--lib` avoids pulling every test/benchmark dependency into the cross-check. `--all-targets`
can additionally compile C dependencies such as `alloca`; these also need a correctly configured
Darwin toolchain. To cover tests and benches for one crate, scope the check:

```bash
cargo clippy --target x86_64-apple-darwin -p glass-macos --all-targets --locked -- -D warnings
```

Cross-checking is not a native link or runtime test. Run on a Mac, or use macOS CI, for that evidence.

For the opt-in event and owned-window inspection tools, see
[Run the macOS native diagnostics](run-macos-diagnostics.md). These are read-only diagnostics,
not evidence of delivered input or desktop isolation.

## Integration suites (Linux)

The GUI integration jobs use Ubuntu 24.04 with GTK 4.14. Their role and rendering assertions depend
on that toolkit baseline; the portable build/lint/unit job continues to use `ubuntu-latest`.
Newer GTK stacks need separate runtime qualification before moving the integration jobs.

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
cargo test -p glass-x11 -- --include-ignored       # needs Xvfb + AT-SPI prerequisites
cargo test -p glass-wayland -- --include-ignored   # needs sway >= 1.12, Mesa, Xwayland
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

Choose the script for the area changed. Some scripts skip when prerequisites are absent; inspect
the output and test counts before treating an exit status of zero as coverage.

| script | what it does |
|---|---|
| `test-x11.sh` | X11 integration suite; self-starts Xvfb |
| `test-wayland.sh` | Wayland suite; needs sway >= 1.12 |
| `test-a11y.sh` | AT-SPI suite; private bus + registry |
| `test-macos.sh` | glass-macos suite; exits 0 off macOS, so it is safe to call anywhere |
| `test-macos-a11y.sh` | compile/link gate for the macOS a11y integration test |
| `test-macos-mcp.sh` | headless stdio MCP smoke — server boots and lists its tools |
| `test-macos-packaging.sh` | signed bundle/disk-image permission checks on macOS |
| `test-macos-account.sh` | account API lifecycle checks; disposable GitHub-hosted macOS runners only |
| `test-android.sh` | attached emulator suites; `--build-only` compiles without a device |
| `test-android-lifecycle.sh` | managed emulator boot/teardown; begin without a running emulator |
| **`test-windows.sh`** | **drives a REMOTE Windows box over SSH**; skips cleanly when none is configured |
| `sandbox-xvfb.sh` | manage the sandbox X display glass-mcp drives |
| `bench.sh` | run the benchmarks, or flamegraph one |
| `coverage.sh` | coverage via cargo-llvm-cov + cargo-nextest |
| `mutants.sh` | mutation-test glass-core (CI does this; see the table above) |
| `build-bundle.sh` / `build-musl.sh` | local Linux bundles / the static Linux release binary |
| `verification-cost.sh` | measure what the verification loop costs |
| `x11-geometry-settle-measurement.sh` | one-off X11 launch-geometry measurement |
| `wayland-geometry-settle-measurement.sh` | the Wayland counterpart |
