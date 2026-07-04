use crate::{GlassError, Result};

/// Modifier names glass recognizes in chords (shared X11/XKB keysym namespace).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Modifier {
    Shift,
    Control,
    Alt,
    Super,
}

impl Modifier {
    /// Parse a modifier name (case-insensitive), matching the tokens chords accept.
    pub fn from_name(name: &str) -> Option<Modifier> {
        match name.to_ascii_lowercase().as_str() {
            "shift" => Some(Modifier::Shift),
            "ctrl" | "control" => Some(Modifier::Control),
            "alt" => Some(Modifier::Alt),
            "super" | "meta" | "win" | "cmd" | "command" => Some(Modifier::Super),
            _ => None,
        }
    }
}

/// Keysym for a single printable ASCII char (the X keysym equals the codepoint
/// for 0x20..=0x7e). Returns `None` for non-ASCII or control chars.
pub fn keysym_for_char(c: char) -> Option<u32> {
    let u = c as u32;
    if (0x20..=0x7e).contains(&u) {
        Some(u)
    } else {
        None
    }
}

/// Keysym for a named key used in chords (e.g. "Return", "Escape", "F4", "a").
/// Single printable chars defer to `keysym_for_char`.
pub fn keysym_for_keyname(name: &str) -> Option<u32> {
    let named = match name {
        "Return" | "Enter" => 0xff0d,
        "Escape" | "Esc" => 0xff1b,
        "Tab" => 0xff09,
        "BackSpace" | "Backspace" => 0xff08,
        "Delete" | "Del" => 0xffff,
        "space" | "Space" => 0x0020,
        "Up" => 0xff52,
        "Down" => 0xff54,
        "Left" => 0xff51,
        "Right" => 0xff53,
        "Home" => 0xff50,
        "End" => 0xff57,
        _ => 0,
    };
    if named != 0 {
        return Some(named);
    }
    // Function keys F1..F12 -> 0xffbe..0xffc9
    if let Some(num) = name.strip_prefix('F').and_then(|n| n.parse::<u32>().ok()) {
        if (1..=12).contains(&num) {
            return Some(0xffbd + num);
        }
    }
    // Single character (case-sensitive; uppercase needs Shift, handled by caller).
    let mut chars = name.chars();
    match (chars.next(), chars.next()) {
        (Some(c), None) => keysym_for_char(c),
        _ => None,
    }
}

/// Parse a chord like "ctrl+s", "alt+F4", "shift+Tab", "Escape" into its
/// modifiers and the final key's keysym.
pub fn parse_chord(chord: &str) -> Result<(Vec<Modifier>, u32)> {
    let parts: Vec<&str> = chord
        .split('+')
        .map(|p| p.trim())
        .filter(|p| !p.is_empty())
        .collect();
    if parts.is_empty() {
        return Err(GlassError::InvalidKey(chord.to_string()));
    }
    let (key, mods) = parts.split_last().unwrap();
    let mut modifiers = Vec::new();
    for m in mods {
        let modifier = Modifier::from_name(m).ok_or_else(|| {
            GlassError::InvalidKey(format!(
                "unknown modifier '{m}' in '{chord}' (use ctrl/shift/alt/super/cmd)"
            ))
        })?;
        modifiers.push(modifier);
    }
    let keysym = keysym_for_keyname(key)
        .ok_or_else(|| GlassError::InvalidKey(format!("unknown key '{key}' in '{chord}'")))?;
    Ok((modifiers, keysym))
}

/// Keysym to type `c` as text: the legacy keysym for ASCII, else the Unicode
/// keysym (`0x01000000 + codepoint`), which XKB renders as that character.
pub fn keysym_for_text(c: char) -> u32 {
    keysym_for_char(c).unwrap_or(0x0100_0000 + c as u32)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn text_keysyms() {
        assert_eq!(keysym_for_text('a'), 0x61);
        assert_eq!(keysym_for_text(' '), 0x20);
        assert_eq!(keysym_for_text('€'), 0x0100_20ac); // U+20AC -> Unicode keysym
    }

    #[test]
    fn char_keysyms() {
        assert_eq!(keysym_for_char('a'), Some(0x61));
        assert_eq!(keysym_for_char(' '), Some(0x20));
        assert_eq!(keysym_for_char('~'), Some(0x7e));
        assert_eq!(keysym_for_char('\n'), None);
    }

    #[test]
    fn named_keysyms() {
        assert_eq!(keysym_for_keyname("Return"), Some(0xff0d));
        assert_eq!(keysym_for_keyname("Escape"), Some(0xff1b));
        assert_eq!(keysym_for_keyname("F4"), Some(0xffc1));
        assert_eq!(keysym_for_keyname("a"), Some(0x61));
        assert_eq!(keysym_for_keyname("nope"), None);
    }

    #[test]
    fn parses_chords() {
        assert_eq!(
            parse_chord("ctrl+s").unwrap(),
            (vec![Modifier::Control], 0x73)
        );
        assert_eq!(
            parse_chord("alt+F4").unwrap(),
            (vec![Modifier::Alt], 0xffc1)
        );
        assert_eq!(
            parse_chord("ctrl+shift+a").unwrap(),
            (vec![Modifier::Control, Modifier::Shift], 0x61)
        );
        assert_eq!(parse_chord("Escape").unwrap(), (vec![], 0xff1b));
    }

    #[test]
    fn rejects_bad_chords() {
        assert!(matches!(
            parse_chord("hyper+x").unwrap_err(),
            GlassError::InvalidKey(_)
        ));
        assert!(matches!(
            parse_chord("ctrl+nope").unwrap_err(),
            GlassError::InvalidKey(_)
        ));
        assert!(matches!(
            parse_chord("").unwrap_err(),
            GlassError::InvalidKey(_)
        ));
    }

    #[test]
    fn modifier_from_name() {
        assert_eq!(Modifier::from_name("ctrl"), Some(Modifier::Control));
        assert_eq!(Modifier::from_name("Control"), Some(Modifier::Control));
        assert_eq!(Modifier::from_name("shift"), Some(Modifier::Shift));
        assert_eq!(Modifier::from_name("super"), Some(Modifier::Super));
        assert_eq!(Modifier::from_name("win"), Some(Modifier::Super));
        assert_eq!(Modifier::from_name("hyper"), None);
    }

    #[test]
    fn modifier_cmd_is_super() {
        // ⌘ is spelled `cmd`/`command` in the macOS idiom; both alias to Super (which the
        // macOS backend renders as the Command flag), so cmd-chords parse like super-chords.
        assert_eq!(Modifier::from_name("cmd"), Some(Modifier::Super));
        assert_eq!(Modifier::from_name("command"), Some(Modifier::Super));
        assert_eq!(Modifier::from_name("Cmd"), Some(Modifier::Super)); // case-insensitive
        assert_eq!(
            parse_chord("cmd+a").unwrap(),
            parse_chord("super+a").unwrap()
        );
    }
}
