use crate::error::{GlassError, Result};

/// A captured frame: tightly-packed RGBA8 pixels, row-major, top-left origin.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct Frame {
    pub width: u32,
    pub height: u32,
    /// Length is always `width * height * 4`.
    pub pixels: Vec<u8>,
}

/// A window-relative sub-rectangle to crop a captured frame to. Top-left
/// origin; same field shape as [`crate::BBox`] so a diff result's bbox can be
/// fed back directly as a capture region.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Region {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

impl Region {
    /// Validate that this region is non-empty and fits within a `width`×`height`
    /// area. Returns [`GlassError::InvalidRegion`] otherwise. Used both to crop a
    /// captured frame and to validate a capture region against the window before
    /// hitting the backend.
    pub fn check_fits(&self, width: u32, height: u32) -> Result<()> {
        // Widen to u64 so a huge x/width can't wrap past the bound.
        let (x, y, w, h) = (self.x as u64, self.y as u64, self.width as u64, self.height as u64);
        if w == 0 || h == 0 || x + w > width as u64 || y + h > height as u64 {
            return Err(GlassError::InvalidRegion(format!(
                "region {}x{} at ({},{}) is empty or exceeds the {}x{} bounds",
                self.width, self.height, self.x, self.y, width, height
            )));
        }
        Ok(())
    }
}

/// The tightly-packed RGBA byte length for a `width`×`height` frame, or `None`
/// if it overflows `usize`. Dimensions can originate outside our control (a
/// WebP decoder's reported canvas size, a directly-constructed `Frame`), so the
/// multiply is checked rather than allowed to panic in debug / wrap in release.
pub(crate) fn rgba_byte_len(width: u32, height: u32) -> Option<usize> {
    (width as usize).checked_mul(height as usize)?.checked_mul(4)
}

impl Frame {
    /// Construct a frame, validating that the buffer matches the dimensions.
    pub fn new(width: u32, height: u32, pixels: Vec<u8>) -> Result<Self> {
        let expected = rgba_byte_len(width, height).ok_or_else(|| {
            GlassError::CaptureFailed(format!(
                "dimensions {width}x{height} overflow the maximum buffer size"
            ))
        })?;
        if pixels.len() != expected {
            return Err(GlassError::CaptureFailed(format!(
                "pixel buffer is {} bytes, expected {} for {}x{}",
                pixels.len(),
                expected,
                width,
                height
            )));
        }
        Ok(Self { width, height, pixels })
    }

    /// A solid-color frame, handy for tests and placeholders.
    pub fn solid(width: u32, height: u32, rgba: [u8; 4]) -> Self {
        let pixels = rgba.iter().copied().cycle().take(width as usize * height as usize * 4).collect();
        Self { width, height, pixels }
    }

    /// Number of pixels (not bytes).
    pub fn pixel_count(&self) -> u64 {
        self.width as u64 * self.height as u64
    }

    /// Crop to a window-relative sub-rectangle, returning a new tightly-packed
    /// frame. Errors with [`GlassError::InvalidRegion`] if the region is empty
    /// or extends past the frame — it never clamps.
    pub fn crop(&self, r: &Region) -> Result<Frame> {
        r.check_fits(self.width, self.height)?;
        let row_bytes = r.width as usize * 4;
        let src_stride = self.width as usize * 4;
        let mut out = Vec::with_capacity(row_bytes * r.height as usize);
        for oy in 0..r.height as usize {
            let start = (r.y as usize + oy) * src_stride + r.x as usize * 4;
            out.extend_from_slice(&self.pixels[start..start + row_bytes]);
        }
        Frame::new(r.width, r.height, out)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_rejects_wrong_buffer_length() {
        let err = Frame::new(2, 2, vec![0; 8]).unwrap_err();
        assert!(matches!(err, GlassError::CaptureFailed(_)));
    }

    #[test]
    fn new_accepts_correct_buffer_length() {
        let f = Frame::new(2, 2, vec![0; 16]).unwrap();
        assert_eq!(f.pixel_count(), 4);
    }

    #[test]
    fn new_rejects_dimensions_that_overflow_usize() {
        // width*height*4 overflows usize for these dims: in debug this panics,
        // in 32-bit release it wraps to a small `expected` that a short buffer
        // would wrongly satisfy. Must return a structured error instead.
        let err = Frame::new(u32::MAX, u32::MAX, vec![0; 16]).unwrap_err();
        assert!(matches!(err, GlassError::CaptureFailed(_)), "got {err:?}");
    }

    #[test]
    fn solid_fills_every_pixel() {
        let f = Frame::solid(3, 2, [10, 20, 30, 255]);
        assert_eq!(f.pixels.len(), 3 * 2 * 4);
        assert_eq!(&f.pixels[0..4], &[10, 20, 30, 255]);
        assert_eq!(&f.pixels[f.pixels.len() - 4..], &[10, 20, 30, 255]);
    }

    #[test]
    fn crop_extracts_exact_subrectangle() {
        // pixel (x,y) encodes as [x, y, 0, 255]
        let (w, h) = (4u32, 4u32);
        let mut px = Vec::new();
        for y in 0..h {
            for x in 0..w {
                px.extend_from_slice(&[x as u8, y as u8, 0, 255]);
            }
        }
        let frame = Frame::new(w, h, px).unwrap();
        let cropped = frame.crop(&Region { x: 1, y: 1, width: 2, height: 2 }).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 2));
        assert_eq!(&cropped.pixels[0..4], &[1, 1, 0, 255]); // (1,1)
        assert_eq!(&cropped.pixels[4..8], &[2, 1, 0, 255]); // (2,1)
        assert_eq!(&cropped.pixels[8..12], &[1, 2, 0, 255]); // (1,2)
        assert_eq!(&cropped.pixels[12..16], &[2, 2, 0, 255]); // (2,2)
    }

    #[test]
    fn crop_full_frame_is_identity() {
        let frame = Frame::solid(3, 2, [9, 8, 7, 255]);
        let cropped = frame.crop(&Region { x: 0, y: 0, width: 3, height: 2 }).unwrap();
        assert_eq!(cropped, frame);
    }

    #[test]
    fn crop_flush_to_edges_succeeds() {
        // bottom-right 2x2 corner: x+w == 4 and y+h == 4 are in-bounds.
        let frame = Frame::solid(4, 4, [1, 2, 3, 255]);
        let cropped = frame.crop(&Region { x: 2, y: 2, width: 2, height: 2 }).unwrap();
        assert_eq!((cropped.width, cropped.height), (2, 2));
    }

    #[test]
    fn region_check_fits_validates_bounds() {
        assert!(Region { x: 0, y: 0, width: 4, height: 4 }.check_fits(4, 4).is_ok());
        assert!(Region { x: 2, y: 2, width: 2, height: 2 }.check_fits(4, 4).is_ok());
        for bad in [
            Region { x: 0, y: 0, width: 0, height: 2 },
            Region { x: 0, y: 0, width: 2, height: 0 },
            Region { x: 3, y: 0, width: 2, height: 1 },
            Region { x: 0, y: 3, width: 1, height: 2 },
        ] {
            assert!(matches!(bad.check_fits(4, 4), Err(GlassError::InvalidRegion(_))), "{bad:?}");
        }
    }

    #[test]
    fn crop_rejects_empty_or_out_of_bounds() {
        let frame = Frame::solid(4, 4, [0, 0, 0, 255]);
        for bad in [
            Region { x: 0, y: 0, width: 0, height: 2 }, // zero width
            Region { x: 0, y: 0, width: 2, height: 0 }, // zero height
            Region { x: 3, y: 0, width: 2, height: 1 }, // x+w = 5 > 4
            Region { x: 0, y: 3, width: 1, height: 2 }, // y+h = 5 > 4
            Region { x: 5, y: 5, width: 1, height: 1 }, // origin past frame
        ] {
            assert!(
                matches!(frame.crop(&bad), Err(GlassError::InvalidRegion(_))),
                "{bad:?} should be rejected"
            );
        }
    }
}
