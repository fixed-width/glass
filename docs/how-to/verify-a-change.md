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

## Integration suites (Linux)

`#[ignore]`d, so the ordinary `cargo test` never starts them. Each self-starts what it needs.

```bash
./scripts/test-x11.sh [name]        # X11 suite; starts its own Xvfb
./scripts/test-wayland.sh [name]    # Wayland suite; needs sway >= 1.12
./scripts/test-a11y.sh [name]       # AT-SPI suite; starts a private bus + registry
```

Pass a substring to run one test.

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
