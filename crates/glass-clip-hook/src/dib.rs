//! Pure DIB / DIBV5 byte-layout parsing + validation + header rewrite (no Win32, Miri-checked).
//!
//! `CF_DIB`/`CF_DIBV5` blobs are attacker-influenced (they come from a boxed app's clipboard), so
//! every size is computed with checked arithmetic and validated against the actual buffer length —
//! this is where an out-of-bounds read or integer overflow would otherwise hide. The `cfg(windows)`
//! hook only hands GDI (`CreateDIBitmap`) a layout that parsed clean here.

const BIH: usize = 40; // BITMAPINFOHEADER
const BV5: usize = 124; // BITMAPV5HEADER

/// Validated geometry of a DIB blob (header + optional color table + pixel bits).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct DibInfo {
    pub width: i32,
    pub height: i32, // may be negative (top-down); magnitude used for size
    pub bpp: u16,
    pub header_bytes: usize,      // 40 or 124
    pub color_table_bytes: usize, // palette size in bytes
    pub stride: usize,            // DWORD-aligned bytes per scan line
    pub image_bytes: usize,       // stride * |height|
}

fn rd_u16(b: &[u8], o: usize) -> Option<u16> {
    Some(u16::from_le_bytes(b.get(o..o + 2)?.try_into().ok()?))
}
fn rd_u32(b: &[u8], o: usize) -> Option<u32> {
    Some(u32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}
fn rd_i32(b: &[u8], o: usize) -> Option<i32> {
    Some(i32::from_le_bytes(b.get(o..o + 4)?.try_into().ok()?))
}

/// Parse a header of declared size `header_bytes` (40 or 124). Fields at offsets shared by BIH/BV5.
fn parse_with_header(b: &[u8], header_bytes: usize) -> Option<DibInfo> {
    if b.len() < header_bytes {
        return None;
    }
    let size = rd_u32(b, 0)? as usize;
    if size != header_bytes {
        return None;
    }
    let width = rd_i32(b, 4)?;
    let height = rd_i32(b, 8)?;
    let bpp = rd_u16(b, 14)?;
    let compression = rd_u32(b, 16)?;
    let clr_used = rd_u32(b, 32)?;
    if width <= 0 || bpp == 0 || compression != 0 {
        return None; // top-down requires width>0; only BI_RGB (0) handled in v2a-i; reject others
    }
    let abs_h = (height as i64).unsigned_abs();
    // color table: for <=8bpp, clr_used (or 2^bpp if 0) entries of 4 bytes.
    let color_table_bytes: usize = if bpp <= 8 {
        let entries = if clr_used == 0 { 1u64 << bpp } else { clr_used as u64 };
        if entries > 256 {
            return None; // a palette can't exceed 2^8
        }
        (entries as usize) * 4
    } else {
        // High-bpp packed DIBs may carry a biClrUsed optimization palette
        // (4 bytes/entry) between header and pixels; count it so the pixel
        // offset stays correct. biClrUsed==0 (the common case) yields 0. Any
        // absurd value is caught by the buffer-length check below, not here.
        (clr_used as u64).checked_mul(4)?.try_into().ok()?
    };
    // stride = ((width*bpp + 31)/32)*4, all in u64 to avoid overflow.
    let bits = (width as u64).checked_mul(bpp as u64)?;
    let stride = ((bits.checked_add(31)?) / 32).checked_mul(4)?;
    let image = stride.checked_mul(abs_h)?;
    if image > (crate::proto::MAX_ITEM_BYTES as u64) {
        return None;
    }
    let stride = stride as usize;
    let image_bytes = image as usize;
    let need = header_bytes
        .checked_add(color_table_bytes)?
        .checked_add(image_bytes)?;
    if b.len() < need {
        return None; // buffer shorter than the geometry declares
    }
    Some(DibInfo {
        width,
        height,
        bpp,
        header_bytes,
        color_table_bytes,
        stride,
        image_bytes,
    })
}

pub(crate) fn parse_dib(b: &[u8]) -> Option<DibInfo> {
    parse_with_header(b, BIH)
}
/// Used by the host-side server path (DIBV5→DIB narrowing) and the tests; the DLL hook only ever
/// widens DIB→DIBV5, so this is dead on a non-test DLL-only build.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn parse_dibv5(b: &[u8]) -> Option<DibInfo> {
    parse_with_header(b, BV5)
}

/// `CF_DIB` (BITMAPINFOHEADER) → `CF_DIBV5` (BITMAPV5HEADER): widen the header to 124 bytes (zero the
/// new fields; the OS supplies sRGB defaults when color-space is absent), keep table + bits verbatim.
pub(crate) fn dib_to_dibv5(b: &[u8]) -> Option<Vec<u8>> {
    let d = parse_dib(b)?;
    let mut out = vec![0u8; BV5];
    out[..BIH].copy_from_slice(&b[..BIH]);
    out[0..4].copy_from_slice(&(BV5 as u32).to_le_bytes()); // biSize = 124
    out.extend_from_slice(&b[BIH..BIH + d.color_table_bytes + d.image_bytes]);
    Some(out)
}

/// `CF_DIBV5` → `CF_DIB`: narrow the header back to 40 bytes; keep table + bits verbatim.
///
/// Used by the host-side server path + the tests; the DLL hook only ever widens DIB→DIBV5.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn dibv5_to_dib(b: &[u8]) -> Option<Vec<u8>> {
    let d = parse_dibv5(b)?;
    let mut out = vec![0u8; BIH];
    out.copy_from_slice(&b[..BIH]);
    out[0..4].copy_from_slice(&(BIH as u32).to_le_bytes());
    out.extend_from_slice(&b[BV5..BV5 + d.color_table_bytes + d.image_bytes]);
    Some(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    // A minimal valid 40-byte BITMAPINFOHEADER for a 2x2 32bpp BI_RGB top-down image (no color table).
    fn bih_2x2_32() -> Vec<u8> {
        let mut v = Vec::new();
        v.extend_from_slice(&40u32.to_le_bytes()); // biSize
        v.extend_from_slice(&2i32.to_le_bytes()); // biWidth
        v.extend_from_slice(&2i32.to_le_bytes()); // biHeight
        v.extend_from_slice(&1u16.to_le_bytes()); // biPlanes
        v.extend_from_slice(&32u16.to_le_bytes()); // biBitCount
        v.extend_from_slice(&0u32.to_le_bytes()); // biCompression = BI_RGB
        v.extend_from_slice(&0u32.to_le_bytes()); // biSizeImage
        v.extend_from_slice(&0i32.to_le_bytes()); // xppm
        v.extend_from_slice(&0i32.to_le_bytes()); // yppm
        v.extend_from_slice(&0u32.to_le_bytes()); // biClrUsed
        v.extend_from_slice(&0u32.to_le_bytes()); // biClrImportant
        v
    }
    fn valid_dib() -> Vec<u8> {
        let mut v = bih_2x2_32();
        v.extend_from_slice(&[0u8; 2 * 2 * 4]); // 16 pixel bytes
        v
    }

    #[test]
    fn parses_a_valid_dib() {
        let d = parse_dib(&valid_dib()).expect("valid");
        assert_eq!(d.width, 2);
        assert_eq!(d.height, 2);
        assert_eq!(d.bpp, 32);
        assert_eq!(d.color_table_bytes, 0);
        assert_eq!(d.stride, 8); // 2px*4B = 8, already DWORD-aligned
        assert_eq!(d.image_bytes, 16);
        assert_eq!(d.header_bytes, 40);
    }

    #[test]
    fn computes_dword_aligned_stride() {
        // 3px wide, 24bpp → 9 bytes/row → padded to 12 (DWORD aligned)
        let mut h = bih_2x2_32();
        h[4..8].copy_from_slice(&3i32.to_le_bytes()); // width=3
        h[8..12].copy_from_slice(&1i32.to_le_bytes()); // height=1
        h[14..16].copy_from_slice(&24u16.to_le_bytes()); // 24bpp
        h.extend_from_slice(&[0u8; 12]); // one padded row
        let d = parse_dib(&h).expect("valid");
        assert_eq!(d.stride, 12);
        assert_eq!(d.image_bytes, 12);
    }

    #[test]
    fn rejects_truncated_header() {
        assert!(parse_dib(&[0u8; 10]).is_none());
        assert!(parse_dib(&[]).is_none());
    }

    #[test]
    fn rejects_absurd_geometry_without_overflow() {
        let mut h = bih_2x2_32();
        h[4..8].copy_from_slice(&i32::MAX.to_le_bytes()); // width = 2^31-1
        h[8..12].copy_from_slice(&i32::MAX.to_le_bytes()); // height = 2^31-1
        // stride*height overflows usize math if done naively; parse must reject, not panic/overflow.
        assert!(parse_dib(&h).is_none());
    }

    #[test]
    fn rejects_buffer_shorter_than_declared_image() {
        let mut v = bih_2x2_32(); // declares a 2x2x32 image (16 bytes) but we append only 4
        v.extend_from_slice(&[0u8; 4]);
        assert!(parse_dib(&v).is_none());
    }

    #[test]
    fn honors_high_bpp_optimization_palette() {
        // A 32bpp BI_RGB DIB may legally carry a biClrUsed optimization palette
        // (4 bytes/entry) between the header and the pixels. It must be counted,
        // or every consumer mislocates the pixel bits (silent corruption).
        let mut v = bih_2x2_32();
        v[8..12].copy_from_slice(&1i32.to_le_bytes()); // height=1 (keep it small)
        v[4..8].copy_from_slice(&1i32.to_le_bytes()); // width=1 → stride 4, image 4
        v[32..36].copy_from_slice(&2u32.to_le_bytes()); // biClrUsed = 2 → 8 palette bytes
        v.extend_from_slice(&[0xAA; 8]); // palette
        v.extend_from_slice(&[0xBB; 4]); // 1px * 4B pixels
        let d = parse_dib(&v).expect("valid high-bpp DIB with palette");
        assert_eq!(d.color_table_bytes, 8, "biClrUsed palette must be counted for bpp>8");
        assert_eq!(d.image_bytes, 4);
        // Pixels must be located after header + palette (40 + 8 = 48), not at 40.
        assert_eq!(
            &v[d.header_bytes + d.color_table_bytes..][..d.image_bytes],
            &[0xBB; 4],
            "pixel offset must skip the palette"
        );
    }

    #[test]
    fn rejects_oversize_color_table() {
        let mut h = bih_2x2_32();
        h[14..16].copy_from_slice(&8u16.to_le_bytes()); // 8bpp
        h[32..36].copy_from_slice(&u32::MAX.to_le_bytes()); // biClrUsed = 2^32-1 → absurd table
        assert!(parse_dib(&h).is_none());
    }

    #[test]
    fn dib_to_dibv5_and_back_preserve_geometry() {
        let dib = valid_dib();
        let v5 = dib_to_dibv5(&dib).expect("v5");
        // V5 header is 124 bytes; total grows by (124-40).
        assert_eq!(v5.len(), dib.len() + (124 - 40));
        let info = parse_dibv5(&v5).expect("parse v5");
        assert_eq!((info.width, info.height, info.bpp), (2, 2, 32));
        let back = dibv5_to_dib(&v5).expect("back");
        assert_eq!(parse_dib(&back).unwrap().width, 2);
    }
}
