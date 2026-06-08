//! Benchmark WebP encode/decode (per screenshot / baseline). Uses gradient and
//! noise content — a *solid* frame would compress unrealistically fast.

mod common;

use common::{gradient, noise, SIZES};
use criterion::{criterion_group, criterion_main, BenchmarkId, Criterion, Throughput};
use glass_core::{frame_from_webp, frame_to_webp};
use std::hint::black_box;

fn bench_encode(c: &mut Criterion) {
    let mut g = c.benchmark_group("webp_encode");
    for &(w, h) in SIZES {
        let size = format!("{w}x{h}");
        g.throughput(Throughput::Bytes(u64::from(w) * u64::from(h) * 4));
        for (kind, frame) in [("gradient", gradient(w, h)), ("noise", noise(w, h))] {
            g.bench_with_input(BenchmarkId::new(kind, &size), &frame, |b, f| {
                b.iter(|| black_box(frame_to_webp(f).unwrap()));
            });
        }
    }
    g.finish();
}

fn bench_decode(c: &mut Criterion) {
    let mut g = c.benchmark_group("webp_decode");
    for &(w, h) in SIZES {
        let size = format!("{w}x{h}");
        let webp = frame_to_webp(&noise(w, h)).unwrap();
        g.throughput(Throughput::Bytes(u64::from(w) * u64::from(h) * 4));
        g.bench_with_input(BenchmarkId::new("noise", &size), &webp, |b, webp| {
            b.iter(|| black_box(frame_from_webp(webp).unwrap()));
        });
    }
    g.finish();
}

criterion_group!(benches, bench_encode, bench_decode);
criterion_main!(benches);
