//! Pure clipboard text codecs (no Win32), unit-tested + Miri-checked on the Linux dev box.
//!
//! The `cfg(windows)` hook does the `unsafe` FFI — locking an `HGLOBAL` into a slice *bounded by
//! `GlobalSize`* — and defers the actual NUL-terminated parse/encode to these helpers. So the
//! UB-prone slicing/decoding lives in safe, tested code; only the FFI lock itself stays `unsafe`.
//!
//! Compiled for `windows` (the hook uses it) and for `test` (so the suite runs on Linux + Miri).

/// Decode a `CF_UNICODETEXT` block (the whole locked buffer, as UTF-16 code units) to a `String`,
/// stopping at the first NUL terminator — or consuming the whole buffer if it is unterminated.
/// Invalid UTF-16 (e.g. a lone surrogate) is replaced, never an error and never UB.
pub(crate) fn utf16_nul_to_string(units: &[u16]) -> String {
    let end = units.iter().position(|&c| c == 0).unwrap_or(units.len());
    String::from_utf16_lossy(&units[..end])
}

/// Decode a `CF_TEXT`/`CF_OEMTEXT` block (the whole locked buffer, as bytes) to a `String`, stopping
/// at the first NUL. v1 treats each byte as ANSI/Latin-1: every byte maps 1:1 to a `char`.
///
/// The v2 hook no longer down/up-converts single-byte text in Rust (it defers to the OS code page
/// via `WideCharToMultiByte`); kept for the tests that pin the pure decode behavior.
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn singlebyte_nul_to_string(bytes: &[u8]) -> String {
    let end = bytes.iter().position(|&b| b == 0).unwrap_or(bytes.len());
    bytes[..end].iter().map(|&b| b as char).collect()
}

/// Encode `text` as a NUL-terminated UTF-16 buffer for `CF_UNICODETEXT`.
pub(crate) fn string_to_utf16_nul(text: &str) -> Vec<u16> {
    text.encode_utf16().chain(std::iter::once(0)).collect()
}

/// Encode `text` as a NUL-terminated single-byte buffer for `CF_TEXT`/`CF_OEMTEXT`. v1 is
/// ASCII-only: any non-ASCII char becomes `'?'` (lossy). NB this is *lossier* than
/// [`singlebyte_nul_to_string`], which round-trips full Latin-1 — encode and decode are asymmetric
/// by design in v1 (we only ever populate the store from CF_UNICODETEXT in practice).
#[cfg_attr(not(test), allow(dead_code))]
pub(crate) fn string_to_singlebyte_nul(text: &str) -> Vec<u8> {
    text.chars()
        .map(|c| if (c as u32) < 0x80 { c as u8 } else { b'?' })
        .chain(std::iter::once(0))
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn utf16_decode_stops_at_nul_and_handles_edges() {
        // stops at the terminator, ignoring anything past it
        assert_eq!(
            utf16_nul_to_string(&[b'H' as u16, b'i' as u16, 0, b'X' as u16]),
            "Hi"
        );
        // unterminated → the whole buffer (no out-of-bounds walk past the end)
        assert_eq!(utf16_nul_to_string(&[b'H' as u16, b'i' as u16]), "Hi");
        // empty buffer and leading NUL → empty string
        assert_eq!(utf16_nul_to_string(&[]), "");
        assert_eq!(utf16_nul_to_string(&[0, b'H' as u16]), "");
        // non-ASCII BMP
        assert_eq!(utf16_nul_to_string(&[0x4e16, 0x754c, 0]), "世界");
        // lone high surrogate is invalid UTF-16 → replacement char (lossy, never UB)
        assert_eq!(utf16_nul_to_string(&[0xD800, 0]), "\u{FFFD}");
    }

    #[test]
    fn singlebyte_decode_is_latin1_to_first_nul() {
        assert_eq!(singlebyte_nul_to_string(b"AB\0C"), "AB");
        assert_eq!(singlebyte_nul_to_string(b"AB"), "AB"); // unterminated
        assert_eq!(singlebyte_nul_to_string(b""), "");
        assert_eq!(singlebyte_nul_to_string(&[0, b'A']), "");
        // high byte → Latin-1 char (0xE9 == 'é')
        assert_eq!(singlebyte_nul_to_string(&[0xE9, 0]), "é");
    }

    #[test]
    fn utf16_encode_appends_one_nul_and_round_trips() {
        assert_eq!(string_to_utf16_nul("Hi"), vec![b'H' as u16, b'i' as u16, 0]);
        assert_eq!(string_to_utf16_nul(""), vec![0]);
        // encode → decode round-trips (decode drops the terminator)
        for s in ["", "Hi", "世界", "tab\tnl\n"] {
            assert_eq!(utf16_nul_to_string(&string_to_utf16_nul(s)), s);
        }
    }

    #[test]
    fn singlebyte_encode_is_ascii_only_lossy() {
        assert_eq!(string_to_singlebyte_nul("AB"), vec![b'A', b'B', 0]);
        assert_eq!(string_to_singlebyte_nul(""), vec![0]);
        // non-ASCII → '?' — documents the encode/decode asymmetry
        assert_eq!(string_to_singlebyte_nul("é世"), vec![b'?', b'?', 0]);
    }
}
