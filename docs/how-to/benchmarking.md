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

Profile a hot path as a flamegraph (needs
[`cargo install flamegraph`](https://github.com/flamegraph-rs/flamegraph) and
`kernel.perf_event_paranoid <= 1`):

```bash
./scripts/bench.sh diff "identical/1920x1080"   # writes flamegraph.svg
```
