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

/// Max keycodes one keymap can hold: XKB keycodes run `9..=255` (evdev `1..=247`),
/// so a single keymap places at most 247 distinct keysyms.
pub const MAX_KEYCODES: usize = 247;

/// One keymap-stable slice of a typed string: the distinct keysyms it needs (each
/// placed at its own keycode by [`build_keymap`]) plus the evdev keycode to tap for
/// every character of the slice, in order.
#[derive(Debug, PartialEq, Eq)]
pub struct TypeChunk {
    /// Distinct keysyms, first-seen order; the Nth (1-based) sits at evdev keycode N.
    pub keysyms: Vec<u32>,
    /// Evdev keycode to tap for each character of the slice, in order.
    pub taps: Vec<u32>,
}

/// Plan typing `text` so the keymap stays fixed while a slice's keys are delivered.
///
/// Each character's keysym is placed once, at its own keycode; a repeated keysym
/// reuses its keycode. Because the keymap is uploaded once per chunk and never
/// swapped between that chunk's key events, `keycode -> keysym` is a fixed function
/// for the chunk's lifetime. A client that resolves keysyms lazily (e.g. an X11 app
/// under Xwayland calling `get_keyboard_mapping` per press) therefore can't read a
/// neighbouring character by racing a mid-string keymap change — the flake that the
/// per-character-keymap approach (a fresh keymap on the *same* keycode for every
/// char) exhibited under load.
///
/// A string with more than [`MAX_KEYCODES`] distinct keysyms is split into
/// consecutive chunks, each within the keycode budget.
pub fn plan_type(text: &str) -> Vec<TypeChunk> {
    let mut chunks = Vec::new();
    let mut keysyms: Vec<u32> = Vec::new();
    let mut taps: Vec<u32> = Vec::new();
    for c in text.chars() {
        let ks = glass_core::keys::keysym_for_text(c);
        let keycode = match keysyms.iter().position(|&k| k == ks) {
            Some(i) => i + 1,
            None => {
                // A new keysym needs a fresh keycode; if the chunk is full, seal it
                // and start the next one so keycodes never exceed the keymap budget.
                if keysyms.len() == MAX_KEYCODES {
                    chunks.push(TypeChunk {
                        keysyms: std::mem::take(&mut keysyms),
                        taps: std::mem::take(&mut taps),
                    });
                }
                keysyms.push(ks);
                keysyms.len()
            }
        };
        taps.push(keycode as u32);
    }
    if !taps.is_empty() {
        chunks.push(TypeChunk { keysyms, taps });
    }
    chunks
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Decode a plan back into the keysym-per-character sequence it will deliver:
    /// for each tap, the keysym is its chunk's keysym at that keycode.
    fn decode(chunks: &[TypeChunk]) -> Vec<u32> {
        chunks
            .iter()
            .flat_map(|c| c.taps.iter().map(move |&t| c.keysyms[(t - 1) as usize]))
            .collect()
    }

    #[test]
    fn repeated_chars_reuse_one_keycode() {
        let plan = plan_type("aaa");
        assert_eq!(
            plan,
            vec![TypeChunk {
                keysyms: vec![0x61],
                taps: vec![1, 1, 1],
            }]
        );
    }

    #[test]
    fn adjacent_distinct_chars_get_distinct_keycodes() {
        // The core of the fix: "brown"'s r and o must tap different keycodes, so a
        // lazily-resolving client can never read one as the other.
        assert_eq!(plan_type("abc")[0].taps, vec![1, 2, 3]);
    }

    #[test]
    fn a_reappearing_keysym_reuses_its_keycode() {
        assert_eq!(plan_type("aba")[0].taps, vec![1, 2, 1]);
    }

    #[test]
    fn plan_reconstructs_the_original_keysym_sequence() {
        let text = "the quick brown fox";
        let expected: Vec<u32> = text
            .chars()
            .map(glass_core::keys::keysym_for_text)
            .collect();
        assert_eq!(decode(&plan_type(text)), expected);
    }

    #[test]
    fn a_realistic_string_fits_in_one_chunk() {
        assert_eq!(plan_type("the quick brown fox").len(), 1);
    }

    #[test]
    fn empty_text_plans_nothing() {
        assert!(plan_type("").is_empty());
    }

    #[test]
    fn chunking_keeps_each_chunk_within_the_keycode_limit() {
        let text: String = (0u32..250)
            .map(|i| char::from_u32(0x100 + i).unwrap())
            .collect();
        let plan = plan_type(&text);
        assert!(plan.len() >= 2, "250 distinct keysyms must span >1 chunk");
        assert!(plan.iter().all(|c| c.keysyms.len() <= MAX_KEYCODES));
    }

    #[test]
    fn chunking_still_reconstructs_the_full_keysym_sequence() {
        let text: String = (0u32..250)
            .map(|i| char::from_u32(0x100 + i).unwrap())
            .collect();
        let expected: Vec<u32> = text
            .chars()
            .map(glass_core::keys::keysym_for_text)
            .collect();
        assert_eq!(decode(&plan_type(&text)), expected);
    }

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
