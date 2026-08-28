#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ElementRoi {
    pub id: u32,
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

const MINIMUM_CHANGED_PIXELS: u64 = 8;
const PIXELS_PER_REQUIRED_CHANGE: u64 = 1_000;
const PIXEL_DELTA_TOLERANCE: u8 = 24;

pub fn unique_named_element(outline: &str, name: &str) -> Result<ElementRoi, String> {
    let needle = format!("\"{name}\"");
    let elements = outline
        .lines()
        .filter_map(|line| {
            let body = line.trim_start().strip_prefix('#')?;
            let (id, shape) = body.split_once(char::is_whitespace)?;
            shape.contains(&needle).then(|| {
                let bounds_start = shape.rfind('(')?;
                let bounds_end = shape[bounds_start..].find(')')? + bounds_start;
                let (origin, size) = shape[bounds_start + 1..bounds_end].split_once(' ')?;
                let (x, y) = origin.split_once(',')?;
                let (width, height) = size.split_once('x')?;
                Some(ElementRoi {
                    id: id.parse().ok()?,
                    x: x.parse().ok()?,
                    y: y.parse().ok()?,
                    width: width.parse().ok()?,
                    height: height.parse().ok()?,
                })
            })?
        })
        .collect::<Vec<_>>();
    match elements.as_slice() {
        [element] => Ok(*element),
        _ => Err(format!(
            "expected one on-screen element named {name:?}, found {}:\n{outline}",
            elements.len()
        )),
    }
}

pub fn assert_roi_changed(
    before_roi: &image::RgbaImage,
    after_frame: &image::RgbaImage,
    roi: ElementRoi,
    label: &str,
) -> Result<u64, String> {
    let area = u64::from(roi.width) * u64::from(roi.height);
    let minimum_changed_pixels = (area / PIXELS_PER_REQUIRED_CHANGE).max(MINIMUM_CHANGED_PIXELS);
    if before_roi.dimensions() != (roi.width, roi.height) {
        return Err(format!(
            "{label}: pre-action ROI is {}x{}, expected {}x{}",
            before_roi.width(),
            before_roi.height(),
            roi.width,
            roi.height
        ));
    }
    let right = roi
        .x
        .checked_add(roi.width)
        .ok_or_else(|| format!("{label}: ROI x extent overflowed"))?;
    let bottom = roi
        .y
        .checked_add(roi.height)
        .ok_or_else(|| format!("{label}: ROI y extent overflowed"))?;
    if right > after_frame.width() || bottom > after_frame.height() {
        return Err(format!(
            "{label}: ROI ({},{} {}x{}) does not fit terminal frame {}x{}",
            roi.x,
            roi.y,
            roi.width,
            roi.height,
            after_frame.width(),
            after_frame.height()
        ));
    }

    let changed = (0..roi.height)
        .flat_map(|y| (0..roi.width).map(move |x| (x, y)))
        .filter(|&(x, y)| {
            let before = before_roi.get_pixel(x, y);
            let after = after_frame.get_pixel(roi.x + x, roi.y + y);
            before
                .0
                .iter()
                .zip(after.0.iter())
                .any(|(&a, &b)| a.abs_diff(b) > PIXEL_DELTA_TOLERANCE)
        })
        .count() as u64;
    if changed < minimum_changed_pixels {
        return Err(format!(
            "{label}: terminal screenshot ROI is stale/pre-action: changed {changed} pixels, expected at least {minimum_changed_pixels} above tolerance {PIXEL_DELTA_TOLERANCE}"
        ));
    }
    Ok(changed)
}
