//! Benchmark the per-capture BGRA→RGBA conversion (in place).
//!
//! The pixel module is cross-platform (no OS calls), so this runs on the Linux
//! dev box even though the WGC capture path it feeds is Windows-only.

use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use glass_windows::pixels::bgra_to_rgba;
use std::hint::black_box;

const SIZES: &[(u32, u32)] = &[(320, 240), (800, 600), (1920, 1080)];

/// A deterministic raw BGRA buffer (4 bytes/pixel), no `rand` dependency.
fn bgra(w: u32, h: u32) -> Vec<u8> {
    let n = (w as usize) * (h as usize);
    let mut d = Vec::with_capacity(n * 4);
    for i in 0..n {
        let v = (i as u32).wrapping_mul(2_654_435_761); // Knuth multiplicative hash
        d.push(v as u8);
        d.push((v >> 8) as u8);
        d.push((v >> 16) as u8);
        d.push((v >> 24) as u8);
    }
    d
}

fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("bgra_to_rgba");
    for &(w, h) in SIZES {
        let data = bgra(w, h);
        g.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        // Conversion is in place, so clone per iteration (the clone is setup, not timed).
        g.bench_with_input(
            BenchmarkId::new("convert", format!("{w}x{h}")),
            &data,
            |b, data| {
                b.iter_batched(
                    || data.clone(),
                    |mut buf| {
                        bgra_to_rgba(black_box(&mut buf));
                        buf
                    },
                    BatchSize::LargeInput,
                );
            },
        );
    }
    g.finish();
}

criterion_group!(benches, bench);
criterion_main!(benches);
