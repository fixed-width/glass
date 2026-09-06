# Benchmark and profile the hot paths

Per-frame hot-path micro-benchmarks ([criterion](https://github.com/bheisler/criterion.rs)) live in
`crates/*/benches/`. This is contributor tooling — you don't need it to use glass.

```bash
# core (diff, webp encode/decode) plus the per-backend pixel conversions
PKGS="-p glass-core -p glass-x11 -p glass-windows -p glass-wayland"
cargo bench $PKGS                          # run all
cargo bench $PKGS -- --save-baseline main  # save a baseline, then compare after a change:
cargo bench $PKGS -- --baseline main
```

`glass-core`, `glass-x11`, `glass-windows`, and `glass-wayland` carry benchmarks; their libs set
`bench = false` so `cargo bench` runs the criterion targets rather than the unit-test harness (which
would reject criterion's `--save-baseline` / `--baseline` flags). The `pixels` bench exists in all
three backends, so name the crate with `-p` to flamegraph one.

Pixel normalization and frame comparison use `fearless_simd`, with CPU features detected once
and cached. On x86 this selects the available SSE2, SSE4.2, AVX2, or AVX-512 backend; AArch64 uses NEON.
The core SIMD code works on stable Rust. The workspace still pins nightly for the Windows
clipboard shim's `retour` static detours.

When comparing portable builds, clear local CPU-specific flags (such as `target-cpu=native` or
`target-cpu=haswell`), so both builds target the same baseline:

```bash
env -u CARGO_ENCODED_RUSTFLAGS -u CARGO_BUILD_RUSTFLAGS RUSTFLAGS='' \
  cargo +stable bench -p glass-core --locked
```

Profile a hot path as a flamegraph (needs
[`cargo install flamegraph`](https://github.com/flamegraph-rs/flamegraph) and
`kernel.perf_event_paranoid <= 1`):

```bash
./scripts/bench.sh diff "identical/1920x1080"   # writes flamegraph.svg
```
