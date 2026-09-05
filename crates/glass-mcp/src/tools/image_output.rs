//! Size requested observations after native capture and comparison.

use glass_core::{Frame, GlassError, Result, frame_to_webp};
use image::RgbaImage;
use serde_json::{Value, json};
use std::num::NonZeroU32;

pub(super) struct ImageOutput {
    pub width: u32,
    pub height: u32,
    bytes: Vec<u8>,
    metadata: Option<Value>,
}

impl ImageOutput {
    pub fn attach_to(self, result: &mut Value) -> Vec<u8> {
        if let Some(metadata) = self.metadata {
            result["image"] = metadata;
        }
        self.bytes
    }
}

fn fit_dimensions(
    width: u32,
    height: u32,
    max_width: Option<NonZeroU32>,
    max_height: Option<NonZeroU32>,
) -> Result<(u32, u32)> {
    if width == 0 || height == 0 {
        return Err(GlassError::ImageCodec(
            "cannot encode an empty frame".into(),
        ));
    }
    let bound_width = max_width.map_or(width, |n| n.get().min(width));
    let bound_height = max_height.map_or(height, |n| n.get().min(height));
    if u64::from(bound_width) * u64::from(height) <= u64::from(bound_height) * u64::from(width) {
        Ok((
            bound_width,
            (u64::from(height) * u64::from(bound_width) / u64::from(width)).max(1) as u32,
        ))
    } else {
        Ok((
            (u64::from(width) * u64::from(bound_height) / u64::from(height)).max(1) as u32,
            bound_height,
        ))
    }
}

pub(super) fn encode_image(
    frame: Frame,
    origin: (u32, u32),
    max_width: Option<NonZeroU32>,
    max_height: Option<NonZeroU32>,
) -> Result<ImageOutput> {
    let (width, height) = fit_dimensions(frame.width, frame.height, max_width, max_height)?;
    let resized = (width, height) != (frame.width, frame.height);
    let metadata = (max_width.is_some() || max_height.is_some()).then(|| {
        json!({
            "source": {"x":origin.0,"y":origin.1,"width":frame.width,"height":frame.height},
            "width":width,"height":height,
            "scale_x":f64::from(width)/f64::from(frame.width),
            "scale_y":f64::from(height)/f64::from(frame.height),
            "resized":resized,"pixel_exact":!resized,"encoding":"lossless_webp"
        })
    });
    let bytes = if resized {
        let expected = (frame.width as usize)
            .checked_mul(frame.height as usize)
            .and_then(|n| n.checked_mul(4));
        if expected != Some(frame.pixels.len()) {
            return Err(GlassError::ImageCodec("invalid frame buffer length".into()));
        }
        let source = RgbaImage::from_raw(frame.width, frame.height, frame.pixels)
            .ok_or_else(|| GlassError::ImageCodec("invalid frame dimensions".into()))?;
        let image = RgbaImage::from_fn(width, height, |x, y| {
            *source.get_pixel(
                source_pixel(x, source.width(), width),
                source_pixel(y, source.height(), height),
            )
        });
        frame_to_webp(&Frame::new(width, height, image.into_raw())?)?
    } else {
        frame_to_webp(&frame)?
    };
    Ok(ImageOutput {
        width,
        height,
        bytes,
        metadata,
    })
}

fn source_pixel(position: u32, source: u32, returned: u32) -> u32 {
    // Integer pixel centers cannot round into the preceding pixel at an exact boundary.
    ((u64::from(position) * u64::from(source) + u64::from(source) / 2) / u64::from(returned)) as u32
}

#[cfg(test)]
mod tests;
