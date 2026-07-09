#![forbid(unsafe_code)]

//! US-ASCII → USB HID Usage Table (Keyboard/Keypad page 0x07) mapping for the iOS
//! backend's synthetic keyboard. Unmapped characters return None so the caller can
//! surface a clear "unsupported character" error rather than silently drop it.
use glass_core::Modifier;

/// HID usage id + whether Shift must be held, for a printable US-ASCII char.
#[allow(dead_code)]
pub fn char_usage(c: char) -> Option<(u16, bool)> {
    Some(match c {
        'a'..='z' => (0x04 + (c as u16 - 'a' as u16), false),
        'A'..='Z' => (0x04 + (c as u16 - 'A' as u16), true),
        '1'..='9' => (0x1E + (c as u16 - '1' as u16), false),
        '0' => (0x27, false),
        ' ' => (0x2C, false),
        '-' => (0x2D, false),
        '=' => (0x2E, false),
        '[' => (0x2F, false),
        ']' => (0x30, false),
        '\\' => (0x31, false),
        ';' => (0x33, false),
        '\'' => (0x34, false),
        '`' => (0x35, false),
        ',' => (0x36, false),
        '.' => (0x37, false),
        '/' => (0x38, false),
        // Shifted symbols (US layout).
        '!' => (0x1E, true),
        '@' => (0x1F, true),
        '#' => (0x20, true),
        '$' => (0x21, true),
        '%' => (0x22, true),
        '^' => (0x23, true),
        '&' => (0x24, true),
        '*' => (0x25, true),
        '(' => (0x26, true),
        ')' => (0x27, true),
        '_' => (0x2D, true),
        '+' => (0x2E, true),
        '{' => (0x2F, true),
        '}' => (0x30, true),
        '|' => (0x31, true),
        ':' => (0x33, true),
        '"' => (0x34, true),
        '~' => (0x35, true),
        '<' => (0x36, true),
        '>' => (0x37, true),
        '?' => (0x38, true),
        _ => return None,
    })
}

/// HID usage for a named key used in chords.
#[allow(dead_code)]
pub fn keyname_usage(name: &str) -> Option<u16> {
    let named = match name {
        "Return" | "Enter" => 0x28,
        "Escape" | "Esc" => 0x29,
        "BackSpace" | "Backspace" => 0x2A,
        "Tab" => 0x2B,
        "space" | "Space" => 0x2C,
        "Delete" | "Del" => 0x4C, // forward delete
        "Right" => 0x4F,
        "Left" => 0x50,
        "Down" => 0x51,
        "Up" => 0x52,
        "Home" => 0x4A,
        "End" => 0x4D,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    // F1..F12 -> 0x3A..0x45
    if let Some(n) = name.strip_prefix('F').and_then(|n| n.parse::<u16>().ok()) {
        if (1..=12).contains(&n) {
            return Some(0x3A + (n - 1));
        }
    }
    // A single printable char, case-sensitive (usage only; caller adds Shift from the flag).
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => char_usage(c).map(|(u, _)| u),
        _ => None,
    }
}

/// HID usage for a modifier key (left-hand variants).
#[allow(dead_code)]
pub fn modifier_usage(m: Modifier) -> u16 {
    match m {
        Modifier::Control => 0xE0,
        Modifier::Shift => 0xE1,
        Modifier::Alt => 0xE2,
        Modifier::Super => 0xE3,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use glass_core::Modifier;

    #[test]
    fn letters_and_shift() {
        assert_eq!(char_usage('a'), Some((0x04, false)));
        assert_eq!(char_usage('z'), Some((0x1D, false)));
        assert_eq!(char_usage('A'), Some((0x04, true)));
        assert_eq!(char_usage('Z'), Some((0x1D, true)));
    }

    #[test]
    fn digits_space_and_symbols() {
        assert_eq!(char_usage('1'), Some((0x1E, false)));
        assert_eq!(char_usage('0'), Some((0x27, false)));
        assert_eq!(char_usage(' '), Some((0x2C, false)));
        assert_eq!(char_usage('!'), Some((0x1E, true))); // shift+1
        assert_eq!(char_usage('.'), Some((0x37, false)));
        assert_eq!(char_usage('?'), Some((0x38, true))); // shift+/
    }

    #[test]
    fn unmapped_char_is_none() {
        assert_eq!(char_usage('€'), None);
        assert_eq!(char_usage('\n'), None);
    }

    #[test]
    fn named_and_modifiers() {
        assert_eq!(keyname_usage("Return"), Some(0x28));
        assert_eq!(keyname_usage("Enter"), Some(0x28));
        assert_eq!(keyname_usage("Escape"), Some(0x29));
        assert_eq!(keyname_usage("Tab"), Some(0x2B));
        assert_eq!(keyname_usage("Up"), Some(0x52));
        assert_eq!(keyname_usage("F1"), Some(0x3A));
        assert_eq!(keyname_usage("nope"), None);
        assert_eq!(modifier_usage(Modifier::Shift), 0xE1);
        assert_eq!(modifier_usage(Modifier::Super), 0xE3);
    }
}
