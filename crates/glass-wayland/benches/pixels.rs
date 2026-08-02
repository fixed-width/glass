//! Benchmark the per-capture `wl_shm`→RGBA conversion, including stride handling.
//!
//! Uses a padded stride (`w*4 + PAD`) so the row loop does real work dropping
//! padding, and `Xrgb8888` so it exercises the R/B-swap path.

// Gated per-item, not with a single `#![cfg]`: that would empty the crate root and leave
// no `fn main` (`criterion_main!` below provides it only on Linux, where the dependency
// itself is present). The stub `main` at the bottom keeps the target linkable elsewhere.

#[cfg(target_os = "linux")]
use criterion::{BenchmarkId, Criterion, Throughput, criterion_group, criterion_main};
#[cfg(target_os = "linux")]
use glass_wayland::pixels::to_rgba;
#[cfg(target_os = "linux")]
use std::hint::black_box;
#[cfg(target_os = "linux")]
use wayland_client::protocol::wl_shm::Format;

#[cfg(target_os = "linux")]
const SIZES: &[(u32, u32)] = &[(320, 240), (800, 600), (1920, 1080)];
#[cfg(target_os = "linux")]
const PAD: u32 = 16; // extra stride padding per row

/// A deterministic strided source buffer (4 bytes/pixel + row padding).
#[cfg(target_os = "linux")]
fn buffer(h: u32, stride: u32) -> Vec<u8> {
    let mut d = vec![0u8; (stride as usize) * (h as usize)];
    for (i, b) in d.iter_mut().enumerate() {
        *b = (i as u32).wrapping_mul(2_654_435_761) as u8; // Knuth multiplicative hash
    }
    d
}

#[cfg(target_os = "linux")]
fn bench(c: &mut Criterion) {
    let mut g = c.benchmark_group("wl_to_rgba");
    for &(w, h) in SIZES {
        let stride = w * 4 + PAD;
        let data = buffer(h, stride);
        g.throughput(Throughput::Elements(u64::from(w) * u64::from(h)));
        g.bench_with_input(
            BenchmarkId::new("xrgb8888", format!("{w}x{h}")),
            &data,
            |b, data| {
                b.iter(|| {
                    black_box(to_rgba(black_box(data), Format::Xrgb8888, w, h, stride).unwrap())
                });
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
