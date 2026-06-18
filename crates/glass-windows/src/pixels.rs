//! Pure pixel-format helpers (no OS calls), unit-tested on the Linux dev box.

use glass_core::pixels::{to_opaque_rgba_in_place, SourceOrder};

/// Convert a BGRA8 pixel buffer (WGC's native layout) to RGBA8 in place, forcing
/// every alpha to 255 (WGC alpha is unreliable for opaque windows). Delegates the
/// per-pixel swizzle to the shared SIMD kernel in [`glass_core::pixels`].
pub fn bgra_to_rgba(buf: &mut [u8]) {
    to_opaque_rgba_in_place(buf, SourceOrder::Bgr);
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn swizzles_bgra_to_rgba_opaque() {
        let mut buf = vec![10u8, 20, 30, 0, 40, 50, 60, 128]; // 2 px, BGRA
        bgra_to_rgba(&mut buf);
        assert_eq!(buf, vec![30, 20, 10, 255, 60, 50, 40, 255]); // RGBA, alpha forced
    }
    #[test]
    fn ignores_trailing_partial_pixel() {
        // chunks_exact_mut leaves a <4 remainder untouched (defensive: shouldn't happen for w*h*4)
        let mut buf = vec![1u8, 2, 3, 4, 9, 9]; // one full px + 2 stray bytes
        bgra_to_rgba(&mut buf);
        assert_eq!(buf, vec![3, 2, 1, 255, 9, 9]);
    }
}
