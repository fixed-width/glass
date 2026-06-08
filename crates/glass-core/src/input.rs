//! Pointer-input geometry helpers (platform-agnostic).

/// Points to warp the pointer through for a straight drag from `from` to `to`,
/// linearly interpolated at ~1px along the dominant axis (capped at
/// `MAX_STEPS`). Endpoints are exact (`path[0] == from`, `path[last] == to`); a
/// zero-length drag yields a single point.
pub fn drag_path(from: (i32, i32), to: (i32, i32)) -> Vec<(i32, i32)> {
    const MAX_STEPS: i64 = 512;
    let dx = (to.0 - from.0) as i64;
    let dy = (to.1 - from.1) as i64;
    let dist = dx.abs().max(dy.abs());
    if dist == 0 {
        return vec![from];
    }
    let steps = dist.min(MAX_STEPS);
    (0..=steps)
        .map(|i| {
            let x = from.0 as i64 + dx * i / steps;
            let y = from.1 as i64 + dy * i / steps;
            (x as i32, y as i32)
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::drag_path;

    #[test]
    fn horizontal_path_steps_one_pixel() {
        assert_eq!(
            drag_path((0, 0), (5, 0)),
            vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]
        );
    }

    #[test]
    fn diagonal_path_is_one_to_one() {
        assert_eq!(drag_path((0, 0), (3, 3)), vec![(0, 0), (1, 1), (2, 2), (3, 3)]);
    }

    #[test]
    fn backward_path_interpolates_with_exact_endpoints() {
        let p = drag_path((5, 5), (0, 5));
        assert_eq!(p.first(), Some(&(5, 5)));
        assert_eq!(p.last(), Some(&(0, 5)));
        assert_eq!(p.len(), 6);
    }

    #[test]
    fn zero_length_is_single_point() {
        assert_eq!(drag_path((2, 2), (2, 2)), vec![(2, 2)]);
    }

    #[test]
    fn long_drag_is_capped_with_exact_endpoints() {
        let p = drag_path((0, 0), (10_000, 0));
        assert_eq!(p.len(), 513); // MAX_STEPS = 512 -> 513 points
        assert_eq!(p.first(), Some(&(0, 0)));
        assert_eq!(p.last(), Some(&(10_000, 0)));
    }
}
