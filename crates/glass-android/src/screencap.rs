use glass_core::{Frame, GlassError, Result};

/// Decode raw `adb exec-out screencap` output (no `-p`) into an RGBA `Frame`.
///
/// Layout: little-endian `width:u32, height:u32, format:u32`, then on API ≥ 29 a
/// `colorspace:u32`, then tightly-packed pixels. Header size is inferred from the
/// total length; RGBA_8888 (format 1) is required.
pub fn decode_screencap(bytes: &[u8]) -> Result<Frame> {
    if bytes.len() < 12 {
        return Err(GlassError::CaptureFailed(format!(
            "screencap returned only {} bytes (no header)",
            bytes.len()
        )));
    }
    let rd = |o: usize| u32::from_le_bytes([bytes[o], bytes[o + 1], bytes[o + 2], bytes[o + 3]]);
    let (w, h, format) = (rd(0), rd(4), rd(8));

    let px = (w as usize)
        .checked_mul(h as usize)
        .and_then(|n| n.checked_mul(4))
        .ok_or_else(|| GlassError::CaptureFailed(format!("screencap dims {w}x{h} overflow")))?;

    let header = if bytes.len() == 12 + px {
        12
    } else if bytes.len() == 16 + px {
        16
    } else {
        return Err(GlassError::CaptureFailed(format!(
            "screencap {w}x{h}: {} payload bytes don't match a 12/16-byte header + {px} pixel bytes \
             (FLAG_SECURE window or unexpected format {format}?)",
            bytes.len()
        )));
    };

    // Android PixelFormat::RGBA_8888 == 1.
    if format != 1 {
        return Err(GlassError::CaptureFailed(format!(
            "screencap pixel format {format} is not RGBA_8888 (1)"
        )));
    }

    Frame::new(w, h, bytes[header..header + px].to_vec())
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::GlassError;

    /// Build a synthetic screencap buffer: header (LE u32 fields) + RGBA pixels.
    fn buf(w: u32, h: u32, format: u32, header_extra: bool, pixels: &[u8]) -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&w.to_le_bytes());
        v.extend_from_slice(&h.to_le_bytes());
        v.extend_from_slice(&format.to_le_bytes());
        if header_extra {
            v.extend_from_slice(&0u32.to_le_bytes()); // colorspace (API >= 29)
        }
        v.extend_from_slice(pixels);
        v
    }

    #[test]
    fn decodes_12_byte_header() {
        let px = vec![1u8; 2 * 2 * 4];
        let f = decode_screencap(&buf(2, 2, 1, false, &px)).unwrap();
        assert_eq!((f.width, f.height), (2, 2));
        assert_eq!(f.pixels, px);
    }

    #[test]
    fn decodes_16_byte_header() {
        let px = vec![7u8; 12];
        let f = decode_screencap(&buf(1, 3, 1, true, &px)).unwrap();
        assert_eq!((f.width, f.height), (1, 3));
        assert_eq!(f.pixels, px);
    }

    #[test]
    fn rejects_non_rgba8888() {
        let px = vec![0u8; 2 * 2 * 4];
        let err = decode_screencap(&buf(2, 2, 4, false, &px)).unwrap_err();
        assert!(matches!(err, GlassError::CaptureFailed(_)));
    }

    #[test]
    fn rejects_length_mismatch_as_capture_failed() {
        // Claims 4x4 but carries too few pixel bytes (e.g. FLAG_SECURE blank).
        let err = decode_screencap(&buf(4, 4, 1, false, &[0u8; 8])).unwrap_err();
        assert!(matches!(err, GlassError::CaptureFailed(_)));
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(matches!(decode_screencap(&[0u8; 4]), Err(GlassError::CaptureFailed(_))));
    }
}
