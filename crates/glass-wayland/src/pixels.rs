use glass_core::pixels::{to_opaque_rgba, SourceOrder};
use glass_core::{GlassError, Result};
use wayland_client::protocol::wl_shm::Format;

/// Convert a `wl_shm` screencopy buffer to tightly-packed opaque RGBA, honoring row
/// `stride` padding. Handles the 32-bpp layouts GPU compositors emit and the 24-bpp packed
/// layouts software renderers negotiate (headless sway advertises only `Bgr888`):
/// - `Xrgb8888`/`Argb8888` — little-endian byte order `[B, G, R, _]` → swap R/B.
/// - `Xbgr8888`/`Abgr8888` — `[R, G, B, _]` → already in order.
/// - `Bgr888` — 3 bytes/pixel, `[R, G, B]` → already in order.
/// - `Rgb888` — 3 bytes/pixel, `[B, G, R]` → swap R/B.
///
/// The `wl_shm`/DRM format *name* is the big-endian channel order; the bytes in memory are
/// its little-endian reverse (so `Xrgb8888` is `[B, G, R, _]` and `Bgr888` is `[R, G, B]`).
/// The 32-bpp swizzle uses the shared SIMD kernel in [`glass_core::pixels`]; the 24-bpp path
/// expands 3→4 bytes scalar. Errors on any other format.
pub fn to_rgba(
    src: &[u8],
    format: Format,
    width: u32,
    height: u32,
    stride: u32,
) -> Result<Vec<u8>> {
    let (w, h, stride) = (width as usize, height as usize, stride as usize);
    let mut out = vec![0u8; w * h * 4];
    match format {
        Format::Xrgb8888 | Format::Argb8888 => {
            unpack32(src, &mut out, w, h, stride, SourceOrder::Bgr)?
        }
        Format::Xbgr8888 | Format::Abgr8888 => {
            unpack32(src, &mut out, w, h, stride, SourceOrder::Rgb)?
        }
        Format::Bgr888 => unpack24(src, &mut out, w, h, stride, SourceOrder::Rgb)?,
        Format::Rgb888 => unpack24(src, &mut out, w, h, stride, SourceOrder::Bgr)?,
        other => {
            return Err(GlassError::CaptureFailed(format!(
                "unsupported screencopy format {other:?}"
            )))
        }
    }
    Ok(out)
}

/// Validate a source buffer's `stride`/size for `bytes_per_pixel`, returning the packed
/// (unpadded) row width in bytes.
fn check_src(
    src: &[u8],
    w: usize,
    h: usize,
    stride: usize,
    bytes_per_pixel: usize,
) -> Result<usize> {
    let row = w * bytes_per_pixel;
    if stride < row {
        return Err(GlassError::CaptureFailed(format!(
            "stride {stride} < {w}*{bytes_per_pixel}"
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
    Ok(row)
}

/// 32-bpp source: swap R/B per `order`, force alpha opaque, via the shared SIMD kernel.
fn unpack32(
    src: &[u8],
    out: &mut [u8],
    w: usize,
    h: usize,
    stride: usize,
    order: SourceOrder,
) -> Result<()> {
    let row = check_src(src, w, h, stride, 4)?;
    for y in 0..h {
        let s = &src[y * stride..y * stride + row];
        let d = &mut out[y * row..y * row + row];
        to_opaque_rgba(s, d, order);
    }
    Ok(())
}

/// 24-bpp packed source: expand 3 bytes/pixel to opaque RGBA, swapping R/B per `order`.
fn unpack24(
    src: &[u8],
    out: &mut [u8],
    w: usize,
    h: usize,
    stride: usize,
    order: SourceOrder,
) -> Result<()> {
    let row = check_src(src, w, h, stride, 3)?;
    let dst_row = w * 4;
    for y in 0..h {
        let s = &src[y * stride..y * stride + row];
        let d = &mut out[y * dst_row..y * dst_row + dst_row];
        for (px, dst) in s.chunks_exact(3).zip(d.chunks_exact_mut(4)) {
            let (r, b) = match order {
                SourceOrder::Bgr => (px[2], px[0]),
                SourceOrder::Rgb => (px[0], px[2]),
            };
            dst[0] = r;
            dst[1] = px[1];
            dst[2] = b;
            dst[3] = 255;
        }
    }
    Ok(())
}

/// The 32-bit `wl_shm` formats [`to_rgba`] converts via the shared SIMD kernel.
fn is_preferred_format(f: Format) -> bool {
    matches!(
        f,
        Format::Xrgb8888 | Format::Argb8888 | Format::Xbgr8888 | Format::Abgr8888
    )
}

/// Choose which advertised screencopy buffer to request. wlr-screencopy v3 advertises one
/// entry per supported shm format `(format, width, height, stride)`; prefer a 32-bit one
/// [`to_rgba`] handles, so capture reuses the well-tested SIMD path instead of whatever a
/// software renderer happens to list first (e.g. a packed 24-bit format). Falls back to the
/// first advertised entry when no preferred format is offered; `None` only when the list is
/// empty.
pub fn pick_shm_format(advertised: &[(Format, u32, u32, u32)]) -> Option<(Format, u32, u32, u32)> {
    advertised
        .iter()
        .copied()
        .find(|(f, ..)| is_preferred_format(*f))
        .or_else(|| advertised.first().copied())
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

    #[test]
    fn converts_bgr888_24bpp_respecting_stride() {
        // Bgr888 memory bytes are [R,G,B] (already RGBA order), 3 bytes/pixel.
        // 2x2, stride 8 (6 used + 2 pad per row).
        let src = vec![
            10, 20, 30, 40, 50, 60, 0, 0, // row 0 (+pad)
            70, 80, 90, 100, 110, 120, 0, 0, // row 1 (+pad)
        ];
        let out = to_rgba(&src, Format::Bgr888, 2, 2, 8).unwrap();
        assert_eq!(
            out,
            vec![10, 20, 30, 255, 40, 50, 60, 255, 70, 80, 90, 255, 100, 110, 120, 255]
        );
    }

    #[test]
    fn converts_rgb888_24bpp_swapping_red_blue() {
        // Rgb888 memory bytes are [B,G,R]; swap to RGBA. 2x1, tight stride 6.
        let src = vec![10, 20, 30, 40, 50, 60];
        let out = to_rgba(&src, Format::Rgb888, 2, 1, 6).unwrap();
        assert_eq!(out, vec![30, 20, 10, 255, 60, 50, 40, 255]);
    }

    #[test]
    fn prefers_a_32bit_format_over_others() {
        // A software compositor may advertise a 24-bit format (which to_rgba can't convert)
        // alongside a 32-bit one; pick the 32-bit one so capture reuses the SIMD path.
        let advertised = [
            (Format::Bgr888, 100, 50, 300),
            (Format::Xrgb8888, 100, 50, 400),
        ];
        assert_eq!(
            pick_shm_format(&advertised),
            Some((Format::Xrgb8888, 100, 50, 400))
        );
    }

    #[test]
    fn falls_back_to_first_when_no_32bit_advertised() {
        let advertised = [(Format::Bgr888, 100, 50, 300)];
        assert_eq!(
            pick_shm_format(&advertised),
            Some((Format::Bgr888, 100, 50, 300))
        );
    }

    #[test]
    fn none_when_nothing_advertised() {
        assert_eq!(pick_shm_format(&[]), None);
    }
}
