//! Benchmark the per-capture BGRX→RGBA conversion.

// Gated per-item, not with a single `#![cfg]`: that would empty the crate root and leave
// no `fn main` (`criterion_main!` below provides it only on Linux, where the dependency
// itself is present). The stub `main` at the bottom keeps the target linkable elsewhere.

#[cfg(target_os = "linux")]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(target_os = "linux")]
use glass_x11::pixels::xdata_to_rgba;
#[cfg(target_os = "linux")]
use std::hint::black_box;

#[cfg(target_os = "linux")]
const SIZES: &[(u32, u32)] = &[(320, 240), (800, 600), (1920, 1080)];

/// A deterministic raw BGRX buffer (4 bytes/pixel), no `rand` dependency.
#[cfg(target_os = "linux")]
fn bgrx(w: u32, h: u32) -> Vec<u8> {
    let n = (w as usize) * (h as usize);
    let mut d = Vec::with_capacity(n * 4);
    for i in 0..n {
        let v = (i as u32).wrapping_mul(2_654_435_761); // Knuth multiplicative hash
        d.push(v as u8);
        d.push((v >> 8) as u8);
        d.push((v >> 16) as u8);
        d.push(0);
    }
    d
}

#[cfg(target_os = "linux")]
fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("xdata_to_rgba");
    for &(w, h) in SIZES {
        let data = bgrx(w, h);
        g.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        g.bench_with_input(
            BenchmarkId::new("convert", format!("{w}x{h}")),
            &data,
            |b, data| {
                b.iter(|| black_box(xdata_to_rgba(black_box(data), w, h, 4).unwrap()));
            },
        );
    }
    g.finish();
}

#[cfg(target_os = "linux")]
criterion_group!(benches, bench);
#[cfg(target_os = "linux")]
criterion_main!(benches);

#[cfg(not(target_os = "linux"))]
fn main() {}
