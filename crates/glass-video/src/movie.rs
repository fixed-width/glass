use bytes::Bytes;
use glass_core::{Frame, GlassError, Result};
use image::{ImageBuffer, Rgba, RgbaImage, imageops};
use mp4::{AvcConfig, Mp4Config, Mp4Sample, Mp4Writer, TrackConfig};
use openh264::encoder::{Encoder, EncoderConfig, FrameRate, QpRange, RateControlMode, UsageType};
use openh264::formats::{RgbaSliceU8, YUVBuffer};
use openh264::{OpenH264API, Timestamp};
use std::io::Cursor;

pub(crate) const MUX_RESERVE: u64 = 256 * 1024;

#[derive(Clone)]
pub(crate) struct Sample {
    pub timestamp: u64,
    pub bytes: Bytes,
    pub sync: bool,
}

pub(crate) struct FrameEncoder {
    encoder: Encoder,
    width: u32,
    height: u32,
    sps: Option<Vec<u8>>,
    pps: Option<Vec<u8>>,
}

pub(crate) struct Movie {
    encoder: FrameEncoder,
    samples: Vec<Sample>,
    bytes: u64,
    max_bytes: u64,
}

fn error(error: impl std::fmt::Display) -> GlassError {
    GlassError::Backend(format!("desktop video encoding: {error}"))
}

impl FrameEncoder {
    pub(crate) fn new(first: &Frame) -> Result<Self> {
        validate_frame(first)?;
        let (width, height) = canvas_size(first.width, first.height);
        let config = EncoderConfig::new()
            .max_frame_rate(FrameRate::from_hz(15.0))
            .usage_type(UsageType::ScreenContentRealTime)
            .rate_control_mode(RateControlMode::Off)
            .qp(QpRange::new(24, 32))
            .adaptive_quantization(false)
            .background_detection(false)
            .num_threads(2)
            .skip_frames(false);
        let encoder =
            Encoder::with_api_config(OpenH264API::from_source(), config).map_err(error)?;
        Ok(Self {
            encoder,
            width,
            height,
            sps: None,
            pps: None,
        })
    }

    pub(crate) fn force_keyframe(&mut self) {
        self.encoder.force_intra_frame();
    }

    pub(crate) fn encode(&mut self, frame: Frame, timestamp: u64) -> Result<Sample> {
        let image = fit(frame, self.width, self.height)?;
        let rgb = RgbaSliceU8::new(image.as_raw(), (self.width as usize, self.height as usize));
        let yuv = YUVBuffer::from_rgba8_source(rgb);
        let stream = self
            .encoder
            .encode_at(&yuv, Timestamp::from_millis(timestamp))
            .map_err(error)?
            .to_vec();
        let mut bytes = Vec::new();
        let mut sync = false;
        let mut picture = false;
        for nal in openh264::nal_units(&stream) {
            let nal = nal
                .strip_prefix(&[0, 0, 0, 1])
                .or_else(|| nal.strip_prefix(&[0, 0, 1]))
                .ok_or_else(|| error("invalid Annex B NAL prefix"))?;
            let kind = nal.first().ok_or_else(|| error("empty NAL"))? & 31;
            match kind {
                7 => self.sps = Some(nal.to_vec()),
                8 => self.pps = Some(nal.to_vec()),
                _ => {
                    picture |= matches!(kind, 1 | 5);
                    sync |= kind == 5;
                    bytes.extend_from_slice(&(nal.len() as u32).to_be_bytes());
                    bytes.extend_from_slice(nal);
                }
            }
        }
        if !picture {
            return Err(error("encoder produced no picture"));
        }
        Ok(Sample {
            timestamp,
            bytes: Bytes::from(bytes),
            sync,
        })
    }

    pub(crate) fn mux(&self, samples: &[Sample], end_ms: u64, max_bytes: u64) -> Result<Vec<u8>> {
        let first = samples
            .first()
            .ok_or_else(|| error("no decodable history"))?;
        if !first.sync {
            return Err(error("history does not begin with a keyframe"));
        }
        let config = Mp4Config {
            major_brand: (*b"isom").into(),
            minor_version: 512,
            compatible_brands: vec![(*b"isom").into(), (*b"avc1").into()],
            timescale: 1000,
        };
        let mut writer = Mp4Writer::write_start(Cursor::new(Vec::new()), &config).map_err(error)?;
        let mut track: TrackConfig = AvcConfig {
            width: self.width as u16,
            height: self.height as u16,
            seq_param_set: self.sps.clone().ok_or_else(|| error("missing SPS"))?,
            pic_param_set: self.pps.clone().ok_or_else(|| error("missing PPS"))?,
        }
        .into();
        track.timescale = 1000;
        writer.add_track(&track).map_err(error)?;
        for (index, sample) in samples.iter().enumerate() {
            let end = samples.get(index + 1).map_or(end_ms, |s| s.timestamp);
            writer
                .write_sample(
                    1,
                    &Mp4Sample {
                        start_time: sample.timestamp.saturating_sub(first.timestamp),
                        duration: end
                            .saturating_sub(sample.timestamp)
                            .max(1)
                            .min(u64::from(u32::MAX)) as u32,
                        rendering_offset: 0,
                        is_sync: sample.sync,
                        bytes: sample.bytes.clone(),
                    },
                )
                .map_err(error)?;
        }
        writer.write_end().map_err(error)?;
        let bytes = writer.into_writer().into_inner();
        if bytes.len() as u64 > max_bytes {
            return Err(error("finalized movie exceeds the clip allowance"));
        }
        glass_core::recording::validate_video(&bytes)?;
        Ok(bytes)
    }
}

impl Movie {
    pub(crate) fn new(first: &Frame, max_bytes: u64) -> Result<Self> {
        Ok(Self {
            encoder: FrameEncoder::new(first)?,
            samples: Vec::new(),
            bytes: 0,
            max_bytes,
        })
    }
    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
    pub(crate) fn push(&mut self, frame: Frame, timestamp: u64) -> Result<bool> {
        let timestamp = self
            .samples
            .last()
            .map_or(0, |last| timestamp.max(last.timestamp + 1));
        let sample = self.encoder.encode(frame, timestamp)?;
        if self.bytes + sample.bytes.len() as u64 + MUX_RESERVE > self.max_bytes
            || self.samples.len() >= 1024
        {
            if self.samples.is_empty() {
                return Err(error("first frame exceeds the clip allowance"));
            }
            return Ok(false);
        }
        self.bytes += sample.bytes.len() as u64;
        self.samples.push(sample);
        Ok(true)
    }
    pub(crate) fn finish(self, end_ms: u64) -> Result<Vec<u8>> {
        self.encoder.mux(&self.samples, end_ms, self.max_bytes)
    }
}

fn canvas_size(width: u32, height: u32) -> (u32, u32) {
    let scale = (1280.0 / f64::from(width.max(height))).min(1.0);
    let even = |value: u32| ((f64::from(value) * scale) as u32 / 2 * 2).max(2);
    (even(width), even(height))
}

fn validate_frame(frame: &Frame) -> Result<()> {
    let size = u64::from(frame.width)
        .checked_mul(u64::from(frame.height))
        .and_then(|pixels| pixels.checked_mul(4))
        .ok_or_else(|| error("capture dimensions overflow"))?;
    if frame.width == 0
        || frame.height == 0
        || size > 256 * 1024 * 1024
        || size != frame.pixels.len() as u64
    {
        return Err(error("invalid capture dimensions or RGBA buffer"));
    }
    Ok(())
}

fn fit(frame: Frame, width: u32, height: u32) -> Result<RgbaImage> {
    validate_frame(&frame)?;
    let source = ImageBuffer::from_raw(frame.width, frame.height, frame.pixels)
        .ok_or_else(|| error("invalid RGBA buffer"))?;
    if source.dimensions() == (width, height) {
        return Ok(source);
    }
    let scale = (f64::from(width) / f64::from(source.width()))
        .min(f64::from(height) / f64::from(source.height()));
    let w = ((f64::from(source.width()) * scale) as u32)
        .max(1)
        .min(width);
    let h = ((f64::from(source.height()) * scale) as u32)
        .max(1)
        .min(height);
    let resized = imageops::resize(&source, w, h, imageops::FilterType::Triangle);
    let mut canvas = ImageBuffer::from_pixel(width, height, Rgba([0, 0, 0, 255]));
    imageops::replace(
        &mut canvas,
        &resized,
        i64::from((width - w) / 2),
        i64::from((height - h) / 2),
    );
    Ok(canvas)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn solid(width: u32, height: u32, color: [u8; 4]) -> Frame {
        Frame::new(width, height, color.repeat((width * height) as usize)).unwrap()
    }

    #[test]
    fn mux_preserves_acquisition_intervals_final_hold_and_resize_canvas() {
        let first = solid(64, 48, [255, 0, 0, 255]);
        let mut movie = Movie::new(&first, 8 * 1024 * 1024).unwrap();
        assert!(movie.push(first, 0).unwrap());
        assert!(movie.push(solid(32, 64, [0, 255, 0, 255]), 80).unwrap());
        assert!(movie.push(solid(128, 96, [0, 0, 255, 255]), 400).unwrap());
        let mut headers = vec![0, 0, 0, 1];
        headers.extend(movie.encoder.sps.as_ref().unwrap());
        headers.extend([0, 0, 0, 1]);
        headers.extend(movie.encoder.pps.as_ref().unwrap());
        let bytes = movie.finish(900).unwrap();
        let mut reader =
            mp4::Mp4Reader::read_header(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        assert_eq!(
            (reader.tracks()[&1].width(), reader.tracks()[&1].height()),
            (64, 48)
        );
        let samples: Vec<_> = (1..=3)
            .map(|id| reader.read_sample(1, id).unwrap().unwrap())
            .collect();
        assert_eq!(
            samples
                .iter()
                .map(|s| (s.start_time, s.duration))
                .collect::<Vec<_>>(),
            [(0, 80), (80, 320), (400, 500)]
        );
        assert!(samples[0].is_sync);
        assert!(
            samples
                .windows(2)
                .all(|pair| pair[0].bytes != pair[1].bytes)
        );
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        decoder.decode(&headers).unwrap();
        for sample in &samples {
            let mut stream = Vec::new();
            let mut rest = sample.bytes.as_ref();
            while !rest.is_empty() {
                let size = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
                stream.extend([0, 0, 0, 1]);
                stream.extend(&rest[4..4 + size]);
                rest = &rest[4 + size..];
            }
            let decoded = decoder.decode(&stream).unwrap().unwrap();
            use openh264::formats::YUVSource;
            assert_eq!(decoded.dimensions(), (64, 48));
        }
    }

    #[test]
    fn resized_content_fits_without_cropping_and_padding_is_opaque() {
        let canvas = fit(solid(2, 8, [4, 9, 27, 255]), 8, 8).unwrap();
        assert_eq!(canvas.get_pixel(0, 0).0, [0, 0, 0, 255]);
        assert_eq!(canvas.get_pixel(3, 0).0, [4, 9, 27, 255]);
        assert_eq!(canvas.get_pixel(4, 7).0, [4, 9, 27, 255]);
        assert_eq!(canvas.get_pixel(7, 7).0, [0, 0, 0, 255]);
        assert_eq!(canvas_size(3840, 2160), (1280, 720));
    }

    #[test]
    fn a_full_allowance_retains_a_decodable_prefix_and_rejects_further_samples() {
        let first = solid(64, 48, [255, 0, 0, 255]);
        let mut movie = Movie::new(&first, 8 * 1024 * 1024).unwrap();
        movie.push(first, 0).unwrap();
        movie.max_bytes = movie.bytes + MUX_RESERVE;
        assert!(!movie.push(solid(64, 48, [0, 0, 255, 255]), 100).unwrap());
        let bytes = movie.finish(200).unwrap();
        let reader = mp4::Mp4Reader::read_header(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        assert_eq!(reader.sample_count(1).unwrap(), 1);
    }

    #[test]
    fn untrusted_frame_dimensions_are_rejected_without_overflow() {
        let frame = Frame {
            width: u32::MAX,
            height: u32::MAX,
            pixels: vec![],
            capture: None,
        };
        assert!(validate_frame(&frame).is_err());
    }

    #[test]
    fn evicted_history_rebases_and_decodes_from_its_retained_keyframe() {
        use crate::history::History;
        use glass_core::RollingOptions;
        let mut encoder = FrameEncoder::new(&solid(64, 48, [255, 0, 0, 255])).unwrap();
        let mut history = History::new(RollingOptions {
            id: "a".repeat(32),
            history_ms: 1000,
            max_bytes: 8 * 1024 * 1024,
        });
        for ms in (0..=3000).step_by(100) {
            if history.keyframe_due(ms) {
                encoder.force_keyframe();
            }
            let sample = encoder
                .encode(
                    solid(64, 48, [((ms / 100) % 16) as u8 * 13, 90, 110, 255]),
                    ms,
                )
                .unwrap();
            history.push(sample).unwrap();
        }
        history.mark(&"b".repeat(32), 3000, 0).unwrap();
        let (window, samples) = history.take(3001, false).unwrap();
        assert!(window.start_ms > 0 && window.start_ms <= 2000);
        let bytes = encoder
            .mux(&samples, window.end_ms, 8 * 1024 * 1024)
            .unwrap();
        let mut movie =
            mp4::Mp4Reader::read_header(Cursor::new(&bytes), bytes.len() as u64).unwrap();
        let mut headers = vec![0, 0, 0, 1];
        headers.extend(encoder.sps.as_ref().unwrap());
        headers.extend([0, 0, 0, 1]);
        headers.extend(encoder.pps.as_ref().unwrap());
        let mut decoder = openh264::decoder::Decoder::new().unwrap();
        decoder.decode(&headers).unwrap();
        for id in 1..=movie.sample_count(1).unwrap() {
            let sample = movie.read_sample(1, id).unwrap().unwrap();
            if id == 1 {
                assert_eq!(sample.start_time, 0);
                assert!(sample.is_sync);
            }
            let mut rest = sample.bytes.as_ref();
            let mut stream = Vec::new();
            while !rest.is_empty() {
                let size = u32::from_be_bytes(rest[..4].try_into().unwrap()) as usize;
                stream.extend([0, 0, 0, 1]);
                stream.extend(&rest[4..4 + size]);
                rest = &rest[4 + size..];
            }
            assert!(decoder.decode(&stream).unwrap().is_some());
        }
    }
}
