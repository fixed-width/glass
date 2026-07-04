use glass_core::pixels::{to_opaque_rgba, SourceOrder};
use glass_core::{GlassError, Result};

/// Convert raw `GetImage` ZPixmap data to tightly packed RGBA8.
///
/// Assumes the common Xvfb/desktop case: depth 24, 32 bits per pixel, LSBFirst
/// byte order, so each source pixel is `[B, G, R, pad]`. `bytes_per_pixel` must
/// be 4; anything else errors rather than guessing (no silent fallback). The
/// per-pixel swizzle (BGRX→RGBA, alpha forced opaque) is the shared SIMD kernel
/// in [`glass_core::pixels`].
pub fn xdata_to_rgba(
    data: &[u8],
    width: u32,
    height: u32,
    bytes_per_pixel: usize,
) -> Result<Vec<u8>> {
    if bytes_per_pixel != 4 {
        return Err(GlassError::Backend(format!(
            "unsupported bits-per-pixel: {} (only 32bpp depth-24 TrueColor is supported)",
            bytes_per_pixel * 8
        )));
    }
    let pixels = width as usize * height as usize;
    let needed = pixels * 4;
    if data.len() < needed {
        return Err(GlassError::CaptureFailed(format!(
            "image data is {} bytes, need at least {} for {}x{}",
            data.len(),
            needed,
            width,
            height
        )));
    }
    let mut out = vec![0u8; needed];
    to_opaque_rgba(&data[..needed], &mut out, SourceOrder::Bgr);
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_bgrx_to_rgba() {
        // one pixel, source B=10 G=20 R=30 pad=0
        let out = xdata_to_rgba(&[10, 20, 30, 0], 1, 1, 4).unwrap();
        assert_eq!(out, vec![30, 20, 10, 255]);
    }

    #[test]
    fn rejects_non_32bpp() {
        assert!(matches!(
            xdata_to_rgba(&[0, 0, 0], 1, 1, 3).unwrap_err(),
            GlassError::Backend(_)
        ));
    }

    #[test]
    fn rejects_short_buffer() {
        assert!(matches!(
            xdata_to_rgba(&[0, 0, 0, 0], 2, 2, 4).unwrap_err(),
            GlassError::CaptureFailed(_)
        ));
    }

    /// Scalar reference: BGRX -> RGBA with alpha forced to 255.
    fn reference(data: &[u8]) -> Vec<u8> {
        data.chunks_exact(4)
            .flat_map(|p| [p[2], p[1], p[0], 255])
            .collect()
    }

    #[test]
    fn simd_matches_scalar_reference() {
        // Pixel counts including non-multiples of 8 (the SIMD chunk) + degenerate.
        for &pixels in &[0usize, 1, 7, 8, 9, 13, 16, 31, 64, 1000] {
            let data: Vec<u8> = (0..pixels * 4)
                .map(|i| (i as u32).wrapping_mul(2_654_435_761) as u8)
                .collect();
            let (w, h) = if pixels == 0 {
                (0u32, 0u32)
            } else {
                (pixels as u32, 1u32)
            };
            let got = xdata_to_rgba(&data, w, h, 4).unwrap();
            assert_eq!(got, reference(&data), "pixels={pixels}");
        }
    }
}
