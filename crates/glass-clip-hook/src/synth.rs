//! Pure synthesis *decisions* (no Win32, Miri-checked): which formats are available given the
//! stored set (canonical + synthesizable, canonical-first), and which stored format synthesizes a
//! requested one. The actual byte conversions live in the `cfg(windows)` hook (code page via
//! `WideCharToMultiByte`; `CF_BITMAP` via GDI; `CF_DIBV5` via `dib.rs`).

use crate::proto::FormatKey::{self, Standard};

const CF_TEXT: u32 = 1;
const CF_BITMAP: u32 = 2;
const CF_OEMTEXT: u32 = 7;
const CF_DIB: u32 = 8;
const CF_UNICODETEXT: u32 = 13;
const CF_LOCALE: u32 = 16;
const CF_DIBV5: u32 = 17;

/// Formats this canonical id can synthesize (excluding itself).
fn derivatives(id: u32) -> &'static [u32] {
    match id {
        CF_UNICODETEXT => &[CF_TEXT, CF_OEMTEXT, CF_LOCALE],
        CF_DIB => &[CF_BITMAP, CF_DIBV5],
        _ => &[],
    }
}

/// The full available-format set for `stored`: each stored key (in order), then any derivatives not
/// already stored. Mirrors the OS enumeration order (canonical-first, then synthesized).
pub(crate) fn available(stored: &[FormatKey]) -> Vec<FormatKey> {
    let mut out: Vec<FormatKey> = stored.to_vec();
    for k in stored {
        if let Standard(id) = k {
            for &d in derivatives(*id) {
                if !out.contains(&Standard(d)) {
                    out.push(Standard(d));
                }
            }
        }
    }
    out
}

/// The formats the OLE proxy advertises: `available` minus GDI `CF_BITMAP` (the byte-serving proxy
/// only produces HGLOBAL media; `CF_BITMAP` is a GDI handle served only by the user32 path).
pub(crate) fn serve_keys(stored: &[FormatKey]) -> Vec<FormatKey> {
    available(stored).into_iter().filter(|k| *k != Standard(CF_BITMAP)).collect()
}

/// If `requested` is a synthesizable derivative, the stored canonical key it derives from.
pub(crate) fn canonical_for(requested: &FormatKey) -> Option<FormatKey> {
    let Standard(id) = requested else { return None };
    let canon = match *id {
        CF_TEXT | CF_OEMTEXT | CF_LOCALE => CF_UNICODETEXT,
        CF_BITMAP | CF_DIBV5 => CF_DIB,
        _ => return None,
    };
    Some(Standard(canon))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::FormatKey::{Named, Standard};

    #[test]
    fn unicodetext_yields_text_triad_and_locale() {
        let avail = available(&[Standard(13)]); // CF_UNICODETEXT
        // canonical first, then synthesized, in a stable order
        assert_eq!(avail[0], Standard(13));
        for k in [Standard(1), Standard(7), Standard(16)] {
            assert!(avail.contains(&k), "missing {k:?}");
        }
        assert_eq!(canonical_for(&Standard(1)), Some(Standard(13))); // CF_TEXT  ← CF_UNICODETEXT
        assert_eq!(canonical_for(&Standard(7)), Some(Standard(13))); // CF_OEMTEXT
    }

    #[test]
    fn dib_yields_bitmap_and_dibv5() {
        let avail = available(&[Standard(8)]); // CF_DIB
        assert_eq!(avail[0], Standard(8));
        for k in [Standard(2), Standard(17)] {
            assert!(avail.contains(&k)); // CF_BITMAP, CF_DIBV5
        }
        assert_eq!(canonical_for(&Standard(2)), Some(Standard(8))); // CF_BITMAP ← CF_DIB
        assert_eq!(canonical_for(&Standard(17)), Some(Standard(8))); // CF_DIBV5  ← CF_DIB
    }

    #[test]
    fn named_formats_pass_through_no_synthesis() {
        let avail = available(&[Named("HTML Format".into())]);
        assert_eq!(avail, vec![Named("HTML Format".into())]);
        assert_eq!(canonical_for(&Named("HTML Format".into())), None);
    }

    #[test]
    fn already_stored_is_not_duplicated() {
        let avail = available(&[Standard(13), Standard(1)]); // both stored
        assert_eq!(avail.iter().filter(|k| **k == Standard(1)).count(), 1);
    }

    #[test]
    fn serve_keys_excludes_cf_bitmap_but_keeps_byte_derivatives() {
        // CF_DIB stored → serve DIB + DIBV5 (byte) but NOT CF_BITMAP (GDI handle).
        let keys = serve_keys(&[Standard(8)]);
        assert!(keys.contains(&Standard(8))); // CF_DIB
        assert!(keys.contains(&Standard(17))); // CF_DIBV5 (byte rewrite)
        assert!(!keys.contains(&Standard(2)), "CF_BITMAP must be excluded: {keys:?}");
        // CF_UNICODETEXT stored → text triad (all byte) kept.
        let t = serve_keys(&[Standard(13)]);
        for k in [Standard(13), Standard(1), Standard(7), Standard(16)] {
            assert!(t.contains(&k), "missing {k:?}");
        }
        // Named formats pass through.
        assert_eq!(serve_keys(&[Named("HTML Format".into())]), vec![Named("HTML Format".into())]);
    }
}
