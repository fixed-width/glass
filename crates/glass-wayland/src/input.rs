use glass_core::MouseButton;

/// Linux evdev button code for a glass mouse button (`linux/input-event-codes.h`).
pub fn evdev_button(button: MouseButton) -> u32 {
    match button {
        MouseButton::Left => 0x110,   // BTN_LEFT
        MouseButton::Right => 0x111,  // BTN_RIGHT
        MouseButton::Middle => 0x112, // BTN_MIDDLE
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn maps_buttons_to_evdev_codes() {
        assert_eq!(evdev_button(MouseButton::Left), 0x110);
        assert_eq!(evdev_button(MouseButton::Right), 0x111);
        assert_eq!(evdev_button(MouseButton::Middle), 0x112);
    }
}
