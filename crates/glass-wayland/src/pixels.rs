use glass_core::pixels::{to_opaque_rgba, SourceOrder};
use glass_core::{GlassError, Result};
use wayland_client::protocol::wl_shm::Format;

/// Convert a 32-bit `wl_shm` screencopy buffer to tightly-packed RGBA, forcing
/// alpha opaque and honoring row `stride` padding. Handles both channel orders
/// wlroots compositors emit:
/// - `Xrgb8888`/`Argb8888` are little-endian byte order `[B, G, R, _]` → swap R/B.
/// - `Xbgr8888`/`Abgr8888` are `[R, G, B, _]` → already in order.
///
/// Stride padding is dropped row-by-row; the per-pixel swizzle is the shared SIMD
/// kernel in [`glass_core::pixels`]. Errors on any other format.
pub fn to_rgba(
    src: &[u8],
    format: Format,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>> {
    let order = match format {
        Format::Xrgb8888 | Format::Argb8888 => SourceOrder::Bgr,
        Format::Xbgr8888 | Format::Abgr8888 => SourceOrder::Rgb,
        other => {
            return Err(GlassError::CaptureFailed(format!(
                "unsupported screencopy format {other:?}"
            )))
        }
    };
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    if stride < w * 4 {
        return Err(GlassError::CaptureFailed(format!(
            "stride {stride} < {w}*4"
        )));
    }
    let needed = stride
        .checked_mul(h)
        .ok_or_else(|| GlassError::CaptureFailed("screencopy size overflow".into()))?;
    if src.len() < needed {
        return Err(GlassError::CaptureFailed(format!(
            "screencopy buffer {} bytes, expected >= {needed}",
            src.len()
        )));
    }
    let row = w * 4;
    let mut out = vec![0u8; w * h * 4];
    for y in 0..h {
        let s = &src[y * stride..y * stride + row];
        let d = &mut out[y * row..y * row + row];
        to_opaque_rgba(s, d, order);
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn converts_xrgb8888_respecting_stride() {
        // 2x2, stride 12 (8 used + 4 pad per row). Source pixels are [B,G,R,X].
        let src = vec![
            1, 2, 3, 255, 4, 5, 6, 255, 0, 0, 0, 0, // row 0 (+pad)
            7, 8, 9, 255, 10, 11, 12, 255, 0, 0, 0, 0, // row 1 (+pad)
        ];
        let out = to_rgba(&src, Format::Xrgb8888, 2, 2, 12).unwrap();
        assert_eq!(
            out,
            vec![3, 2, 1, 255, 6, 5, 4, 255, 9, 8, 7, 255, 12, 11, 10, 255]
        );
    }

    #[test]
    fn converts_xbgr8888_without_swap() {
        // Xbgr8888 bytes are already [R,G,B,X]; tight stride.
        let src = vec![1, 2, 3, 255, 4, 5, 6, 255];
        let out = to_rgba(&src, Format::Xbgr8888, 2, 1, 8).unwrap();
        assert_eq!(out, vec![1, 2, 3, 255, 4, 5, 6, 255]);
    }

    #[test]
    fn rejects_unsupported_format() {
        let err = to_rgba(&[0u8; 16], Format::Rgb565, 2, 2, 8).unwrap_err();
        assert!(matches!(err, GlassError::CaptureFailed(_)));
    }

    #[test]
    fn rejects_short_buffer() {
        let err = to_rgba(&[0u8; 8], Format::Argb8888, 2, 2, 8).unwrap_err();
        assert!(matches!(err, GlassError::CaptureFailed(_)));
    }
}
