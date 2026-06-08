/// Translate a window-relative point to absolute root coordinates given the
/// window's origin (top-left) in root coordinates. XTEST motion uses root
/// coordinates, so callers add the window origin (from `translate_coordinates`).
pub fn window_to_root(origin_x: i32, origin_y: i32, x: i32, y: i32) -> (i16, i16) {
    ((origin_x + x) as i16, (origin_y + y) as i16)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn adds_origin() {
        assert_eq!(window_to_root(100, 50, 5, 7), (105, 57));
    }

    #[test]
    fn handles_zero_origin() {
        assert_eq!(window_to_root(0, 0, 12, 34), (12, 34));
    }
}
