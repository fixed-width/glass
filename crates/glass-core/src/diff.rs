use crate::error::{GlassError, Result};
use crate::frame::{Frame, Region};
use std::simd::cmp::{SimdOrd, SimdPartialOrd};
use std::simd::{u8x32, Simd};

/// Axis-aligned bounding box of changed pixels.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct BBox {
    pub x: u32,
    pub y: u32,
    pub width: u32,
    pub height: u32,
}

/// Result of comparing two frames.
#[derive(Clone, Debug, PartialEq)]
pub struct DiffResult {
    pub changed_pixels: u64,
    pub total_pixels: u64,
    pub changed_pct: f32,
    /// `None` when nothing changed.
    pub bbox: Option<BBox>,
    /// Pixels that differed but were suppressed as anti-aliasing by the perceptual
    /// diff (always 0 for the exact diff). Surfaces how much was filtered.
    pub aa_ignored: u64,
    /// Pixels excluded from the comparison by an [`IgnoreMask`], counting
    /// overlapping rects once. `changed_pct` is measured over the remaining
    /// (considered) pixels.
    pub ignored_pixels: u64,
}

/// Rectangles excluded from a comparison, precomputed into merged per-row
/// column spans. Built once per diff; the rect list is small in practice, so
/// per-row spans are cheaper than a per-pixel bitmap over the whole frame.
///
/// Spans are half-open `[start, end)`, sorted, and non-overlapping — merging is
/// what makes overlapping rects count once in [`ignored_count`].
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct IgnoreMask {
    rows: Vec<Vec<(u32, u32)>>,
    ignored: u64,
}

impl IgnoreMask {
    /// A mask that excludes nothing.
    pub fn empty() -> Self {
        Self::default()
    }

    /// Build a mask over a `width`×`height` area. Rects are clamped to that area;
    /// a rect entirely outside contributes nothing (its mistake shows up as a zero
    /// `ignored_count`, not an error). A zero-area rect is a caller bug and errors.
    pub fn new(rects: &[Region], width: u32, height: u32) -> Result<Self> {
        for r in rects {
            if r.width == 0 || r.height == 0 {
                return Err(GlassError::InvalidRegion(format!(
                    "ignore rect has zero area: {}x{} at ({},{})",
                    r.width, r.height, r.x, r.y
                )));
            }
        }
        if rects.is_empty() || width == 0 || height == 0 {
            return Ok(Self::default());
        }

        let mut rows: Vec<Vec<(u32, u32)>> = vec![Vec::new(); height as usize];
        for r in rects {
            let x0 = r.x.min(width);
            let y0 = r.y.min(height);
            let x1 = r.x.saturating_add(r.width).min(width);
            let y1 = r.y.saturating_add(r.height).min(height);
            if x0 >= x1 || y0 >= y1 {
                continue; // fully outside the frame
            }
            for row in rows.iter_mut().take(y1 as usize).skip(y0 as usize) {
                row.push((x0, x1));
            }
        }

        let mut ignored = 0u64;
        for spans in &mut rows {
            spans.sort_unstable();
            let mut merged: Vec<(u32, u32)> = Vec::with_capacity(spans.len());
            for &(s, e) in spans.iter() {
                match merged.last_mut() {
                    // `s <= last.1` merges touching spans too, not just overlapping.
                    Some(last) if s <= last.1 => last.1 = last.1.max(e),
                    _ => merged.push((s, e)),
                }
            }
            ignored += merged.iter().map(|&(s, e)| u64::from(e - s)).sum::<u64>();
            *spans = merged;
        }

        Ok(Self { rows, ignored })
    }

    /// Build a mask for a comparison scoped to `region`: each rect is intersected
    /// with the region and translated into region-local coordinates, so callers
    /// always pass window-relative rects regardless of scoping.
    pub fn for_region(
        rects: &[Region],
        region: Option<&Region>,
        width: u32,
        height: u32,
    ) -> Result<Self> {
        let Some(region) = region else {
            return Self::new(rects, width, height);
        };
        for r in rects {
            if r.width == 0 || r.height == 0 {
                return Err(GlassError::InvalidRegion(format!(
                    "ignore rect has zero area: {}x{} at ({},{})",
                    r.width, r.height, r.x, r.y
                )));
            }
        }
        let local: Vec<Region> = rects
            .iter()
            .filter_map(|r| r.intersect(region))
            .map(|i| Region {
                x: i.x - region.x,
                y: i.y - region.y,
                width: i.width,
                height: i.height,
            })
            .collect();
        Self::new(&local, region.width, region.height)
    }

    /// True when nothing is excluded — lets callers take the unmasked fast path.
    #[inline]
    pub fn is_empty(&self) -> bool {
        self.ignored == 0
    }

    /// Total excluded pixels, counting overlaps once.
    #[inline]
    pub fn ignored_count(&self) -> u64 {
        self.ignored
    }

    /// Merged, sorted excluded column spans for row `y`.
    #[inline]
    pub fn spans_for_row(&self, y: u32) -> &[(u32, u32)] {
        self.rows.get(y as usize).map_or(&[], Vec::as_slice)
    }

    /// True when column `x` of row `y` is excluded.
    #[inline]
    pub fn is_ignored(&self, x: u32, y: u32) -> bool {
        self.spans_for_row(y).iter().any(|&(s, e)| x >= s && x < e)
    }

    /// True when the half-open column run `[x0, x1)` of row `y` is *entirely*
    /// excluded — the whole-SIMD-chunk skip test.
    #[inline]
    pub fn covers_span(&self, y: u32, x0: u32, x1: u32) -> bool {
        self.spans_for_row(y)
            .iter()
            .any(|&(s, e)| s <= x0 && x1 <= e)
    }
}

/// Direction of a region wait: diverge from a reference, or converge to it.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum RegionUntil {
    /// Satisfied once the region differs from the reference.
    Changes,
    /// Satisfied once the region is identical to the reference (within the
    /// diff's per-pixel sensitivity).
    Matches,
}

/// Whether a region wait is satisfied by this diff. `changed_pixels` is measured
/// with the chosen mode's per-pixel sensitivity (`threshold`/`tolerance`), so
/// that sensitivity is the noise knob; this only checks "any change vs none".
pub fn region_satisfied(d: &DiffResult, until: RegionUntil) -> bool {
    match until {
        RegionUntil::Changes => d.changed_pixels > 0,
        RegionUntil::Matches => d.changed_pixels == 0,
    }
}

const LANES: usize = 32; // 8 RGBA pixels per SIMD chunk

/// True if this pixel's max per-channel absolute difference exceeds `tolerance`.
#[inline]
fn pixel_changed(ra: &[u8], rb: &[u8], off: usize, tolerance: u8) -> bool {
    ra[off..off + 4]
        .iter()
        .zip(&rb[off..off + 4])
        .map(|(p, q)| p.abs_diff(*q))
        .max()
        .unwrap_or(0)
        > tolerance
}

/// Compare two same-size frames. A pixel counts as changed when the maximum
/// per-channel absolute difference exceeds `tolerance`.
pub fn diff(a: &Frame, b: &Frame, tolerance: u8) -> Result<DiffResult> {
    diff_with_mask(a, b, tolerance, &IgnoreMask::empty())
}

/// Like [`diff`], but pixels covered by `mask` are excluded: they never count as
/// changed, never extend the bbox, and are removed from the `changed_pct`
/// denominator. The mask never mutates pixel data.
pub fn diff_with_mask(
    a: &Frame,
    b: &Frame,
    tolerance: u8,
    mask: &IgnoreMask,
) -> Result<DiffResult> {
    if a.width != b.width || a.height != b.height {
        return Err(GlassError::SizeMismatch {
            a: (a.width, a.height),
            b: (b.width, b.height),
        });
    }
    let row_bytes = a.width as usize * 4;
    let tol_vec = Simd::splat(tolerance);
    let mut changed = 0u64;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);

    for y in 0..a.height {
        let base = y as usize * row_bytes;
        let ra = &a.pixels[base..base + row_bytes];
        let rb = &b.pixels[base..base + row_bytes];
        let masked_row = !mask.spans_for_row(y).is_empty();
        let mut off = 0usize;
        let mut col = 0u32;
        // SIMD over full 32-byte (8-pixel) chunks: skip chunks with no change.
        while off + LANES <= row_bytes {
            let chunk_end = col + (LANES / 4) as u32;
            // Whole-chunk skip: the cheap win when a mask is large.
            if masked_row && mask.covers_span(y, col, chunk_end) {
                off += LANES;
                col = chunk_end;
                continue;
            }
            let va = u8x32::from_slice(&ra[off..off + LANES]);
            let vb = u8x32::from_slice(&rb[off..off + LANES]);
            let d = va.simd_max(vb) - va.simd_min(vb);
            if d.simd_gt(tol_vec).any() {
                for px in 0..(LANES / 4) {
                    let cx = col + px as u32;
                    if masked_row && mask.is_ignored(cx, y) {
                        continue;
                    }
                    if pixel_changed(ra, rb, off + px * 4, tolerance) {
                        changed += 1;
                        min_x = min_x.min(cx);
                        min_y = min_y.min(y);
                        max_x = max_x.max(cx);
                        max_y = max_y.max(y);
                    }
                }
            }
            off += LANES;
            col = chunk_end;
        }
        // Scalar tail (< 8 pixels left in the row).
        while off < row_bytes {
            if !(masked_row && mask.is_ignored(col, y)) && pixel_changed(ra, rb, off, tolerance) {
                changed += 1;
                min_x = min_x.min(col);
                min_y = min_y.min(y);
                max_x = max_x.max(col);
                max_y = max_y.max(y);
            }
            off += 4;
            col += 1;
        }
    }

    let total = a.pixel_count();
    let ignored = mask.ignored_count().min(total);
    let bbox = (changed > 0).then(|| BBox {
        x: min_x,
        y: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    });
    Ok(DiffResult {
        changed_pixels: changed,
        total_pixels: total,
        changed_pct: pct(changed, total - ignored),
        bbox,
        aa_ignored: 0,
        ignored_pixels: ignored,
    })
}

/// `changed` as a percentage of `considered`; 0.0 when nothing was considered.
#[inline]
fn pct(changed: u64, considered: u64) -> f32 {
    if considered > 0 {
        (changed as f64 / considered as f64 * 100.0) as f32
    } else {
        0.0
    }
}

// ---------------------------------------------------------------------------
// Perceptual diff — odiff/Honeydiff-class (the pixelmatch algorithm): a YIQ
// perceptual color delta plus conservative anti-alias suppression. Used for
// baseline regression, where cross-render anti-aliasing / sub-pixel / GPU-font
// noise makes the exact diff untrustworthy. `wait_stable` keeps the exact diff.
// ---------------------------------------------------------------------------

/// Largest YIQ perceptual delta the metric can report; scales `threshold`.
const MAX_YIQ_DELTA: f32 = 35215.0;

// The canonical pixelmatch YIQ coefficients (kept at full precision for
// traceability; f32 rounds them, hence the expect).
#[inline]
#[expect(
    clippy::excessive_precision,
    reason = "canonical pixelmatch YIQ coefficients kept at full precision for traceability; f32 narrows them"
)]
fn rgb2y(r: f32, g: f32, b: f32) -> f32 {
    r * 0.29889531 + g * 0.58662247 + b * 0.11448223
}
#[inline]
#[expect(
    clippy::excessive_precision,
    reason = "canonical pixelmatch YIQ coefficients kept at full precision for traceability; f32 narrows them"
)]
fn rgb2i(r: f32, g: f32, b: f32) -> f32 {
    r * 0.59597799 - g * 0.27417610 - b * 0.32180189
}
#[inline]
#[expect(
    clippy::excessive_precision,
    reason = "canonical pixelmatch YIQ coefficients kept at full precision for traceability; f32 narrows them"
)]
fn rgb2q(r: f32, g: f32, b: f32) -> f32 {
    r * 0.21147017 - g * 0.52261711 + b * 0.31114694
}

/// RGB of pixel `off`, blended over neutral gray when translucent. Screenshots are
/// normally opaque (a == 255), so the common path returns the raw RGB.
#[inline]
fn blended_rgb(px: &[u8], off: usize) -> (f32, f32, f32) {
    let a = px[off + 3] as f32 * (1.0 / 255.0);
    if a >= 1.0 {
        return (px[off] as f32, px[off + 1] as f32, px[off + 2] as f32);
    }
    const BG: f32 = 128.0;
    (
        BG + (px[off] as f32 - BG) * a,
        BG + (px[off + 1] as f32 - BG) * a,
        BG + (px[off + 2] as f32 - BG) * a,
    )
}

/// Signed YIQ perceptual delta between `a[oa..]` and `b[ob..]`. Magnitude is in
/// `[0, MAX_YIQ_DELTA]`; the sign follows the luminance delta (used by anti-alias
/// detection). `y_only` returns just the luminance delta (for neighbor brightness).
#[inline]
fn color_delta(a: &[u8], oa: usize, b: &[u8], ob: usize, y_only: bool) -> f32 {
    if a[oa..oa + 4] == b[ob..ob + 4] {
        return 0.0;
    }
    let (ar, ag, ab) = blended_rgb(a, oa);
    let (br, bg, bb) = blended_rgb(b, ob);
    let dy = rgb2y(ar, ag, ab) - rgb2y(br, bg, bb);
    if y_only {
        return dy;
    }
    let di = rgb2i(ar, ag, ab) - rgb2i(br, bg, bb);
    let dq = rgb2q(ar, ag, ab) - rgb2q(br, bg, bb);
    let delta = 0.5053 * dy * dy + 0.299 * di * di + 0.1957 * dq * dq;
    if dy < 0.0 {
        -delta
    } else {
        delta
    }
}

/// True if the pixel at (x,y) has 3+ identical neighbors (frame edges count) — the
/// flat-region marker the anti-alias test uses to confirm an edge.
fn has_many_siblings(px: &[u8], x: u32, y: u32, w: u32, h: u32) -> bool {
    let x0 = x.saturating_sub(1);
    let y0 = y.saturating_sub(1);
    let x2 = (x + 1).min(w - 1);
    let y2 = (y + 1).min(h - 1);
    let pos = ((y * w + x) * 4) as usize;
    let mut zeroes = u32::from(x == x0 || x == x2 || y == y0 || y == y2);
    for ny in y0..=y2 {
        for nx in x0..=x2 {
            if nx == x && ny == y {
                continue;
            }
            let p2 = ((ny * w + nx) * 4) as usize;
            if px[pos..pos + 4] == px[p2..p2 + 4] {
                zeroes += 1;
                if zeroes > 2 {
                    return true;
                }
            }
        }
    }
    false
}

/// pixelmatch anti-alias detection: the pixel at (x,y) differs between `px` and
/// `other`; is the difference attributable to anti-aliasing (so it shouldn't count
/// as a real change)? Conservative — only true when the neighborhood looks like an
/// anti-aliased edge in *both* images.
fn is_antialiased(px: &[u8], x: u32, y: u32, w: u32, h: u32, other: &[u8]) -> bool {
    let x0 = x.saturating_sub(1);
    let y0 = y.saturating_sub(1);
    let x2 = (x + 1).min(w - 1);
    let y2 = (y + 1).min(h - 1);
    let pos = ((y * w + x) * 4) as usize;
    let mut zeroes = u32::from(x == x0 || x == x2 || y == y0 || y == y2);
    let (mut min_d, mut max_d) = (0f32, 0f32);
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (0u32, 0u32, 0u32, 0u32);
    for ny in y0..=y2 {
        for nx in x0..=x2 {
            if nx == x && ny == y {
                continue;
            }
            let p2 = ((ny * w + nx) * 4) as usize;
            let delta = color_delta(px, pos, px, p2, true);
            if delta == 0.0 {
                zeroes += 1;
                if zeroes > 2 {
                    return false;
                }
            } else if delta < min_d {
                min_d = delta;
                (min_x, min_y) = (nx, ny);
            } else if delta > max_d {
                max_d = delta;
                (max_x, max_y) = (nx, ny);
            }
        }
    }
    if min_d == 0.0 || max_d == 0.0 {
        return false;
    }
    (has_many_siblings(px, min_x, min_y, w, h) && has_many_siblings(other, min_x, min_y, w, h))
        || (has_many_siblings(px, max_x, max_y, w, h)
            && has_many_siblings(other, max_x, max_y, w, h))
}

enum PixelClass {
    Same,
    Changed,
    AntiAliased,
}

/// Classify one pixel: unchanged, a real perceptual change, or an anti-alias artifact.
#[inline]
fn classify(a: &[u8], b: &[u8], x: u32, y: u32, w: u32, h: u32, max_delta: f32) -> PixelClass {
    let off = ((y * w + x) * 4) as usize;
    if color_delta(a, off, b, off, false).abs() <= max_delta {
        return PixelClass::Same;
    }
    if is_antialiased(a, x, y, w, h, b) || is_antialiased(b, x, y, w, h, a) {
        PixelClass::AntiAliased
    } else {
        PixelClass::Changed
    }
}

/// Compare two same-size frames perceptually. See [`diff_perceptual_with_mask`];
/// pixels covered by `mask` are excluded exactly as in [`diff_with_mask`].
pub fn diff_perceptual(a: &Frame, b: &Frame, threshold: f32) -> Result<DiffResult> {
    diff_perceptual_with_mask(a, b, threshold, &IgnoreMask::empty())
}

/// Like [`diff_perceptual`], but pixels covered by `mask` are excluded: they never
/// count as changed or anti-aliased, never extend the bbox, and are removed from the
/// `changed_pct` denominator. Neighbour reads for anti-alias classification still hit
/// the unmodified frames, so a masked pixel's real value can still confirm an edge in
/// an unmasked neighbour. `threshold` ∈ [0,1] sets sensitivity (smaller = stricter;
/// ~0.1 is a sensible default).
pub fn diff_perceptual_with_mask(
    a: &Frame,
    b: &Frame,
    threshold: f32,
    mask: &IgnoreMask,
) -> Result<DiffResult> {
    if a.width != b.width || a.height != b.height {
        return Err(GlassError::SizeMismatch {
            a: (a.width, a.height),
            b: (b.width, b.height),
        });
    }
    let (w, h) = (a.width, a.height);
    let t = threshold.clamp(0.0, 1.0);
    let max_delta = MAX_YIQ_DELTA * t * t;
    let row_bytes = w as usize * 4;
    let mut changed = 0u64;
    let mut aa_ignored = 0u64;
    let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);

    for y in 0..h {
        let base = y as usize * row_bytes;
        let ra = &a.pixels[base..base + row_bytes];
        let rb = &b.pixels[base..base + row_bytes];
        let masked_row = !mask.spans_for_row(y).is_empty();
        let mut off = 0usize;
        let mut col = 0u32;
        // SIMD pre-scan: byte-identical 8-pixel chunks (the common case) can't
        // contain a change, so skip the per-pixel perceptual + AA work entirely.
        while off + LANES <= row_bytes {
            let chunk_end = col + (LANES / 4) as u32;
            // Whole-chunk skip: the cheap win when a mask is large.
            if masked_row && mask.covers_span(y, col, chunk_end) {
                off += LANES;
                col = chunk_end;
                continue;
            }
            if u8x32::from_slice(&ra[off..off + LANES]) != u8x32::from_slice(&rb[off..off + LANES])
            {
                for px in 0..(LANES / 4) as u32 {
                    let cx = col + px;
                    if masked_row && mask.is_ignored(cx, y) {
                        continue;
                    }
                    classify_into(
                        a,
                        b,
                        cx,
                        y,
                        w,
                        h,
                        max_delta,
                        &mut changed,
                        &mut aa_ignored,
                        &mut min_x,
                        &mut min_y,
                        &mut max_x,
                        &mut max_y,
                    );
                }
            }
            off += LANES;
            col = chunk_end;
        }
        while off < row_bytes {
            if !(masked_row && mask.is_ignored(col, y)) {
                classify_into(
                    a,
                    b,
                    col,
                    y,
                    w,
                    h,
                    max_delta,
                    &mut changed,
                    &mut aa_ignored,
                    &mut min_x,
                    &mut min_y,
                    &mut max_x,
                    &mut max_y,
                );
            }
            off += 4;
            col += 1;
        }
    }

    let total = a.pixel_count();
    let ignored = mask.ignored_count().min(total);
    let bbox = (changed > 0).then(|| BBox {
        x: min_x,
        y: min_y,
        width: max_x - min_x + 1,
        height: max_y - min_y + 1,
    });
    Ok(DiffResult {
        changed_pixels: changed,
        total_pixels: total,
        changed_pct: pct(changed, total - ignored),
        bbox,
        aa_ignored,
        ignored_pixels: ignored,
    })
}

/// Classify the pixel at (x,y) and fold it into the running counters/bbox.
#[inline]
#[expect(
    clippy::too_many_arguments,
    reason = "hot per-pixel classifier; threads counters/bbox by &mut to avoid per-pixel allocation"
)]
fn classify_into(
    a: &Frame,
    b: &Frame,
    x: u32,
    y: u32,
    w: u32,
    h: u32,
    max_delta: f32,
    changed: &mut u64,
    aa_ignored: &mut u64,
    min_x: &mut u32,
    min_y: &mut u32,
    max_x: &mut u32,
    max_y: &mut u32,
) {
    match classify(&a.pixels, &b.pixels, x, y, w, h, max_delta) {
        PixelClass::Same => {}
        PixelClass::AntiAliased => *aa_ignored += 1,
        PixelClass::Changed => {
            *changed += 1;
            *min_x = (*min_x).min(x);
            *min_y = (*min_y).min(y);
            *max_x = (*max_x).max(x);
            *max_y = (*max_y).max(y);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn identical_frames_report_no_change() {
        let a = Frame::solid(4, 4, [1, 2, 3, 255]);
        let b = a.clone();
        let r = diff(&a, &b, 0).unwrap();
        assert_eq!(r.changed_pixels, 0);
        assert_eq!(r.total_pixels, 16);
        assert_eq!(r.bbox, None);
        assert_eq!(r.changed_pct, 0.0);
    }

    #[test]
    fn single_changed_pixel_is_located() {
        let a = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut b = a.clone();
        // change pixel at (x=1, y=2) => index 2*4 + 1 = 9, byte offset 36
        let off = (2 * 4 + 1) * 4;
        b.pixels[off] = 255;
        let r = diff(&a, &b, 0).unwrap();
        assert_eq!(r.changed_pixels, 1);
        assert_eq!(
            r.bbox,
            Some(BBox {
                x: 1,
                y: 2,
                width: 1,
                height: 1
            })
        );
    }

    #[test]
    fn changes_within_tolerance_are_ignored() {
        let a = Frame::solid(2, 2, [100, 100, 100, 255]);
        let mut b = a.clone();
        b.pixels[0] = 105; // delta 5
        let r = diff(&a, &b, 10).unwrap();
        assert_eq!(r.changed_pixels, 0);
    }

    #[test]
    fn bbox_spans_all_changes() {
        let a = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut b = a.clone();
        for (x, y) in [(1u32, 1u32), (3, 2)] {
            let off = ((y * 4 + x) * 4) as usize;
            b.pixels[off] = 255;
        }
        let r = diff(&a, &b, 0).unwrap();
        assert_eq!(r.changed_pixels, 2);
        assert_eq!(
            r.bbox,
            Some(BBox {
                x: 1,
                y: 1,
                width: 3,
                height: 2
            })
        );
    }

    #[test]
    fn size_mismatch_errors() {
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(3, 2, [0, 0, 0, 255]);
        assert!(matches!(
            diff(&a, &b, 0).unwrap_err(),
            GlassError::SizeMismatch { .. }
        ));
    }

    /// Independent scalar reference (the pre-optimization algorithm) used to
    /// cross-check the optimized `diff` across sizes — including widths that are
    /// NOT multiples of the SIMD lane width, and degenerate frames.
    fn reference_diff(a: &Frame, b: &Frame, tolerance: u8) -> DiffResult {
        let mut changed = 0u64;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for i in 0..(a.pixel_count() as usize) {
            let off = i * 4;
            let delta = a.pixels[off..off + 4]
                .iter()
                .zip(&b.pixels[off..off + 4])
                .map(|(x, y)| x.abs_diff(*y))
                .max()
                .unwrap_or(0);
            if delta > tolerance {
                changed += 1;
                let x = (i as u32) % a.width;
                let y = (i as u32) / a.width;
                min_x = min_x.min(x);
                min_y = min_y.min(y);
                max_x = max_x.max(x);
                max_y = max_y.max(y);
            }
        }
        let total = a.pixel_count();
        let bbox = (changed > 0).then(|| BBox {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        });
        let changed_pct = if total > 0 {
            (changed as f64 / total as f64 * 100.0) as f32
        } else {
            0.0
        };
        DiffResult {
            changed_pixels: changed,
            total_pixels: total,
            changed_pct,
            bbox,
            aa_ignored: 0,
            ignored_pixels: 0,
        }
    }

    fn make(w: u32, h: u32, seed: u32) -> Frame {
        let n = (w as usize) * (h as usize) * 4;
        let px = (0..n)
            .map(|i| (i as u32).wrapping_mul(2_654_435_761).wrapping_add(seed) as u8)
            .collect();
        Frame::new(w, h, px).unwrap()
    }

    #[test]
    fn simd_matches_scalar_reference() {
        // Sizes chosen to exercise full chunks, tails, and degenerate cases.
        let sizes = [
            (0u32, 0u32),
            (1, 1),
            (7, 1),
            (1, 7),
            (8, 1),
            (9, 3),
            (13, 7),
            (32, 2),
            (33, 2),
            (64, 4),
            (100, 50),
        ];
        for &(w, h) in &sizes {
            let a = make(w, h, 0);
            assert_eq!(
                diff(&a, &a, 0).unwrap(),
                reference_diff(&a, &a, 0),
                "identical {w}x{h}"
            );
            let b = make(w, h, 7);
            for tol in [0u8, 10, 255] {
                assert_eq!(
                    diff(&a, &b, tol).unwrap(),
                    reference_diff(&a, &b, tol),
                    "{w}x{h} tol={tol}"
                );
            }
            if w > 0 && h > 0 {
                let mut c = a.clone();
                let last = c.pixels.len() - 4;
                c.pixels[last] ^= 0xFF;
                assert_eq!(
                    diff(&a, &c, 0).unwrap(),
                    reference_diff(&a, &c, 0),
                    "one-changed {w}x{h}"
                );
            }
        }
    }

    // ---- perceptual diff ----

    /// Black on the left, an anti-aliased gray seam at column `seam`, then white —
    /// every row identical. Shifting `seam` models an anti-aliased edge moving 1px.
    fn edge_frame(w: u32, h: u32, seam: u32) -> Frame {
        let mut px = vec![0u8; (w * h * 4) as usize];
        for y in 0..h {
            for x in 0..w {
                let off = ((y * w + x) * 4) as usize;
                let v: u8 = if x < seam {
                    0
                } else if x == seam {
                    128
                } else {
                    255
                };
                px[off..off + 4].copy_from_slice(&[v, v, v, 255]);
            }
        }
        Frame::new(w, h, px).unwrap()
    }

    #[test]
    fn perceptual_identical_no_change() {
        let a = Frame::solid(6, 6, [10, 20, 30, 255]);
        let r = diff_perceptual(&a, &a.clone(), 0.1).unwrap();
        assert_eq!(r.changed_pixels, 0);
        assert_eq!(r.aa_ignored, 0);
        assert_eq!(r.bbox, None);
    }

    #[test]
    fn color_delta_properties() {
        let black = [0u8, 0, 0, 255];
        let white = [255u8, 255, 255, 255];
        assert_eq!(color_delta(&black, 0, &black, 0, false), 0.0);
        let d = color_delta(&black, 0, &white, 0, false);
        assert!(d.abs() > 30_000.0, "black/white delta too small: {d}");
        // magnitude is order-independent
        assert_eq!(d.abs(), color_delta(&white, 0, &black, 0, false).abs());
    }

    #[test]
    fn perceptual_full_recolor_counts_every_pixel() {
        // A uniform recolor has no edges, so nothing is anti-aliasing: all count.
        let a = Frame::solid(10, 10, [0, 0, 0, 255]);
        let b = Frame::solid(10, 10, [255, 255, 255, 255]);
        let r = diff_perceptual(&a, &b, 0.1).unwrap();
        assert_eq!(r.changed_pixels, 100);
        assert_eq!(r.aa_ignored, 0);
        assert_eq!(
            r.bbox,
            Some(BBox {
                x: 0,
                y: 0,
                width: 10,
                height: 10
            })
        );
    }

    #[test]
    fn perceptual_suppresses_antialiased_edge_shift() {
        // The exact diff flags the moved anti-aliased seam; perceptual recognizes it.
        let a = edge_frame(8, 8, 3);
        let b = edge_frame(8, 8, 4);
        let exact = diff(&a, &b, 0).unwrap();
        let perc = diff_perceptual(&a, &b, 0.1).unwrap();
        assert!(exact.changed_pixels > 0, "exact should see the shift");
        assert!(
            perc.aa_ignored > 0,
            "perceptual should suppress some pixels as AA"
        );
        assert!(
            perc.changed_pixels < exact.changed_pixels,
            "perceptual ({}) should report fewer changes than exact ({})",
            perc.changed_pixels,
            exact.changed_pixels
        );
    }

    #[test]
    fn perceptual_threshold_is_monotonic() {
        let a = make(40, 30, 0);
        let b = make(40, 30, 9);
        let strict = diff_perceptual(&a, &b, 0.05).unwrap();
        let loose = diff_perceptual(&a, &b, 0.3).unwrap();
        assert!(
            loose.changed_pixels <= strict.changed_pixels,
            "looser threshold ({}) reported more than stricter ({})",
            loose.changed_pixels,
            strict.changed_pixels
        );
        assert!(strict.changed_pixels + strict.aa_ignored <= strict.total_pixels);
    }

    #[test]
    fn perceptual_size_mismatch_errors() {
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(3, 2, [0, 0, 0, 255]);
        assert!(matches!(
            diff_perceptual(&a, &b, 0.1).unwrap_err(),
            GlassError::SizeMismatch { .. }
        ));
    }

    /// Naive per-pixel reference (no SIMD pre-scan, no chunking) — guards the
    /// optimized `diff_perceptual`'s loop/bbox against the straightforward result.
    fn reference_perceptual(a: &Frame, b: &Frame, threshold: f32) -> DiffResult {
        let (w, h) = (a.width, a.height);
        let max_delta = MAX_YIQ_DELTA * threshold.clamp(0.0, 1.0).powi(2);
        let mut changed = 0u64;
        let mut aa_ignored = 0u64;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                match classify(&a.pixels, &b.pixels, x, y, w, h, max_delta) {
                    PixelClass::Same => {}
                    PixelClass::AntiAliased => aa_ignored += 1,
                    PixelClass::Changed => {
                        changed += 1;
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                    }
                }
            }
        }
        let total = a.pixel_count();
        let bbox = (changed > 0).then(|| BBox {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        });
        let changed_pct = if total > 0 {
            (changed as f64 / total as f64 * 100.0) as f32
        } else {
            0.0
        };
        DiffResult {
            changed_pixels: changed,
            total_pixels: total,
            changed_pct,
            bbox,
            aa_ignored,
            ignored_pixels: 0,
        }
    }

    #[test]
    fn perceptual_matches_naive_reference() {
        // Widths spanning SIMD-chunk boundaries and tails, against the naive loop.
        let sizes = [
            (1u32, 1u32),
            (7, 3),
            (8, 8),
            (9, 9),
            (33, 17),
            (64, 8),
            (100, 40),
        ];
        for &(w, h) in &sizes {
            let a = make(w, h, 1);
            let b = make(w, h, 5);
            for thr in [0.02f32, 0.1, 0.4] {
                assert_eq!(
                    diff_perceptual(&a, &b, thr).unwrap(),
                    reference_perceptual(&a, &b, thr),
                    "{w}x{h} thr={thr}"
                );
            }
            assert_eq!(
                diff_perceptual(&a, &a.clone(), 0.1).unwrap(),
                reference_perceptual(&a, &a.clone(), 0.1),
                "identical {w}x{h}"
            );
        }
    }

    fn diff_result(changed: u64) -> DiffResult {
        DiffResult {
            changed_pixels: changed,
            total_pixels: 100,
            changed_pct: changed as f32,
            bbox: if changed > 0 {
                Some(BBox {
                    x: 0,
                    y: 0,
                    width: 1,
                    height: 1,
                })
            } else {
                None
            },
            aa_ignored: 0,
            ignored_pixels: 0,
        }
    }

    #[test]
    fn region_changes_satisfied_when_pixels_differ() {
        assert!(region_satisfied(&diff_result(5), RegionUntil::Changes));
        assert!(!region_satisfied(&diff_result(0), RegionUntil::Changes));
    }

    #[test]
    fn region_matches_satisfied_when_identical() {
        assert!(region_satisfied(&diff_result(0), RegionUntil::Matches));
        assert!(!region_satisfied(&diff_result(5), RegionUntil::Matches));
    }

    // ---- ignore mask ----

    fn rect(x: u32, y: u32, width: u32, height: u32) -> Region {
        Region {
            x,
            y,
            width,
            height,
        }
    }

    #[test]
    fn empty_mask_ignores_nothing() {
        let m = IgnoreMask::new(&[], 10, 10).unwrap();
        assert!(m.is_empty());
        assert_eq!(m.ignored_count(), 0);
        assert!(!m.is_ignored(0, 0));
    }

    #[test]
    fn mask_counts_a_single_rect() {
        let m = IgnoreMask::new(&[rect(1, 1, 2, 3)], 10, 10).unwrap();
        assert_eq!(m.ignored_count(), 6);
        assert!(m.is_ignored(1, 1));
        assert!(m.is_ignored(2, 3));
        assert!(!m.is_ignored(3, 1), "x=3 is outside [1,3)");
        assert!(!m.is_ignored(1, 4), "y=4 is outside [1,4)");
    }

    #[test]
    fn overlapping_rects_count_each_pixel_once() {
        // Two 4x4 rects overlapping in a 2x2 corner: 16 + 16 - 4 = 28.
        let m = IgnoreMask::new(&[rect(0, 0, 4, 4), rect(2, 2, 4, 4)], 10, 10).unwrap();
        assert_eq!(m.ignored_count(), 28);
    }

    #[test]
    fn adjacent_spans_merge_into_one() {
        let m = IgnoreMask::new(&[rect(0, 0, 2, 1), rect(2, 0, 2, 1)], 10, 10).unwrap();
        assert_eq!(m.spans_for_row(0), &[(0, 4)]);
        assert_eq!(m.ignored_count(), 4);
    }

    #[test]
    fn rect_partly_out_of_bounds_is_clamped() {
        // 4 wide starting at x=8 in a 10-wide frame => only x in [8,10) masks.
        let m = IgnoreMask::new(&[rect(8, 0, 4, 1)], 10, 10).unwrap();
        assert_eq!(m.ignored_count(), 2);
        assert_eq!(m.spans_for_row(0), &[(8, 10)]);
    }

    #[test]
    fn rect_fully_out_of_bounds_ignores_nothing_and_does_not_error() {
        let m = IgnoreMask::new(&[rect(50, 50, 4, 4)], 10, 10).unwrap();
        assert!(m.is_empty());
        assert_eq!(m.ignored_count(), 0);
    }

    #[test]
    fn zero_area_rect_is_an_error() {
        assert!(matches!(
            IgnoreMask::new(&[rect(0, 0, 0, 4)], 10, 10).unwrap_err(),
            GlassError::InvalidRegion(_)
        ));
        assert!(matches!(
            IgnoreMask::new(&[rect(0, 0, 4, 0)], 10, 10).unwrap_err(),
            GlassError::InvalidRegion(_)
        ));
    }

    #[test]
    fn covers_span_detects_fully_masked_runs() {
        let m = IgnoreMask::new(&[rect(0, 0, 8, 1)], 16, 4).unwrap();
        assert!(m.covers_span(0, 0, 8), "[0,8) is inside [0,8)");
        assert!(!m.covers_span(0, 4, 12), "[4,12) runs past the mask");
        assert!(!m.covers_span(1, 0, 8), "row 1 is unmasked");
    }

    #[test]
    fn for_region_intersects_and_translates_into_region_space() {
        // Frame 100x100, region at (10,10) 20x20, mask rect at (15,15) 10x10.
        // Intersection is (15,15)-(25,25) clipped to the region => (15,15) 10x10,
        // translated to region-local (5,5) 10x10 => 100 px.
        let region = rect(10, 10, 20, 20);
        let m = IgnoreMask::for_region(&[rect(15, 15, 10, 10)], Some(&region), 100, 100).unwrap();
        assert_eq!(m.ignored_count(), 100);
        assert!(m.is_ignored(5, 5), "region-local origin of the mask");
        assert!(!m.is_ignored(4, 5), "just outside the translated mask");
    }

    #[test]
    fn for_region_drops_rects_outside_the_region() {
        let region = rect(0, 0, 10, 10);
        let m = IgnoreMask::for_region(&[rect(50, 50, 5, 5)], Some(&region), 100, 100).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn region_intersect_returns_none_when_disjoint() {
        assert_eq!(rect(0, 0, 5, 5).intersect(&rect(10, 10, 5, 5)), None);
        assert_eq!(
            rect(0, 0, 10, 10).intersect(&rect(5, 5, 10, 10)),
            Some(rect(5, 5, 5, 5))
        );
    }

    // ---- masked exact diff ----

    #[test]
    fn masked_pixels_do_not_count_as_changed() {
        let a = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut b = a.clone();
        // Change (1,2) and (3,3); mask only (1,2).
        for (x, y) in [(1u32, 2u32), (3, 3)] {
            b.pixels[((y * 4 + x) * 4) as usize] = 255;
        }
        let mask = IgnoreMask::new(&[rect(1, 2, 1, 1)], 4, 4).unwrap();
        let r = diff_with_mask(&a, &b, 0, &mask).unwrap();
        assert_eq!(r.changed_pixels, 1, "only the unmasked change counts");
        assert_eq!(r.ignored_pixels, 1);
        assert_eq!(
            r.bbox,
            Some(BBox {
                x: 3,
                y: 3,
                width: 1,
                height: 1
            }),
            "bbox must not stretch to the masked pixel"
        );
    }

    #[test]
    fn changed_pct_uses_the_considered_denominator() {
        // 4x4 = 16 px; mask 8 px; 1 changed px among the 8 considered => 12.5%.
        let a = Frame::solid(4, 4, [0, 0, 0, 255]);
        let mut b = a.clone();
        b.pixels[0] = 255; // (0,0), unmasked
        let mask = IgnoreMask::new(&[rect(0, 2, 4, 2)], 4, 4).unwrap();
        let r = diff_with_mask(&a, &b, 0, &mask).unwrap();
        assert_eq!(r.ignored_pixels, 8);
        assert_eq!(r.total_pixels, 16);
        assert_eq!(r.changed_pixels, 1);
        assert!((r.changed_pct - 12.5).abs() < 1e-4, "got {}", r.changed_pct);
    }

    #[test]
    fn fully_masked_frame_reports_zeros() {
        let a = Frame::solid(4, 4, [0, 0, 0, 255]);
        let b = Frame::solid(4, 4, [255, 255, 255, 255]);
        let mask = IgnoreMask::new(&[rect(0, 0, 4, 4)], 4, 4).unwrap();
        let r = diff_with_mask(&a, &b, 0, &mask).unwrap();
        assert_eq!(r.changed_pixels, 0);
        assert_eq!(r.bbox, None);
        assert_eq!(r.changed_pct, 0.0);
        assert_eq!(r.ignored_pixels, r.total_pixels);
    }

    #[test]
    fn unmasked_diff_is_unchanged_by_the_new_field() {
        let a = make(33, 9, 0);
        let b = make(33, 9, 4);
        let old = diff(&a, &b, 0).unwrap();
        let new = diff_with_mask(&a, &b, 0, &IgnoreMask::empty()).unwrap();
        assert_eq!(old, new);
        assert_eq!(old.ignored_pixels, 0);
    }

    #[test]
    fn masked_simd_matches_masked_scalar_reference() {
        // Masks chosen to land on and across 8-pixel SIMD chunk boundaries.
        let sizes = [
            (1u32, 1u32),
            (7, 3),
            (8, 8),
            (9, 9),
            (33, 17),
            (64, 8),
            (100, 40),
        ];
        for &(w, h) in &sizes {
            let a = make(w, h, 0);
            let b = make(w, h, 7);
            let masks = mask_matrix(w, h);
            for (label, m) in masks {
                for tol in [0u8, 10] {
                    assert_eq!(
                        diff_with_mask(&a, &b, tol, &m).unwrap(),
                        reference_diff_masked(&a, &b, tol, &m),
                        "{w}x{h} tol={tol} mask={label}"
                    );
                }
            }
        }
    }

    /// Mask shapes that exercise empty, sub-chunk, chunk-aligned, cross-chunk,
    /// full-row, and full-frame coverage.
    fn mask_matrix(w: u32, h: u32) -> Vec<(&'static str, IgnoreMask)> {
        let mut out = vec![("empty", IgnoreMask::empty())];
        let mk = |label, rects: Vec<Region>| (label, IgnoreMask::new(&rects, w, h).unwrap());
        if w > 0 && h > 0 {
            out.push(mk("single-px", vec![rect(0, 0, 1, 1)]));
            out.push(mk("full-frame", vec![rect(0, 0, w, h)]));
            out.push(mk("first-row", vec![rect(0, 0, w, 1)]));
        }
        if w >= 8 && h >= 2 {
            out.push(mk("chunk-aligned", vec![rect(0, 0, 8, 2)]));
            out.push(mk("cross-chunk", vec![rect(4, 0, 8, 2)]));
            out.push(mk("overlapping", vec![rect(0, 0, 6, 2), rect(3, 0, 6, 2)]));
        }
        out
    }

    /// Naive masked reference — the straightforward loop the optimized masked
    /// diff must agree with.
    fn reference_diff_masked(a: &Frame, b: &Frame, tolerance: u8, mask: &IgnoreMask) -> DiffResult {
        let mut changed = 0u64;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..a.height {
            for x in 0..a.width {
                if mask.is_ignored(x, y) {
                    continue;
                }
                let off = ((y * a.width + x) * 4) as usize;
                if pixel_changed(&a.pixels, &b.pixels, off, tolerance) {
                    changed += 1;
                    min_x = min_x.min(x);
                    min_y = min_y.min(y);
                    max_x = max_x.max(x);
                    max_y = max_y.max(y);
                }
            }
        }
        let total = a.pixel_count();
        let ignored = mask.ignored_count().min(total);
        let considered = total - ignored;
        let bbox = (changed > 0).then(|| BBox {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        });
        let changed_pct = if considered > 0 {
            (changed as f64 / considered as f64 * 100.0) as f32
        } else {
            0.0
        };
        DiffResult {
            changed_pixels: changed,
            total_pixels: total,
            changed_pct,
            bbox,
            aa_ignored: 0,
            ignored_pixels: ignored,
        }
    }

    // ---- masked perceptual diff ----

    #[test]
    fn perceptual_mask_excludes_pixels_and_keeps_denominator_honest() {
        let a = Frame::solid(10, 10, [0, 0, 0, 255]);
        let b = Frame::solid(10, 10, [255, 255, 255, 255]);
        // Mask the top 5 rows: 50 of 100 px considered, all of which changed.
        let mask = IgnoreMask::new(&[rect(0, 0, 10, 5)], 10, 10).unwrap();
        let r = diff_perceptual_with_mask(&a, &b, 0.1, &mask).unwrap();
        assert_eq!(r.ignored_pixels, 50);
        assert_eq!(r.changed_pixels, 50);
        assert!(
            (r.changed_pct - 100.0).abs() < 1e-4,
            "got {}",
            r.changed_pct
        );
        assert_eq!(
            r.bbox,
            Some(BBox {
                x: 0,
                y: 5,
                width: 10,
                height: 5
            })
        );
    }

    #[test]
    fn perceptual_unmasked_is_unchanged() {
        let a = make(33, 17, 1);
        let b = make(33, 17, 5);
        let old = diff_perceptual(&a, &b, 0.1).unwrap();
        let new = diff_perceptual_with_mask(&a, &b, 0.1, &IgnoreMask::empty()).unwrap();
        assert_eq!(old, new);
    }

    /// The decisive guard on masking in-loop rather than copying frame A's masked
    /// rects into frame B before diffing: anti-alias detection must keep reading
    /// real neighbours, so a pixel next to a mask classifies exactly as it would
    /// from the true, unmutated frame data.
    ///
    /// Geometry is load-bearing here, and deliberately small — do not "simplify"
    /// this to a bigger/rounder frame. `is_antialiased`'s "3+ identical
    /// neighbours" flat-region check (`has_many_siblings`) is satisfied by *any*
    /// matching neighbour; a wide or tall frame is mostly uniform black/white
    /// padding, which hands it redundant confirmations everywhere, including
    /// right next to a mask. That redundancy is exactly what let a previous,
    /// larger version of this fixture (16x8, 2 masked rows) pass identically
    /// under both the in-loop and the copy-into-B designs — the two designs
    /// only diverge when the surviving row bordering the mask has *no* spare
    /// matching neighbour to fall back on.
    ///
    /// A 4-wide, 3-row frame with only row 0 masked gives row 1 (the row right
    /// below the mask) exactly that: at the seam, row 1's own vertical neighbour
    /// is row 0. Verified by hand-tracing `is_antialiased` and by an experimental
    /// copy-into-B implementation: under the correct design, pixels (1,1) and
    /// (2,1) classify `Changed` (row 1 reads B's real row 0, seam at column 2);
    /// under the rejected design they classify `AntiAliased` instead, because
    /// row 0 in the diffed copy of B would hold A's seam (column 1) copied in
    /// before diffing — a value B never actually had. Row 2 isn't adjacent to
    /// the mask, so its classification (`AntiAliased`) doesn't move either way;
    /// it is what makes this fixture assert something beyond "row 1 disappeared".
    #[test]
    fn perceptual_mask_does_not_disturb_neighbouring_aa_classification() {
        // Seam 1 -> 2 in a 4-wide frame differs only at columns 1 and 2:
        // a = [black, gray(seam), white, white], b = [black, black, gray(seam), white].
        let a = edge_frame(4, 3, 1);
        let b = edge_frame(4, 3, 2);
        // Mask only row 0, leaving row 1 (adjacent to the mask) and row 2 (not
        // adjacent) to survive.
        let mask = IgnoreMask::new(&[rect(0, 0, 4, 1)], 4, 3).unwrap();

        let masked = diff_perceptual_with_mask(&a, &b, 0.1, &mask).unwrap();

        // The real invariant: every surviving pixel must classify exactly as it
        // would from the true, unmutated frames. `reference_perceptual_masked`
        // is that definition made concrete — it calls `classify` on `a.pixels`
        // and `b.pixels` verbatim and never mutates either frame. Equality here
        // is what "in-loop masking never disturbs anti-alias classification"
        // means; it would still hold trivially if this fixture didn't
        // discriminate, which is why the concrete counts below matter too.
        let reference = reference_perceptual_masked(&a, &b, 0.1, &mask);
        assert_eq!(
            masked, reference,
            "masked diff must match direct per-pixel classification of the real, unmutated frames"
        );

        // Pin the concrete counts so a regression is legible without diffing a
        // DiffResult by hand (see the doc comment above for how these were
        // derived and cross-checked against the rejected design).
        assert_eq!(masked.ignored_pixels, 4, "row 0 (4 px) is excluded");
        assert_eq!(
            masked.changed_pixels, 2,
            "(1,1) and (2,1): row 1's seam pixels see B's real row-0 neighbour, not A's copied-in seam"
        );
        assert_eq!(
            masked.aa_ignored, 2,
            "(1,2) and (2,2): row 2's seam pixels are unaffected by the mask either way"
        );
        assert_eq!(
            masked.bbox,
            Some(BBox {
                x: 1,
                y: 1,
                width: 2,
                height: 1
            }),
            "only row 1's pixels are real changes; row 2's are suppressed as AA"
        );
    }

    #[test]
    fn masked_perceptual_matches_masked_naive_reference() {
        let sizes = [(1u32, 1u32), (7, 3), (8, 8), (9, 9), (33, 17), (64, 8)];
        for &(w, h) in &sizes {
            let a = make(w, h, 1);
            let b = make(w, h, 5);
            for (label, m) in mask_matrix(w, h) {
                for thr in [0.02f32, 0.1, 0.4] {
                    assert_eq!(
                        diff_perceptual_with_mask(&a, &b, thr, &m).unwrap(),
                        reference_perceptual_masked(&a, &b, thr, &m),
                        "{w}x{h} thr={thr} mask={label}"
                    );
                }
            }
        }
    }

    fn reference_perceptual_masked(
        a: &Frame,
        b: &Frame,
        threshold: f32,
        mask: &IgnoreMask,
    ) -> DiffResult {
        let (w, h) = (a.width, a.height);
        let max_delta = MAX_YIQ_DELTA * threshold.clamp(0.0, 1.0).powi(2);
        let mut changed = 0u64;
        let mut aa_ignored = 0u64;
        let (mut min_x, mut min_y, mut max_x, mut max_y) = (u32::MAX, u32::MAX, 0u32, 0u32);
        for y in 0..h {
            for x in 0..w {
                if mask.is_ignored(x, y) {
                    continue;
                }
                match classify(&a.pixels, &b.pixels, x, y, w, h, max_delta) {
                    PixelClass::Same => {}
                    PixelClass::AntiAliased => aa_ignored += 1,
                    PixelClass::Changed => {
                        changed += 1;
                        min_x = min_x.min(x);
                        min_y = min_y.min(y);
                        max_x = max_x.max(x);
                        max_y = max_y.max(y);
                    }
                }
            }
        }
        let total = a.pixel_count();
        let ignored = mask.ignored_count().min(total);
        let bbox = (changed > 0).then(|| BBox {
            x: min_x,
            y: min_y,
            width: max_x - min_x + 1,
            height: max_y - min_y + 1,
        });
        DiffResult {
            changed_pixels: changed,
            total_pixels: total,
            changed_pct: pct(changed, total - ignored),
            bbox,
            aa_ignored,
            ignored_pixels: ignored,
        }
    }
}
