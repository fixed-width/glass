//! Pure X-keysym → Windows virtual-key mapping for the NAMED keys glass_core::keys emits.
//! Printable ASCII (0x20..=0x7e) is resolved separately via VkKeyScanW (Windows-only), so it
//! is intentionally NOT handled here. VK numeric values are the stable Win32 ABI codes.

/// Map a named-key / F-key X keysym (as produced by `glass_core::keys`) to a Windows VK code.
/// Returns None for printable-ASCII keysyms (caller resolves via VkKeyScanW) and unknowns.
pub fn named_keysym_to_vk(keysym: u32) -> Option<u16> {
    Some(match keysym {
        0xff0d => 0x0D, // VK_RETURN
        0xff1b => 0x1B, // VK_ESCAPE
        0xff09 => 0x09, // VK_TAB
        0xff08 => 0x08, // VK_BACK
        0xffff => 0x2E, // VK_DELETE
        0xff52 => 0x26, // VK_UP
        0xff54 => 0x28, // VK_DOWN
        0xff51 => 0x25, // VK_LEFT
        0xff53 => 0x27, // VK_RIGHT
        0xff50 => 0x24, // VK_HOME
        0xff57 => 0x23, // VK_END
        0xffbe..=0xffc9 => (0x70 + (keysym - 0xffbe)) as u16, // F1..F12 -> VK_F1(0x70)..VK_F12(0x7B)
        _ => return None,
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn maps_named_keys() {
        assert_eq!(named_keysym_to_vk(0xff0d), Some(0x0D)); // Return
        assert_eq!(named_keysym_to_vk(0xff1b), Some(0x1B)); // Escape
        assert_eq!(named_keysym_to_vk(0xff51), Some(0x25)); // Left
    }
    #[test]
    fn maps_function_keys_without_off_by_one() {
        assert_eq!(named_keysym_to_vk(0xffbe), Some(0x70)); // F1
        assert_eq!(named_keysym_to_vk(0xffc1), Some(0x73)); // F4
        assert_eq!(named_keysym_to_vk(0xffc9), Some(0x7B)); // F12
    }
    #[test]
    fn returns_none_for_ascii_and_unknown() {
        assert_eq!(named_keysym_to_vk(0x61), None); // 'a' — ASCII, caller uses VkKeyScanW
        assert_eq!(named_keysym_to_vk(0x20), None); // space — ASCII arm
        assert_eq!(named_keysym_to_vk(0xdead), None); // unknown
    }
}
