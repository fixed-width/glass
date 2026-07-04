//! Benchmark the per-capture BGRX→RGBA conversion.

use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use glass_x11::pixels::xdata_to_rgba;
use std::hint::black_box;

const SIZES: &[(u32, u32)] = &[(320, 240), (800, 600), (1920, 1080)];

/// A deterministic raw BGRX buffer (4 bytes/pixel), no `rand` dependency.
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

criterion_group!(benches, bench);
criterion_main!(benches);
