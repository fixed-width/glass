use crate::error::{GlassError, Result};
use crate::frame::Frame;
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
        let mut off = 0usize;
        let mut col = 0u32;
        // SIMD over full 32-byte (8-pixel) chunks: skip chunks with no change.
        while off + LANES <= row_bytes {
            let va = u8x32::from_slice(&ra[off..off + LANES]);
            let vb = u8x32::from_slice(&rb[off..off + LANES]);
            let d = va.simd_max(vb) - va.simd_min(vb);
            if d.simd_gt(tol_vec).any() {
                for px in 0..(LANES / 4) {
                    if pixel_changed(ra, rb, off + px * 4, tolerance) {
                        let cx = col + px as u32;
                        changed += 1;
                        min_x = min_x.min(cx);
                        min_y = min_y.min(y);
                        max_x = max_x.max(cx);
                        max_y = max_y.max(y);
                    }
                }
            }
            off += LANES;
            col += (LANES / 4) as u32;
        }
        // Scalar tail (< 8 pixels left in the row).
        while off < row_bytes {
            if pixel_changed(ra, rb, off, tolerance) {
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
    Ok(DiffResult {
        changed_pixels: changed,
        total_pixels: total,
        changed_pct,
        bbox,
        aa_ignored: 0,
    })
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

/// Compare two same-size frames perceptually. `threshold` ∈ [0,1] sets sensitivity
/// (smaller = stricter; ~0.1 is a sensible default). A pixel counts as changed only
/// when its perceptual delta exceeds the threshold **and** it isn't anti-aliasing in
/// either frame; suppressed anti-alias pixels are reported in `aa_ignored`.
pub fn diff_perceptual(a: &Frame, b: &Frame, threshold: f32) -> Result<DiffResult> {
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
        let mut off = 0usize;
        let mut col = 0u32;
        // SIMD pre-scan: byte-identical 8-pixel chunks (the common case) can't
        // contain a change, so skip the per-pixel perceptual + AA work entirely.
        while off + LANES <= row_bytes {
            if u8x32::from_slice(&ra[off..off + LANES]) != u8x32::from_slice(&rb[off..off + LANES])
            {
                for px in 0..(LANES / 4) as u32 {
                    classify_into(
                        a,
                        b,
                        col + px,
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
            col += (LANES / 4) as u32;
        }
        while off < row_bytes {
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
            off += 4;
            col += 1;
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
    Ok(DiffResult {
        changed_pixels: changed,
        total_pixels: total,
        changed_pct,
        bbox,
        aa_ignored,
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
}
