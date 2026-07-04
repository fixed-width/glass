//! Deterministic frame generators shared by the glass-core benches. No `rand`,
//! so benchmark inputs are reproducible. `#![allow(dead_code)]` because each
//! bench binary uses only a subset.
#![allow(dead_code)]

use glass_core::Frame;

pub const SIZES: &[(u32, u32)] = &[(320, 240), (800, 600), (1920, 1080)];

/// Smooth gradient — mid-range compressibility.
pub fn gradient(w: u32, h: u32) -> Frame {
    let mut px = Vec::with_capacity((w as usize) * (h as usize) * 4);
    for y in 0..h {
        for x in 0..w {
            px.push((x & 0xFF) as u8);
            px.push((y & 0xFF) as u8);
            px.push(((x + y) & 0xFF) as u8);
            px.push(255);
        }
    }
    Frame::new(w, h, px).unwrap()
}

/// Deterministic pseudo-noise — worst case for image compression.
pub fn noise(w: u32, h: u32) -> Frame {
    let n = (w as usize) * (h as usize);
    let mut px = Vec::with_capacity(n * 4);
    for i in 0..n {
        let v = (i as u32).wrapping_mul(2_654_435_761);
        px.push(v as u8);
        px.push((v >> 8) as u8);
        px.push((v >> 16) as u8);
        px.push(255);
    }
    Frame::new(w, h, px).unwrap()
}

/// `base` with roughly `fraction` (0.0..=1.0) of pixels altered.
pub fn with_changed(base: &Frame, fraction: f64) -> Frame {
    let mut f = base.clone();
    let total = f.pixel_count() as usize;
    let stride = if fraction >= 1.0 {
        1
    } else {
        (1.0 / fraction).max(1.0) as usize
    };
    let mut i = 0;
    while i < total {
        f.pixels[i * 4] ^= 0xFF; // flip this pixel's red channel
        i += stride;
    }
    f
}
