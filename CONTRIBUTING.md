# Contributing to Glass

## Build and run

[Build from source](docs/how-to/build-from-source.md). The toolchain is pinned in
`rust-toolchain.toml`, so rustup installs it on the first build — there is nothing to choose.

## Before you open a PR

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
```

Then run `cargo test --workspace --locked` on Linux or Windows. On macOS, run
`./scripts/test-macos.sh` and `cargo test --workspace --lib --locked`; native integration binaries
need a separately granted GUI context. The [build prerequisites](docs/how-to/build-from-source.md)
include C/C++ compilers and CMake. No live display or device is needed for these ordinary gates.

**If you changed platform code, the host gates alone are not enough.** `cargo test --workspace` on Linux does
not compile `cfg(target_os = "macos")` or `cfg(windows)` modules at all — it reports clean without
having looked at them. [Verify a change](docs/how-to/verify-a-change.md) gives the per-target
commands and native dependencies needed to cover them.

## What CI runs

Linux, macOS and Windows each run build, clippy and test across the whole workspace, plus the X11,
Wayland and AT-SPI integration suites on Linux. The macOS runtime test gate uses `--lib`; CI also
builds the native integration targets without running them. Android has a separate emulator workflow.
Mutation testing runs sharded on CI and should not
be run locally — it saturates the machine.

## House style

- **No silent fallbacks.** A failed capture or input returns a structured error, never a blank or
  stale frame.
- **`glass-core` stays platform-agnostic.** OS types live behind the `Platform` seam.
- **Avoid `unsafe`.** Where it is unavoidable, isolate it and document each site with `// SAFETY:`.
- **Permissively-licensed dependencies only** (MIT/Apache; no copyleft).
- **Comments carry a fact, not an argument** — one sentence with something the code cannot say for
  itself.

`AGENTS.md` holds the same invariants in the form an agent working in this repo needs.

## Documentation changes

The [documentation index](docs/README.md) separates tutorials (learning by doing), how-to guides
(completing a task), reference (contracts and facts), and explanations (understanding the model).
Keep each page focused on one need and link to the others for detail.

Check examples against the current CLI, tool schemas and implementation. Use `glass-mcp tools --json`
and `glass-mcp env --json` to inspect the actual surfaces. Link new pages from the index, preserve
useful existing anchors when moving content, and keep release-archive READMEs usable without a checkout.
Record behaviour changes under `[Unreleased]` in `CHANGELOG.md`; a documentation-only refresh needs
no release entry. Public docs describe product behaviour, without internal plans or strategy.

## Licensing

Apache-2.0. By contributing you agree your contributions are licensed under it.
