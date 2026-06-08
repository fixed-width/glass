# CLAUDE.md

Guidance for Claude Code working in the **glass** open-core Rust workspace.

## What glass is

A Rust **MCP server** giving an AI agent a closed **build → see → interact → debug** loop
over external native GUI apps: launch, capture (lossless WebP), inject mouse/keyboard,
read logs, diff against baselines, wait-until-stable, and read/drive the accessibility
tree — served over MCP (stdio, or `serve --http`). Backends behind a `Platform` seam: X11
and Wayland (headless sway) on Linux, Windows (WGC/SendInput); macOS planned.

## Layout

Cargo workspace at the repo root. Crates: `glass-core` (platform-agnostic heart — the
`Platform`/`Accessibility` seams, session, `Frame`, diff, stability, log buffer), the
backends (`glass-x11`, `glass-wayland`, `glass-windows`), the a11y readers
(`glass-a11y-linux`, `glass-a11y-windows`), `glass-sandbox-linux`, the `glass-mcp` server
binary, and the `glass-testapp` fixture.

## Commands

```bash
cargo build
cargo test --workspace                    # unit tests (integration tests are #[ignore]d)
cargo clippy --workspace --all-targets -- -D warnings   # lint gate — keep clean
./scripts/test-x11.sh [name]              # X11 integration suite (self-starts Xvfb)
./scripts/test-wayland.sh [name]          # Wayland suite (needs sway >=1.12)
./scripts/test-a11y.sh [name]             # AT-SPI suite
```
The workspace is pinned to nightly via `rust-toolchain.toml`.

## Invariants

- **External automation only** — drive apps as a black box; never require the app to be glass-aware.
- **Keep `glass-core` platform-agnostic** — no OS types in core; every OS detail lives behind `Platform`.
- **No silent fallbacks** — a failed capture/input returns a structured error, never a blank/stale frame.
- **Coordinates are window-relative** at the tool boundary; only the backend maps to global coords.
- **Permissively-licensed deps only** (MIT/Apache; no copyleft).
- **Avoid `unsafe`** — prefer safe abstractions; isolate + document any required `unsafe` with `// SAFETY:`.

## Licensing

Open core, **Apache-2.0**. See `LICENSE-APACHE`.
