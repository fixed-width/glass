//! Dynamic XKB keymap generation for the virtual keyboard (the `wtype`
//! technique): each keysym gets its own keycode at level 1, written by numeric
//! value so we need no `libxkbcommon` and no keysym-name table.

/// Build an XKB keymap string placing each keysym at its own keycode, level 1.
/// Keycodes are XKB codes `9..` (evdev `N` -> XKB `N+8`); the Nth keysym
/// (1-based) sits at XKB keycode `8+N`, tapped by sending evdev keycode `N`.
pub fn build_keymap(keysyms: &[u32]) -> String {
    let codes: String = (1..=keysyms.len())
        .map(|n| format!("<K{n}> = {};", 8 + n))
        .collect();
    let syms: String = keysyms
        .iter()
        .enumerate()
        .map(|(i, ks)| format!("key <K{}> {{ [ 0x{:04x} ] }};", i + 1, ks))
        .collect();
    format!(
        "xkb_keymap {{\n\
         xkb_keycodes {{ minimum = 8; maximum = 255; {codes} }};\n\
         xkb_types {{ include \"complete\" }};\n\
         xkb_compatibility {{ include \"complete\" }};\n\
         xkb_symbols {{ {syms} }};\n\
         }};\n"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn places_keysyms_at_sequential_keycodes() {
        let km = build_keymap(&[0x61, 0xff0d]);
        assert!(km.contains("<K1> = 9;"), "{km}");
        assert!(km.contains("<K2> = 10;"), "{km}");
        assert!(km.contains("key <K1> { [ 0x0061 ] };"), "{km}");
        assert!(km.contains("key <K2> { [ 0xff0d ] };"), "{km}");
    }

    #[test]
    fn has_all_four_sections() {
        let km = build_keymap(&[0x61]);
        for s in [
            "xkb_keycodes",
            "xkb_types",
            "xkb_compatibility",
            "xkb_symbols",
        ] {
            assert!(km.contains(s), "missing {s} in {km}");
        }
    }

    #[test]
    fn empty_is_a_valid_keyless_keymap() {
        let km = build_keymap(&[]);
        assert!(km.contains("xkb_symbols {  }"), "{km}");
        assert!(!km.contains("key <K"), "{km}");
    }
}
