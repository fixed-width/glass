# Contributing to glass

## Build and run

[Build from source](docs/how-to/build-from-source.md). The toolchain is pinned in
`rust-toolchain.toml`, so rustup installs it on the first build — there is nothing to choose.

## Before you open a PR

```bash
cargo fmt --all --check
cargo clippy --workspace --all-targets --locked -- -D warnings
cargo test --workspace --locked
```

**If you changed platform code, those three are not enough.** `cargo test --workspace` on Linux does
not compile `cfg(target_os = "macos")` or `cfg(windows)` modules at all — it reports clean without
having looked at them. [Verify a change](docs/how-to/verify-a-change.md) gives the per-target
commands that do cover them, most of which run from any host.

## What CI runs

Linux, macOS and Windows each run build, clippy and test across the whole workspace, plus the X11,
Wayland and AT-SPI integration suites on Linux. Mutation testing runs sharded on CI and should not
be run locally — it saturates the machine.

## House style

- **No silent fallbacks.** A failed capture or input returns a structured error, never a blank or
  stale frame.
- **`glass-core` stays platform-agnostic.** OS types live behind the `Platform` seam.
- **Avoid `unsafe`.** Where it is unavoidable, isolate it and document each site with `// SAFETY:`.
- **Permissively-licensed dependencies only** (MIT/Apache; no copyleft).
- **Comments carry a fact, not an argument** — one sentence with something the code cannot say for
  itself.

`CLAUDE.md` holds the same invariants in the form an agent working in this repo needs.

## Licensing

Apache-2.0. By contributing you agree your contributions are licensed under it.
