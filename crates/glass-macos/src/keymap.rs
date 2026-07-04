//! Pure US-layout ASCII → (virtual keycode, needs-shift). Cross-platform so it is
//! unit-tested on the Linux dev box; Plan 3's CGEvent input casts `u16 as CGKeyCode`.
//! Keycodes are the documented Carbon `kVK_ANSI_*` values (validated in inject_input.swift).

/// Map an ASCII character to its US-layout virtual keycode and whether Shift is held.
/// Returns `None` for characters with no single-key US mapping (caller skips/handles).
pub fn key_for(ch: char) -> Option<(u16, bool)> {
    // Base (unshifted) keys: lowercase letters, digits, and the unshifted punctuation.
    const BASE: &[(char, u16)] = &[
        ('a', 0),
        ('s', 1),
        ('d', 2),
        ('f', 3),
        ('h', 4),
        ('g', 5),
        ('z', 6),
        ('x', 7),
        ('c', 8),
        ('v', 9),
        ('b', 11),
        ('q', 12),
        ('w', 13),
        ('e', 14),
        ('r', 15),
        ('y', 16),
        ('t', 17),
        ('1', 18),
        ('2', 19),
        ('3', 20),
        ('4', 21),
        ('6', 22),
        ('5', 23),
        ('=', 24),
        ('9', 25),
        ('7', 26),
        ('-', 27),
        ('8', 28),
        ('0', 29),
        (']', 30),
        ('o', 31),
        ('u', 32),
        ('[', 33),
        ('i', 34),
        ('p', 35),
        ('l', 37),
        ('j', 38),
        ('k', 40),
        ('n', 45),
        ('m', 46),
        ('.', 47),
        (' ', 49),
        ('/', 44),
        (';', 41),
        ('\'', 39),
        (',', 43),
        ('`', 50),
        ('\\', 42),
    ];
    if let Some(&(_, code)) = BASE.iter().find(|&&(c, _)| c == ch) {
        return Some((code, false));
    }
    // Uppercase letters → same key with Shift.
    if ch.is_ascii_uppercase() {
        let lower = ch.to_ascii_lowercase();
        if let Some(&(_, code)) = BASE.iter().find(|&&(c, _)| c == lower) {
            return Some((code, true));
        }
    }
    // Shifted symbols → the base key for the symbol on that physical key, with Shift.
    const SHIFTED: &[(char, char)] = &[
        ('!', '1'),
        ('@', '2'),
        ('#', '3'),
        ('$', '4'),
        ('%', '5'),
        ('^', '6'),
        ('&', '7'),
        ('*', '8'),
        ('(', '9'),
        (')', '0'),
        ('_', '-'),
        ('+', '='),
        ('{', '['),
        ('}', ']'),
        ('|', '\\'),
        (':', ';'),
        ('"', '\''),
        ('<', ','),
        ('>', '.'),
        ('?', '/'),
        ('~', '`'),
    ];
    if let Some(&(_, base)) = SHIFTED.iter().find(|&&(c, _)| c == ch) {
        if let Some(&(_, code)) = BASE.iter().find(|&&(c, _)| c == base) {
            return Some((code, true));
        }
    }
    None
}

/// Map a named key (as used in a chord's final token, e.g. `"Return"`, `"F4"`) to its
/// US-layout virtual keycode. Matched case-insensitively against the documented Carbon
/// `kVK_*` values (validated in `inject_input.swift`'s reference). Falls back to
/// [`key_for`] for a single ASCII char (dropping the shift bit — callers that need it use
/// `key_for` directly), so a chord's key token doesn't need two different lookups
/// depending on whether it's named or literal. Returns `None` for anything else.
pub fn keycode_for_keyname(name: &str) -> Option<u16> {
    const NAMED: &[(&str, u16)] = &[
        ("return", 36),
        ("tab", 48),
        ("space", 49),
        ("delete", 51),
        ("escape", 53),
        ("left", 123),
        ("right", 124),
        ("down", 125),
        ("up", 126),
        ("home", 115),
        ("end", 119),
        ("pageup", 116),
        ("pagedown", 121),
        ("forwarddelete", 117),
        ("f1", 122),
        ("f2", 120),
        ("f3", 99),
        ("f4", 118),
        ("f5", 96),
        ("f6", 97),
        ("f7", 98),
        ("f8", 100),
        ("f9", 101),
        ("f10", 109),
        ("f11", 103),
        ("f12", 111),
    ];
    let lower = name.to_ascii_lowercase();
    if let Some(&(_, code)) = NAMED.iter().find(|&&(n, _)| n == lower) {
        return Some(code);
    }
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => key_for(c).map(|(code, _)| code),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::{key_for, keycode_for_keyname};

    #[test]
    fn lowercase_letters_unshifted() {
        assert_eq!(key_for('a'), Some((0, false)));
        assert_eq!(key_for('m'), Some((46, false)));
    }

    #[test]
    fn uppercase_letters_shifted_same_key() {
        assert_eq!(key_for('A'), Some((0, true)));
        assert_eq!(key_for('Z'), Some((6, true)));
    }

    #[test]
    fn digits_unshifted_and_their_symbols_shifted() {
        assert_eq!(key_for('1'), Some((18, false)));
        assert_eq!(key_for('!'), Some((18, true))); // same physical key as '1', shifted
        assert_eq!(key_for(')'), Some((29, true))); // same key as '0'
    }

    #[test]
    fn space_and_unmapped() {
        assert_eq!(key_for(' '), Some((49, false)));
        assert_eq!(key_for('€'), None);
    }

    #[test]
    fn named_keys_map_to_their_kvk_codes() {
        assert_eq!(keycode_for_keyname("Return"), Some(36));
        assert_eq!(keycode_for_keyname("Escape"), Some(53));
        assert_eq!(keycode_for_keyname("Tab"), Some(48));
        assert_eq!(keycode_for_keyname("Delete"), Some(51));
    }

    #[test]
    fn named_keys_are_case_insensitive() {
        assert_eq!(keycode_for_keyname("return"), Some(36));
        assert_eq!(keycode_for_keyname("ESCAPE"), Some(53));
        assert_eq!(keycode_for_keyname("EsCaPe"), Some(53));
    }

    #[test]
    fn arrow_keys() {
        assert_eq!(keycode_for_keyname("Left"), Some(123));
        assert_eq!(keycode_for_keyname("Right"), Some(124));
        assert_eq!(keycode_for_keyname("Down"), Some(125));
        assert_eq!(keycode_for_keyname("Up"), Some(126));
    }

    #[test]
    fn function_keys() {
        assert_eq!(keycode_for_keyname("F1"), Some(122));
        assert_eq!(keycode_for_keyname("F5"), Some(96));
        assert_eq!(keycode_for_keyname("F12"), Some(111));
    }

    #[test]
    fn single_char_falls_back_to_key_for() {
        assert_eq!(keycode_for_keyname("a"), Some(0));
        assert_eq!(keycode_for_keyname("A"), Some(0)); // shift bit dropped, same physical key
    }

    #[test]
    fn unknown_name_is_none() {
        assert_eq!(keycode_for_keyname("nope"), None);
        assert_eq!(keycode_for_keyname("F13"), None);
        assert_eq!(keycode_for_keyname(""), None);
    }
}
