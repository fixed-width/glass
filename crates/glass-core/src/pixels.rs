//! Pixel-format normalization shared by the capture backends.
//!
//! Every backend captures a 32-bit-per-pixel buffer whose channel order is one of
//! two layouts and whose alpha byte is unreliable. The one common per-pixel step —
//! swap R/B when needed, force alpha opaque — lives here, once, with a portable
//! SIMD fast path (`u8x32`, 8 pixels per step) and a scalar tail. Backends keep
//! their own validation, stride handling, and buffer allocation; they call in here
//! for the hot loop so the vectorized kernel exists in exactly one place.

use fearless_simd::{dispatch, prelude::*, u8x32};

const LANES: usize = 32; // 8 pixels per SIMD chunk (4 bytes each)

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
#[inline(always)]
fn alpha_mask<S: Simd>(simd: S) -> u8x32<S> {
    u8x32::simd_from(
        simd,
        [
            0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, //
            0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255,
        ],
    )
}

/// Swizzle one 8-pixel chunk and force its alpha opaque. `SWAP` is a const
/// generic so the unused branch is dropped at monomorphization.
#[inline(always)]
fn swizzle_chunk<S: Simd, const SWAP: bool>(v: u8x32<S>, alpha: u8x32<S>) -> u8x32<S> {
    if SWAP {
        v.swizzle_dyn_within_blocks(u8x32::simd_from(
            v.simd,
            [
                2, 1, 0, 3, 6, 5, 4, 7, 10, 9, 8, 11, 14, 13, 12, 15, 2, 1, 0, 3, 6, 5, 4, 7, 10,
                9, 8, 11, 14, 13, 12, 15,
            ],
        )) | alpha
    } else {
        v | alpha
    }
}

#[inline(always)]
fn convert<S: Simd, const SWAP: bool>(simd: S, src: &[u8], dst: &mut [u8]) {
    let dst = &mut dst[..src.len()];
    let end = src.len() / LANES * LANES;
    let alpha = alpha_mask(simd);
    let mut off = 0;
    while off < end {
        let v = u8x32::from_slice(simd, &src[off..off + LANES]);
        swizzle_chunk::<S, SWAP>(v, alpha).store_slice(&mut dst[off..off + LANES]);
        off += LANES;
    }
    // Scalar tail (< 8 pixels). A trailing run shorter than one pixel is left
    // untouched, matching the per-backend buffers (always whole `w*h*4` pixels).
    debug_assert!(
        src.len() - off < LANES,
        "SIMD loop must consume every full chunk"
    );
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

#[inline(always)]
fn convert_in_place<S: Simd, const SWAP: bool>(simd: S, buf: &mut [u8]) {
    #[cfg(any(target_arch = "x86", target_arch = "x86_64"))]
    let buf = if !SWAP && simd.level().as_avx2().is_some() {
        // Align whole pixels to avoid split AVX2 accesses on 16-byte-aligned buffers.
        let prefix = (buf.as_ptr().align_offset(LANES) & !3).min(buf.len() / 4 * 4);
        debug_assert!(
            prefix < LANES,
            "alignment prefix must be shorter than one SIMD chunk"
        );
        let (head, rest) = buf.split_at_mut(prefix);
        let alpha = fearless_simd::u8x16::simd_from(
            simd,
            [0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255, 0, 0, 0, 255],
        );
        let (chunks, tail) = head.as_chunks_mut::<16>();
        for chunk in chunks {
            (fearless_simd::u8x16::from_slice(simd, chunk) | alpha).store_slice(chunk);
        }
        for pixel in tail.as_chunks_mut::<4>().0 {
            pixel[3] = 255;
        }
        rest
    } else {
        buf
    };
    let alpha = alpha_mask(simd);
    let (chunks, tail) = buf.as_chunks_mut::<LANES>();
    for chunk in chunks {
        let v = u8x32::from_slice(simd, chunk);
        swizzle_chunk::<S, SWAP>(v, alpha).store_slice(chunk);
    }
    for pixel in tail.as_chunks_mut::<4>().0 {
        if SWAP {
            pixel.swap(0, 2);
        }
        pixel[3] = 255;
    }
}

/// Convert a tightly packed 32-bit `src` into opaque RGBA in `dst`.
///
/// `src` and `dst` must be the same length. Any trailing bytes that don't form a
/// whole 4-byte pixel are left untouched. Every alpha byte is forced to 255.
pub fn to_opaque_rgba(src: &[u8], dst: &mut [u8], order: SourceOrder) {
    debug_assert_eq!(src.len(), dst.len(), "src and dst must be the same length");
    // NEON is the baseline on these targets; direct dispatch also keeps tiny rows cheap.
    #[cfg(all(target_arch = "aarch64", target_feature = "neon"))]
    if let Some(simd) = fearless_simd::Level::baseline().as_neon() {
        match order {
            SourceOrder::Bgr => convert::<_, true>(simd, src, dst),
            SourceOrder::Rgb => convert::<_, false>(simd, src, dst),
        }
        return;
    }
    match order {
        SourceOrder::Bgr => {
            dispatch!(crate::simd_level(), simd => convert::<_, true>(simd, src, dst))
        }
        SourceOrder::Rgb => {
            dispatch!(crate::simd_level(), simd => convert::<_, false>(simd, src, dst))
        }
    }
}

/// In-place variant of [`to_opaque_rgba`]: rewrite a tightly packed 32-bit `buf`
/// to opaque RGBA, swapping R/B per `order` and forcing every alpha byte to 255.
pub fn to_opaque_rgba_in_place(buf: &mut [u8], order: SourceOrder) {
    match order {
        SourceOrder::Bgr => {
            dispatch!(crate::simd_level(), simd => convert_in_place::<_, true>(simd, buf))
        }
        SourceOrder::Rgb => {
            dispatch!(crate::simd_level(), simd => convert_in_place::<_, false>(simd, buf))
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Scalar reference: swap R/B per `order`, force alpha to 255.
    fn reference(data: &[u8], order: SourceOrder) -> Vec<u8> {
        data.as_chunks::<4>()
            .0
            .iter()
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

    #[test]
    fn simd_backends_match_scalar_at_every_alignment_and_tail() {
        for level in [fearless_simd::Level::baseline(), crate::simd_level()] {
            for alignment in 0usize..64 {
                for len in (0usize..1028).chain([4095, 4096, 4097, 8191, 8192, 8193]) {
                    for order in [SourceOrder::Rgb, SourceOrder::Bgr] {
                        let mut actual: Vec<u8> = (0..len + 128)
                            .map(|i| i.wrapping_mul(73).rotate_left((i % 13) as u32) as u8)
                            .collect();
                        let offset = actual.as_ptr().align_offset(64) + alignment;
                        let src = &actual[offset..offset + len];
                        let converted = reference(src, order);
                        let mut expected = actual.clone();
                        expected[offset..offset + converted.len()].copy_from_slice(&converted);

                        let mut out = vec![42; len + 128];
                        let dst_offset = out.as_ptr().align_offset(64) + (alignment * 13 + 7) % 64;
                        let mut expected_out = out.clone();
                        expected_out[dst_offset..dst_offset + converted.len()]
                            .copy_from_slice(&converted);
                        let dst = &mut out[dst_offset..dst_offset + len];
                        match order {
                            SourceOrder::Bgr => {
                                dispatch!(level, simd => convert::<_, true>(simd, src, dst))
                            }
                            SourceOrder::Rgb => {
                                dispatch!(level, simd => convert::<_, false>(simd, src, dst))
                            }
                        }
                        assert_eq!(
                            out, expected_out,
                            "out-of-place {level:?} alignment={alignment} len={len} {order:?}"
                        );

                        let buf = &mut actual[offset..offset + len];
                        match order {
                            SourceOrder::Bgr => {
                                dispatch!(level, simd => convert_in_place::<_, true>(simd, buf))
                            }
                            SourceOrder::Rgb => {
                                dispatch!(level, simd => convert_in_place::<_, false>(simd, buf))
                            }
                        }
                        assert_eq!(
                            actual, expected,
                            "in-place {level:?} alignment={alignment} len={len} {order:?}"
                        );
                    }
                }
            }
        }
    }
}
