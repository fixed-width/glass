//! Benchmark the per-pixel frame diff (and the StabilityTracker wrapper that
//! wait_stable runs every polling interval).

mod common;

use common::{gradient, with_changed, SIZES};
use criterion::{criterion_group, criterion_main, BatchSize, BenchmarkId, Criterion, Throughput};
use glass_core::{diff, diff_perceptual, StabilityTracker};
use std::hint::black_box;

fn bench_diff(c: &mut Criterion) {
    let mut g = c.benchmark_group("diff");
    for &(w, h) in SIZES {
        let size = format!("{w}x{h}");
        let base = gradient(w, h);
        let scenarios = [
            ("identical", base.clone()),
            ("changed_1pct", with_changed(&base, 0.01)),
            ("changed_full", with_changed(&base, 1.0)),
        ];
        g.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        for (name, other) in &scenarios {
            g.bench_with_input(BenchmarkId::new(*name, &size), other, |b, other| {
                b.iter(|| black_box(diff(black_box(&base), black_box(other), 0).unwrap()));
            });
        }
    }
    g.finish();
}

fn bench_diff_perceptual(c: &mut Criterion) {
    let mut g = c.benchmark_group("diff_perceptual");
    for &(w, h) in SIZES {
        let size = format!("{w}x{h}");
        let base = gradient(w, h);
        // The byte-identical SIMD pre-scan skips unchanged chunks, so cost scales
        // with the changed area (where the per-pixel YIQ + anti-alias work runs).
        let scenarios = [
            ("identical", base.clone()),
            ("changed_1pct", with_changed(&base, 0.01)),
            ("changed_full", with_changed(&base, 1.0)),
        ];
        g.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        for (name, other) in &scenarios {
            g.bench_with_input(BenchmarkId::new(*name, &size), other, |b, other| {
                b.iter(|| {
                    black_box(diff_perceptual(black_box(&base), black_box(other), 0.1).unwrap())
                });
            });
        }
    }
    g.finish();
}

fn bench_observe(c: &mut Criterion) {
    let mut g = c.benchmark_group("stability_observe");
    for &(w, h) in SIZES {
        let size = format!("{w}x{h}");
        let frame = gradient(w, h);
        g.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        // observe() consumes the Frame, so clone it in setup (unmeasured) and
        // measure only the observe call (diff + O(1) bookkeeping).
        g.bench_with_input(BenchmarkId::new("stable", &size), &frame, |b, frame| {
            b.iter_batched(
                || {
                    let mut t = StabilityTracker::new(2, 0);
                    t.observe(frame.clone()).unwrap(); // prime `last`
                    (t, frame.clone())
                },
                |(mut t, f)| black_box(t.observe(f).unwrap()),
                BatchSize::PerIteration,
            );
        });
    }
    g.finish();
}

criterion_group!(benches, bench_diff, bench_diff_perceptual, bench_observe);
criterion_main!(benches);
