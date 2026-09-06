use crate::error::{GlassError, Result};
use crate::frame::{Frame, Region};
use fearless_simd::{dispatch, prelude::*, u8x32};

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
    /// Reject a zero-area rect up front: it can never mask anything, so it is a
    /// caller bug worth naming rather than silently dropping. Shared by [`new`]
    /// and [`for_region`] so both entry points validate identically.
    ///
    /// [`new`]: Self::new
    /// [`for_region`]: Self::for_region
    fn ensure_no_zero_area(rects: &[Region]) -> Result<()> {
        for r in rects {
            if r.width == 0 || r.height == 0 {
                return Err(GlassError::InvalidRegion(format!(
                    "ignore rect has zero area: {}x{} at ({},{})",
                    r.width, r.height, r.x, r.y
                )));
            }
        }
        Ok(())
    }

    /// Build a mask over a `width`×`height` area. Rects are clamped to that area;
    /// a rect entirely outside contributes nothing (its mistake shows up as a zero
    /// `ignored_count`, not an error). A zero-area rect is a caller bug and errors.
    pub fn new(rects: &[Region], width: u32, height: u32) -> Result<Self> {
        Self::ensure_no_zero_area(rects)?;
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

        if ignored == 0 {
            // Every rect fell outside the area, so this masks nothing. Collapse to
            // the canonical empty mask rather than keeping a rows-of-empty-spans
            // representation, so an all-out-of-bounds list compares `Eq` with
            // `empty()` — both exclude exactly nothing.
            return Ok(Self::default());
        }
        Ok(Self { rows, ignored })
    }

    /// Build a mask for a comparison scoped to `region`: each rect is intersected
    /// with the region and translated into region-local coordinates, so callers
    /// always pass window-relative rects regardless of scoping. The mask is sized
    /// from the region — the space the scoped comparison runs in. A zero-area rect
    /// is rejected up front, before intersecting, so region-scoping can't launder
    /// it into a silent drop.
    pub fn for_region(rects: &[Region], region: &Region) -> Result<Self> {
        Self::ensure_no_zero_area(rects)?;
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
    diff_with_mask(a, b, tolerance, &IgnoreMask::default())
}

/// Like [`diff`], but pixels covered by `mask` are excluded: they never count as
/// changed, never extend the bbox, and are removed from the `changed_pct`
/// denominator. The mask never mutates pixel data.
///
/// The `mask` must be built for `a`'s dimensions. A mask sized for a different
/// frame silently miscompares — it masks the wrong columns/rows and can subtract
/// more than the frame holds; the internal `.min(total)` clamp only keeps the
/// arithmetic sound, not the result meaningful.
pub fn diff_with_mask(
    a: &Frame,
    b: &Frame,
    tolerance: u8,
    mask: &IgnoreMask,
) -> Result<DiffResult> {
    dispatch!(crate::simd_level(), simd => diff_with_mask_simd(simd, a, b, tolerance, mask))
}

#[inline(always)]
fn diff_with_mask_simd<S: Simd>(
    simd: S,
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
    let tol_vec = u8x32::splat(simd, tolerance);
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
            let va = u8x32::from_slice(simd, &ra[off..off + LANES]);
            let vb = u8x32::from_slice(simd, &rb[off..off + LANES]);
            let d = va.max(vb) - va.min(vb);
            let byte_changes = d.simd_gt(tol_vec);
            if byte_changes.any_true() {
                let byte_flags =
                    byte_changes.select(u8x32::splat(simd, 255), u8x32::splat(simd, 0));
                // Four byte predicates form one nonzero word per changed RGBA pixel.
                let word_flags: fearless_simd::u32x8<S> = byte_flags.bitcast();
                let mut bits = word_flags
                    .simd_gt(fearless_simd::u32x8::splat(simd, 0))
                    .to_bitmask() as u8;
                if masked_row {
                    let mut remaining = bits;
                    while remaining != 0 {
                        let px = remaining.trailing_zeros();
                        remaining &= remaining - 1;
                        if mask.is_ignored(col + px, y) {
                            bits &= !(1 << px);
                        }
                    }
                }
                if bits != 0 {
                    changed += u64::from(bits.count_ones());
                    min_x = min_x.min(col + bits.trailing_zeros());
                    max_x = max_x.max(col + 7 - bits.leading_zeros());
                    min_y = min_y.min(y);
                    max_y = max_y.max(y);
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
    if dy < 0.0 { -delta } else { delta }
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
    diff_perceptual_with_mask(a, b, threshold, &IgnoreMask::default())
}

/// Like [`diff_perceptual`], but pixels covered by `mask` are excluded: they never
/// count as changed or anti-aliased, never extend the bbox, and are removed from the
/// `changed_pct` denominator. Neighbour reads for anti-alias classification still hit
/// the unmodified frames, so a masked pixel's real value can still confirm an edge in
/// an unmasked neighbour. `threshold` ∈ [0,1] sets sensitivity (smaller = stricter;
/// ~0.1 is a sensible default).
///
/// The `mask` must be built for `a`'s dimensions. A mask sized for a different
/// frame silently miscompares — it masks the wrong columns/rows and can subtract
/// more than the frame holds; the internal `.min(total)` clamp only keeps the
/// arithmetic sound, not the result meaningful.
pub fn diff_perceptual_with_mask(
    a: &Frame,
    b: &Frame,
    threshold: f32,
    mask: &IgnoreMask,
) -> Result<DiffResult> {
    dispatch!(crate::simd_level(), simd => diff_perceptual_with_mask_simd(simd, a, b, threshold, mask))
}

#[inline(always)]
fn diff_perceptual_with_mask_simd<S: Simd>(
    simd: S,
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
            if u8x32::from_slice(simd, &ra[off..off + LANES])
                .simd_eq(u8x32::from_slice(simd, &rb[off..off + LANES]))
                .any_false()
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

    /// Each channel weighted by its own coefficient and summed. Asserted on the primaries
    /// rather than on white, where the coefficients sum to 1 and any mix-up still yields 255.
    #[test]
    fn yiq_conversion_weights_each_channel() {
        let near = |got: f32, want: f32| assert!((got - want).abs() < 0.01, "{got} != {want}");
        near(rgb2y(255.0, 0.0, 0.0), 76.2183);
        near(rgb2y(0.0, 255.0, 0.0), 149.5887);
        near(rgb2y(0.0, 0.0, 255.0), 29.1930);
        near(rgb2y(255.0, 255.0, 255.0), 255.0);

        // I and Q subtract two of their three channels; a sign flip lands on the wrong side
        // of zero, which an absolute-value assertion would miss.
        near(rgb2i(255.0, 0.0, 0.0), 151.9744);
        near(rgb2i(0.0, 255.0, 0.0), -69.9149);
        near(rgb2i(0.0, 0.0, 255.0), -82.0595);
        near(rgb2q(255.0, 0.0, 0.0), 53.9249);
        near(rgb2q(0.0, 255.0, 0.0), -133.2674);
        near(rgb2q(0.0, 0.0, 255.0), 79.3425);

        // Grey carries no chroma, so both chroma axes are zero however the terms are ordered.
        near(rgb2i(255.0, 255.0, 255.0), 0.0);
        near(rgb2q(255.0, 255.0, 255.0), 0.0);
    }

    /// Opaque returns the raw channels; anything translucent is composited over neutral grey.
    #[test]
    fn blended_rgb_composites_only_when_translucent() {
        let near = |got: f32, want: f32| assert!((got - want).abs() < 0.01, "{got} != {want}");

        let opaque = [200u8, 100, 50, 255];
        let (r, g, b) = blended_rgb(&opaque, 0);
        near(r, 200.0);
        near(g, 100.0);
        near(b, 50.0);

        // Fully transparent is the background itself, whatever the colour channels say.
        let clear = [200u8, 100, 50, 0];
        let (r, g, b) = blended_rgb(&clear, 0);
        near(r, 128.0);
        near(g, 128.0);
        near(b, 128.0);

        // Half alpha lands between the two, and on the side the channel sits: 200 is above
        // the background and 0 below it, so an inverted blend would show up as a swap.
        // No channel may sit *on* the background: at 128 the blend term is zero, so adding
        // or subtracting it gives the same answer and the operator is unpinned.
        let half = [200u8, 0, 60, 128];
        let (r, g, b) = blended_rgb(&half, 0);
        near(r, 164.1412);
        near(g, 63.7490);
        near(b, 93.8667);
    }

    /// `y_only` is the signed luminance difference, and it is what anti-alias detection reads.
    #[test]
    fn color_delta_y_only_is_the_signed_luminance_difference() {
        let near = |got: f32, want: f32| assert!((got - want).abs() < 0.01, "{got} != {want}");
        let black = [0u8, 0, 0, 255];
        let white = [255u8, 255, 255, 255];
        near(color_delta(&black, 0, &white, 0, true), -255.0);
        near(color_delta(&white, 0, &black, 0, true), 255.0);

        // Pure red against black differs in luminance by red's own Y weight, so `y_only`
        // cannot be returning the full three-axis delta.
        let red = [255u8, 0, 0, 255];
        near(color_delta(&red, 0, &black, 0, true), 76.2183);

        // Exact, not an order-of-magnitude bound: the weighted sum is four operators, and a
        // bound this loose accepts most ways of getting them wrong.
        near(color_delta(&red, 0, &black, 0, false), 10410.246);
        // The sign follows the luminance delta, so the reverse is the negation.
        near(color_delta(&black, 0, &red, 0, false), -10410.246);
    }

    /// Equal luminance, different chroma. `dy` is exactly zero, which is the only input that
    /// separates `dy < 0.0` from `dy <= 0.0` or `dy == 0.0`, and the delta stays non-zero, so
    /// the I and Q terms must both reach it.
    #[test]
    fn color_delta_at_zero_luminance_difference_stays_positive() {
        let near = |got: f32, want: f32| assert!((got - want).abs() < 0.01, "{got} != {want}");
        // Both sides come to Y = 111.22486 to the bit.
        let a = [60u8, 120, 200, 255];
        let b = [67u8, 150, 28, 255];
        near(color_delta(&a, 0, &b, 0, true), 0.0);
        near(color_delta(&a, 0, &b, 0, false), 1684.1283);
    }

    /// `is_empty` reads the running count rather than answering a constant.
    #[test]
    fn ignore_mask_is_empty_tracks_what_was_masked() {
        assert!(IgnoreMask::default().is_empty());
        let masked = IgnoreMask::new(
            &[Region {
                x: 1,
                y: 1,
                width: 2,
                height: 2,
            }],
            10,
            10,
        )
        .unwrap();
        assert!(!masked.is_empty());
        assert_eq!(masked.ignored_count(), 4);
    }

    /// A pixel counts a sibling only where the whole RGBA quad matches; a frame edge counts
    /// as one on its own, which is what lets a corner pixel qualify.
    #[test]
    fn has_many_siblings_counts_identical_neighbours() {
        let px = |c: [u8; 4]| c;
        // 3x3, uniform: the centre has eight identical neighbours.
        let uniform: Vec<u8> = std::iter::repeat_n(px([10, 20, 30, 255]), 9)
            .flatten()
            .collect();
        assert!(has_many_siblings(&uniform, 1, 1, 3, 3));

        // Centre differs from all eight, and is not on an edge, so it has none.
        let mut lone = uniform.clone();
        lone[16..20].copy_from_slice(&[99, 99, 99, 255]);
        assert!(!has_many_siblings(&lone, 1, 1, 3, 3));

        // Alpha is part of the comparison: same RGB, different A, is not a sibling.
        let mut alpha: Vec<u8> = std::iter::repeat_n(px([10, 20, 30, 255]), 9)
            .flatten()
            .collect();
        for i in 0..9 {
            if i != 4 {
                alpha[i * 4 + 3] = 128;
            }
        }
        assert!(!has_many_siblings(&alpha, 1, 1, 3, 3));

        // Exactly two matches: the threshold is "more than two", so this is still false. All
        // the cases above have either eight matches or none, and those cannot tell a
        // `zeroes > 2` from a `zeroes < 2` — both answer the same on either side.
        let mut two: Vec<u8> = std::iter::repeat_n(px([99, 99, 99, 255]), 9)
            .flatten()
            .collect();
        let centre = [10u8, 20, 30, 255];
        two[16..20].copy_from_slice(&centre);
        two[0..4].copy_from_slice(&centre);
        two[4..8].copy_from_slice(&centre);
        assert!(!has_many_siblings(&two, 1, 1, 3, 3));

        // And one more match tips it over.
        two[8..12].copy_from_slice(&centre);
        assert!(has_many_siblings(&two, 1, 1, 3, 3));
    }

    /// A 5x5 vertical edge: two black columns, one mid-grey column, two white columns. The
    /// grey column is what an anti-aliased edge looks like, and (2,2) sits in it.
    fn aa_edge() -> Vec<u8> {
        let mut px = Vec::with_capacity(5 * 5 * 4);
        for _y in 0..5 {
            for x in 0..5u32 {
                let v = match x {
                    0 | 1 => 0u8,
                    2 => 128,
                    _ => 255,
                };
                px.extend_from_slice(&[v, v, v, 255]);
            }
        }
        px
    }

    /// True needs all of it: a darker neighbour, a brighter one, few identical ones, and both
    /// extremes sitting in flat regions of *both* images.
    #[test]
    fn is_antialiased_accepts_an_edge_and_rejects_what_is_not_one() {
        let edge = aa_edge();
        assert!(is_antialiased(&edge, 2, 2, 5, 5, &edge));

        // Uniform: every neighbour is identical, so the third one gives up early.
        let flat: Vec<u8> = std::iter::repeat_n([70u8, 70, 70, 255], 25)
            .flatten()
            .collect();
        assert!(!is_antialiased(&flat, 2, 2, 5, 5, &flat));

        // Brightest in its neighbourhood: every delta is positive, so there is no darker
        // neighbour and `min_d` never moves off zero.
        let mut brightest = flat.clone();
        brightest[(2 * 5 + 2) * 4..(2 * 5 + 2) * 4 + 4].copy_from_slice(&[200, 200, 200, 255]);
        assert!(!is_antialiased(&brightest, 2, 2, 5, 5, &brightest));

        // And the mirror: darkest, so `max_d` never moves. Both halves of the same guard.
        let mut darkest = flat.clone();
        darkest[(2 * 5 + 2) * 4..(2 * 5 + 2) * 4 + 4].copy_from_slice(&[10, 10, 10, 255]);
        assert!(!is_antialiased(&darkest, 2, 2, 5, 5, &darkest));

        // Every border pixel, so the neighbourhood is clamped on each side in turn: a bound
        // that clamps past the last row or column indexes out of the buffer. The grey column
        // still reads as an edge where it meets the top and bottom borders; the flat frame
        // never does, wherever it is sampled.
        for (x, y) in [(0, 2), (4, 2), (0, 0), (4, 4)] {
            assert!(!is_antialiased(&edge, x, y, 5, 5, &edge), "edge at {x},{y}");
        }
        for (x, y) in [(2, 0), (2, 4)] {
            assert!(is_antialiased(&edge, x, y, 5, 5, &edge), "edge at {x},{y}");
        }
        for (x, y) in [(0, 2), (4, 2), (2, 0), (2, 4), (0, 0), (4, 4)] {
            assert!(!is_antialiased(&flat, x, y, 5, 5, &flat), "flat at {x},{y}");
        }
    }

    /// The frame edge counts as one identical neighbour here too, and unlike in
    /// `has_many_siblings` it pushes the count toward *rejecting* the pixel. Built so the
    /// bonus is decisive: two real matches plus the edge is three, which bails out early;
    /// without the bonus it is two, and the run continues to a positive answer.
    #[test]
    fn is_antialiased_counts_the_frame_edge_toward_its_bail_out() {
        let mut f = Vec::with_capacity(5 * 5 * 4);
        for _y in 0..5 {
            for x in 0..5u32 {
                let v: u8 = match x {
                    0 | 1 => 0,
                    2 => 128,
                    _ => 255,
                };
                f.extend_from_slice(&[v, v, v, 255]);
            }
        }
        // Give the top-edge grey pixel a second identical neighbour.
        let at = |x: usize, y: usize| (y * 5 + x) * 4;
        f[at(1, 1)..at(1, 1) + 4].copy_from_slice(&[128, 128, 128, 255]);
        assert!(!is_antialiased(&f, 2, 0, 5, 5, &f));

        // The same again on a side edge with an interior row, where the column terms are the
        // only ones true — a top-edge pixel cannot distinguish them.
        let mut g = Vec::with_capacity(5 * 5 * 4);
        for y in 0..5u32 {
            for _x in 0..5 {
                let v: u8 = match y {
                    0 | 1 => 0,
                    2 => 128,
                    _ => 255,
                };
                g.extend_from_slice(&[v, v, v, 255]);
            }
        }
        g[at(1, 1)..at(1, 1) + 4].copy_from_slice(&[128, 128, 128, 255]);
        assert!(!is_antialiased(&g, 0, 2, 5, 5, &g));
    }

    /// The edge must look flat in the *other* image too — that is what stops a real change
    /// from being written off as anti-aliasing.
    #[test]
    fn is_antialiased_requires_the_other_image_to_agree() {
        let edge = aa_edge();
        // Same geometry, but the other image is noise, so neither extreme has siblings there.
        let noisy: Vec<u8> = (0..25u8)
            .flat_map(|i| {
                [
                    i.wrapping_mul(37),
                    i.wrapping_mul(11),
                    i.wrapping_mul(29),
                    255,
                ]
            })
            .collect();
        assert!(!is_antialiased(&edge, 2, 2, 5, 5, &noisy));
    }

    /// classify routes on the outcome of both, so an anti-aliased edge is not counted as
    /// changed while a genuine recolor is.
    #[test]
    fn classify_separates_anti_aliasing_from_a_real_change() {
        let edge = aa_edge();
        assert!(matches!(
            classify(&edge, &edge, 2, 2, 5, 5, 0.0),
            PixelClass::Same
        ));

        let mut moved = edge.clone();
        // Shift the grey column one pixel: the edge is in a different place, which is what an
        // anti-aliased render difference looks like.
        for y in 0..5usize {
            let at = |x: usize| (y * 5 + x) * 4;
            moved[at(2)..at(2) + 4].copy_from_slice(&[0, 0, 0, 255]);
            moved[at(3)..at(3) + 4].copy_from_slice(&[128, 128, 128, 255]);
        }
        assert!(matches!(
            classify(&edge, &moved, 3, 2, 5, 5, 100.0),
            PixelClass::AntiAliased
        ));
    }

    /// A frame edge counts as a sibling on its own. Asserted with *exactly two* real
    /// neighbours, so the pixel qualifies only if the edge bonus is added: with it the count
    /// is three, without it two, and the threshold sits between them.
    #[test]
    fn has_many_siblings_counts_the_frame_edge() {
        let mut f = vec![0u8; 3 * 3 * 4];
        let put = |f: &mut Vec<u8>, x: usize, y: usize, v: [u8; 4]| {
            let o = (y * 3 + x) * 4;
            f[o..o + 4].copy_from_slice(&v);
        };
        let c = [128u8, 128, 128, 255];
        for y in 0..3 {
            for x in 0..3 {
                put(&mut f, x, y, [9, 9, 9, 255]);
            }
        }
        // Bottom edge, interior column: only `y == y2` is true, so this pins that term alone.
        put(&mut f, 1, 2, c);
        put(&mut f, 0, 2, c);
        put(&mut f, 1, 1, c);
        assert!(has_many_siblings(&f, 1, 2, 3, 3));

        // Left edge, interior row: only `x == x0`.
        let mut g = vec![0u8; 3 * 3 * 4];
        for y in 0..3 {
            for x in 0..3 {
                put(&mut g, x, y, [9, 9, 9, 255]);
            }
        }
        put(&mut g, 0, 1, c);
        put(&mut g, 0, 0, c);
        put(&mut g, 1, 1, c);
        assert!(has_many_siblings(&g, 0, 1, 3, 3));
    }

    /// A 5x5 opaque greyscale image from a grid of luminances.
    fn grey5(rows: [[u8; 5]; 5]) -> Vec<u8> {
        rows.iter()
            .flatten()
            .flat_map(|&v| [v, v, v, 255])
            .collect()
    }

    /// Overwrite the RGB of pixel (x,y) in a 5x5 greyscale image.
    fn shade5(px: &mut [u8], x: usize, y: usize, v: u8) {
        let o = (y * 5 + x) * 4;
        px[o..o + 3].copy_from_slice(&[v, v, v]);
    }

    /// The sole *brighter* neighbour sits directly below the centre, so skipping the centre's
    /// column — or reading the centre from another row — loses the bright side of the edge.
    #[test]
    fn is_antialiased_reads_every_neighbour_of_the_centre() {
        let px = grey5([
            [50, 50, 50, 0, 0],
            [50, 50, 50, 60, 0],
            [50, 50, 100, 61, 0],
            [0, 71, 200, 73, 0],
            [0, 0, 0, 0, 0],
        ]);
        let mut other = px.clone();
        shade5(&mut other, 2, 2, 110); // the difference under test
        assert!(is_antialiased(&px, 2, 2, 5, 5, &other));
    }

    /// One column past the right edge, a neighbour is the first pixel of the *next* row —
    /// three of those match the centre here, enough zero-deltas to call the edge flat.
    #[test]
    fn is_antialiased_clamps_the_neighbourhood_to_the_right_edge() {
        let px = grey5([
            [0, 0, 0, 0, 0],
            [0, 0, 0, 60, 50],
            [100, 0, 0, 61, 100],
            [100, 0, 0, 62, 200],
            [100, 0, 0, 200, 200],
        ]);
        let mut other = px.clone();
        shade5(&mut other, 4, 2, 110);
        assert!(is_antialiased(&px, 4, 2, 5, 5, &other));
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
    fn perceptual_change_below_the_first_row_is_found() {
        // Both the row slice and the per-pixel offset have to follow `y`; the frame is 8 wide
        // because a narrower one never reaches the SIMD pre-scan.
        let a = Frame::solid(8, 2, [0, 0, 0, 255]);
        let mut b = a.clone();
        let (x, y) = (3usize, 1usize);
        let off = (y * 8 + x) * 4;
        b.pixels[off..off + 3].copy_from_slice(&[255, 255, 255]);
        assert_eq!(diff_perceptual(&a, &b, 0.1).unwrap().changed_pixels, 1);
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
        let m = IgnoreMask::for_region(&[rect(15, 15, 10, 10)], &region).unwrap();
        assert_eq!(m.ignored_count(), 100);
        assert!(m.is_ignored(5, 5), "region-local origin of the mask");
        assert!(!m.is_ignored(4, 5), "just left of the translated mask");
        assert!(!m.is_ignored(5, 4), "just above the translated mask");
    }

    #[test]
    fn for_region_drops_rects_outside_the_region() {
        let region = rect(0, 0, 10, 10);
        let m = IgnoreMask::for_region(&[rect(50, 50, 5, 5)], &region).unwrap();
        assert!(m.is_empty());
    }

    #[test]
    fn for_region_rejects_a_zero_area_rect_before_intersecting() {
        // `for_region` validates zero-area up front, before intersecting with the region, so
        // region-scoping can't launder a zero-area rect into a silent drop.
        let region = rect(0, 0, 10, 10);
        assert!(matches!(
            IgnoreMask::for_region(&[rect(0, 0, 0, 4)], &region).unwrap_err(),
            GlassError::InvalidRegion(_)
        ));
    }

    #[test]
    fn region_intersect_returns_none_when_disjoint() {
        assert_eq!(rect(0, 0, 5, 5).intersect(&rect(10, 10, 5, 5)), None);
        assert_eq!(
            rect(0, 0, 10, 10).intersect(&rect(5, 5, 10, 10)),
            Some(rect(5, 5, 5, 5))
        );
    }

    #[test]
    fn region_intersect_returns_none_for_regions_that_only_touch_side_by_side() {
        // A zero-width overlap is no overlap — an empty region would flow on into
        // `IgnoreMask::new`, which rejects zero area.
        assert_eq!(rect(0, 0, 5, 5).intersect(&rect(5, 0, 5, 5)), None);
    }

    #[test]
    fn region_intersect_returns_none_for_regions_that_only_touch_end_to_end() {
        assert_eq!(rect(0, 0, 5, 5).intersect(&rect(0, 5, 5, 5)), None);
    }

    #[test]
    fn region_intersect_needs_both_axes_to_overlap() {
        assert_eq!(rect(0, 0, 5, 5).intersect(&rect(0, 9, 5, 5)), None);
    }

    /// 1 against 255 is the adversarial pair: far apart, but any combination of the two other
    /// than a difference wraps through zero and reads as clean.
    #[test]
    fn the_simd_prescan_never_skips_a_chunk_holding_a_change() {
        let one_pixel = |v: u8| {
            let mut p = vec![0u8; 32]; // one 8-pixel SIMD chunk
            p[0] = v;
            Frame::new(8, 1, p).unwrap()
        };
        let d = diff(&one_pixel(1), &one_pixel(255), 0).unwrap();
        assert_eq!(d.changed_pixels, 1);
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
        let new = diff_with_mask(&a, &b, 0, &IgnoreMask::default()).unwrap();
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
                for tol in [0u8, 1, 10, 127, 254, 255] {
                    let expected = reference_diff_masked(&a, &b, tol, &m);
                    for level in [fearless_simd::Level::baseline(), crate::simd_level()] {
                        assert_eq!(
                            dispatch!(level, simd => diff_with_mask_simd(simd, &a, &b, tol, &m))
                                .unwrap(),
                            expected,
                            "{level:?} {w}x{h} tol={tol} mask={label}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn simd_pixel_bits_match_scalar_for_every_channel_and_mask() {
        let (w, h) = (17, 3);
        let a = Frame::solid(w, h, [0; 4]);
        for pattern in 0u16..=255 {
            for channel in 0..4 {
                let mut b = a.clone();
                for (px, pixel) in b.pixels.as_chunks_mut::<4>().0.iter_mut().enumerate() {
                    if pattern & (1 << (px % w as usize % 8)) != 0 {
                        pixel[channel] = [1, 127, 128, 254, 255][px % 5];
                    }
                }
                for (label, mask) in mask_matrix(w, h) {
                    for tolerance in [0, 1, 127, 254, 255] {
                        let expected = reference_diff_masked(&a, &b, tolerance, &mask);
                        for level in [fearless_simd::Level::baseline(), crate::simd_level()] {
                            assert_eq!(
                                dispatch!(level, simd => diff_with_mask_simd(simd, &a, &b, tolerance, &mask)).unwrap(),
                                expected,
                                "{level:?} pattern={pattern:08b} channel={channel} tolerance={tolerance} mask={label}"
                            );
                        }
                    }
                }
            }
        }
    }

    /// Mask shapes that exercise empty, sub-chunk, chunk-aligned, cross-chunk,
    /// full-row, and full-frame coverage.
    fn mask_matrix(w: u32, h: u32) -> Vec<(&'static str, IgnoreMask)> {
        let mut out = vec![("empty", IgnoreMask::default())];
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
        let new = diff_perceptual_with_mask(&a, &b, 0.1, &IgnoreMask::default()).unwrap();
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

        // The real invariant: every surviving pixel must classify exactly as it would from
        // the true, unmutated frames. `reference_perceptual_masked` is that definition made
        // concrete — it calls `classify` on `a.pixels` and `b.pixels` verbatim and mutates
        // neither frame.
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
                for thr in [0.0f32, 0.02, 0.1, 0.4, 1.0] {
                    let expected = reference_perceptual_masked(&a, &b, thr, &m);
                    for level in [fearless_simd::Level::baseline(), crate::simd_level()] {
                        assert_eq!(
                            dispatch!(level, simd => diff_perceptual_with_mask_simd(simd, &a, &b, thr, &m)).unwrap(),
                            expected,
                            "{level:?} {w}x{h} thr={thr} mask={label}"
                        );
                    }
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
