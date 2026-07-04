use crate::error::{GlassError, Result};
use crate::frame::Frame;
use image::codecs::webp::WebPEncoder;
use image::{ExtendedColorType, ImageEncoder, ImageFormat};

/// Encode a frame as lossless WebP bytes (used for MCP image content and
/// baselines).
///
/// glass uses WebP rather than PNG for two reasons: the encoded bytes are the
/// base64 payload sent to the agent's vision model on every screenshot, and
/// lossless WebP is both substantially smaller (cutting that per-look token
/// cost) and faster to encode than our previous Fast+Sub PNG on UI-like
/// content. The vision model decodes WebP natively. image-webp's encoder is
/// lossless-only, so frames round-trip exactly — required so diffs and
/// baselines stay bit-exact.
pub fn frame_to_webp(frame: &Frame) -> Result<Vec<u8>> {
    let expected = crate::frame::rgba_byte_len(frame.width, frame.height).ok_or_else(|| {
        GlassError::ImageCodec(format!(
            "dimensions {}x{} overflow the maximum buffer size",
            frame.width, frame.height
        ))
    })?;
    if frame.pixels.len() != expected {
        return Err(GlassError::ImageCodec(format!(
            "frame buffer is {} bytes, expected {} for {}x{}",
            frame.pixels.len(),
            expected,
            frame.width,
            frame.height
        )));
    }
    let mut out = Vec::new();
    WebPEncoder::new_lossless(&mut out)
        .write_image(
            &frame.pixels,
            frame.width,
            frame.height,
            ExtendedColorType::Rgba8,
        )
        .map_err(|e| GlassError::ImageCodec(e.to_string()))?;
    Ok(out)
}

/// Decode lossless WebP bytes back into a frame (used to load baselines).
pub fn frame_from_webp(bytes: &[u8]) -> Result<Frame> {
    let img = image::load_from_memory_with_format(bytes, ImageFormat::WebP)
        .map_err(|e| GlassError::ImageCodec(e.to_string()))?
        .to_rgba8();
    let (w, h) = img.dimensions();
    Frame::new(w, h, img.into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn webp_roundtrip_is_lossless() {
        // A few distinct colors so a lossy codec would visibly corrupt them.
        let mut px = Vec::new();
        for c in [
            [200, 100, 50, 255],
            [0, 0, 0, 255],
            [255, 255, 255, 255],
            [12, 240, 90, 255],
        ] {
            px.extend_from_slice(&c);
        }
        let frame = Frame::new(2, 2, px).unwrap();
        let webp = frame_to_webp(&frame).unwrap();
        let decoded = frame_from_webp(&webp).unwrap();
        assert_eq!(decoded, frame, "lossless WebP must round-trip exactly");
    }

    #[test]
    fn from_webp_rejects_garbage() {
        let err = frame_from_webp(b"not a webp").unwrap_err();
        assert!(matches!(err, GlassError::ImageCodec(_)));
    }

    #[test]
    fn to_webp_rejects_dimensions_that_overflow_usize() {
        // Frame has pub fields, so a frame whose dims overflow width*height*4
        // can be constructed directly (bypassing Frame::new's guard). Encoding
        // it must error, not panic on the overflowing multiply.
        let frame = Frame {
            width: u32::MAX,
            height: u32::MAX,
            pixels: vec![0; 16],
        };
        let err = frame_to_webp(&frame).unwrap_err();
        assert!(matches!(err, GlassError::ImageCodec(_)), "got {err:?}");
    }
}
