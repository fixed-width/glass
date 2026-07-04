//! Pixel-format normalization shared by the capture backends.
//!
//! Every backend captures a 32-bit-per-pixel buffer whose channel order is one of
//! two layouts and whose alpha byte is unreliable. The one common per-pixel step —
//! swap R/B when needed, force alpha opaque — lives here, once, with a portable
//! SIMD fast path (`u8x32`, 8 pixels per step) and a scalar tail. Backends keep
//! their own validation, stride handling, and buffer allocation; they call in here
//! for the hot loop so the vectorized kernel exists in exactly one place.

use std::simd::{simd_swizzle, u8x32};

const LANES: usize = 32; // 8 pixels per SIMD chunk (4 bytes each)

/// `[B,G,R,_]` -> `[R,G,B,_]`: swap bytes 0 and 2 within each 4-byte pixel.
const SWAP_RB: [usize; LANES] = [
    2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, //
    18, 17, 16, 19, 22, 21, 20, 23, 26, 25, 24, 27, 30, 29, 28, 31,
];
/// Identity: source is already `[R,G,B,_]`, only alpha needs forcing.
const IDENTITY: [usize; LANES] = [
    0, 1, 2, 3, 4, 5, 6, 7, 8, 9, 10, 11, 12, 13, 14, 15, //
    16, 17, 18, 19, 20, 21, 22, 23, 24, 25, 26, 27, 28, 29, 30, 31,
];

/// Channel order of a 32-bit source pixel relative to the RGBA target.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum SourceOrder {
    /// `[B, G, R, _]` — R and B are swapped vs. RGBA (X11 ZPixmap, WGC BGRA,
    /// wlroots `Xrgb8888`/`Argb8888`).
    Bgr,
    /// `[R, G, B, _]` — already in RGBA channel order (wlroots `Xbgr8888`/`Abgr8888`).
    Rgb,
}

/// OR-ing this onto a chunk forces each pixel's alpha lane to 255
/// (`pad | 255 == 255`) and leaves R/G/B untouched (`x | 0 == x`).
#[inline]
fn alpha_mask() -> u8x32 {
    u8x32::from_array([
        0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, //
        0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255,
    ])
}

/// Swizzle one 8-pixel chunk and force its alpha opaque. `SWAP` is a const
/// generic so the unused branch is dropped at monomorphization.
#[inline]
fn swizzle_chunk<const SWAP: bool>(v: u8x32, alpha: u8x32) -> u8x32 {
    if SWAP {
        simd_swizzle!(v, SWAP_RB) | alpha
    } else {
        simd_swizzle!(v, IDENTITY) | alpha
    }
}

fn convert<const SWAP: bool>(src: &[u8], dst: &mut [u8]) {
    let alpha = alpha_mask();
    let mut off = 0;
    while off + LANES <= src.len() {
        let v = u8x32::from_slice(&src[off..off + LANES]);
        swizzle_chunk::<SWAP>(v, alpha).copy_to_slice(&mut dst[off..off + LANES]);
        off += LANES;
    }
    // Scalar tail (< 8 pixels). A trailing run shorter than one pixel is left
    // untouched, matching the per-backend buffers (always whole `w*h*4` pixels).
    while off + 4 <= src.len() {
        let (r, b) = if SWAP {
            (src[off + 2], src[off])
        } else {
            (src[off], src[off + 2])
        };
        dst[off] = r;
        dst[off + 1] = src[off + 1];
        dst[off + 2] = b;
        dst[off + 3] = 255;
        off += 4;
    }
}

fn convert_in_place<const SWAP: bool>(buf: &mut [u8]) {
    let alpha = alpha_mask();
    let mut off = 0;
    while off + LANES <= buf.len() {
        let v = u8x32::from_slice(&buf[off..off + LANES]);
        swizzle_chunk::<SWAP>(v, alpha).copy_to_slice(&mut buf[off..off + LANES]);
        off += LANES;
    }
    while off + 4 <= buf.len() {
        if SWAP {
            buf.swap(off, off + 2);
        }
        buf[off + 3] = 255;
        off += 4;
    }
}

/// Convert a tightly packed 32-bit `src` into opaque RGBA in `dst`.
///
/// `src` and `dst` must be the same length. Any trailing bytes that don't form a
/// whole 4-byte pixel are left untouched. Every alpha byte is forced to 255.
pub fn to_opaque_rgba(src: &[u8], dst: &mut [u8], order: SourceOrder) {
    debug_assert_eq!(src.len(), dst.len(), "src and dst must be the same length");
    match order {
        SourceOrder::Bgr => convert::<true>(src, dst),
        SourceOrder::Rgb => convert::<false>(src, dst),
    }
}

/// In-place variant of [`to_opaque_rgba`]: rewrite a tightly packed 32-bit `buf`
/// to opaque RGBA, swapping R/B per `order` and forcing every alpha byte to 255.
pub fn to_opaque_rgba_in_place(buf: &mut [u8], order: SourceOrder) {
    match order {
        SourceOrder::Bgr => convert_in_place::<true>(buf),
        SourceOrder::Rgb => convert_in_place::<false>(buf),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scalar reference: swap R/B per `order`, force alpha to 255.
    fn reference(data: &[u8], order: SourceOrder) -> Vec<u8> {
        data.chunks_exact(4)
            .flat_map(|p| match order {
                SourceOrder::Bgr => [p[2], p[1], p[0], 255],
                SourceOrder::Rgb => [p[0], p[1], p[2], 255],
            })
            .collect()
    }

    fn sample(pixels: usize) -> Vec<u8> {
        (0..pixels * 4)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761) as u8)
            .collect()
    }

    #[test]
    fn swizzle_matches_scalar_reference() {
        // Pixel counts straddling the 8-pixel SIMD chunk, plus degenerate 0.
        for &pixels in &[0usize, 1, 7, 8, 9, 13, 16, 31, 64, 1000] {
            let data = sample(pixels);
            for order in [SourceOrder::Bgr, SourceOrder::Rgb] {
                let mut out = vec![0u8; data.len()];
                to_opaque_rgba(&data, &mut out, order);
                assert_eq!(
                    out,
                    reference(&data, order),
                    "pixels={pixels} order={order:?}"
                );

                let mut inplace = data.clone();
                to_opaque_rgba_in_place(&mut inplace, order);
                assert_eq!(
                    inplace,
                    reference(&data, order),
                    "in-place pixels={pixels} order={order:?}"
                );
            }
        }
    }

    #[test]
    fn bgr_swaps_red_and_blue_and_forces_alpha() {
        let src = [10u8, 20, 30, 0, 40, 50, 60, 128]; // 2 px, [B,G,R,_]
        let mut out = [0u8; 8];
        to_opaque_rgba(&src, &mut out, SourceOrder::Bgr);
        assert_eq!(out, [30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn rgb_keeps_order_and_forces_alpha() {
        let src = [10u8, 20, 30, 0, 40, 50, 60, 128]; // 2 px, [R,G,B,_]
        let mut out = [0u8; 8];
        to_opaque_rgba(&src, &mut out, SourceOrder::Rgb);
        assert_eq!(out, [10, 20, 30, 255, 40, 50, 60, 255]);
    }

    #[test]
    fn in_place_leaves_trailing_partial_pixel_untouched() {
        let mut buf = vec![1u8, 2, 3, 4, 9, 9]; // one full px + 2 stray bytes
        to_opaque_rgba_in_place(&mut buf, SourceOrder::Bgr);
        assert_eq!(buf, vec![3, 2, 1, 255, 9, 9]);
    }
}
