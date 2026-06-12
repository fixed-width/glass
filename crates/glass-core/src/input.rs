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

/// Resample a straight drag from `from` to `to` into a bounded set of waypoints
/// spread over `duration_ms`, returning the waypoints (first == `from`, last ==
/// `to`, monotonic) and the delay to sleep between consecutive waypoints. Pacing
/// the motion over wall-clock time lets a frame-based client (egui/winit) sample
/// the path across multiple repaint frames instead of coalescing it into one
/// (which loses the path, and under continuous repaint drops the drag entirely).
pub fn drag_schedule(
    from: (i32, i32),
    to: (i32, i32),
    duration_ms: u64,
) -> (Vec<(i32, i32)>, std::time::Duration) {
    const STEP_MS: u64 = 16; // ~one 60Hz frame
    const MIN_WAYPOINTS: usize = 10;
    let full = drag_path(from, to);
    if full.len() <= 1 {
        return (full, std::time::Duration::ZERO);
    }
    // Target ~one waypoint per frame of the requested duration, but never fewer
    // than MIN_WAYPOINTS, and never more points than the path actually has.
    let target = (duration_ms / STEP_MS).max(1) as usize;
    let n = target.max(MIN_WAYPOINTS).min(full.len());
    let last = full.len() - 1;
    let waypoints: Vec<(i32, i32)> = (0..n).map(|i| full[i * last / (n - 1)]).collect();
    let step = std::time::Duration::from_millis(duration_ms / n as u64);
    (waypoints, step)
}

#[cfg(test)]
mod tests {
    use super::drag_path;
    use super::drag_schedule;
    use std::time::Duration;

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

    #[test]
    fn schedule_endpoints_exact_and_bounded() {
        // Long drag, 200ms: ~200/16 = 12 waypoints, endpoints exact.
        let (wp, step) = drag_schedule((0, 0), (1000, 0), 200);
        assert_eq!(wp.first(), Some(&(0, 0)));
        assert_eq!(wp.last(), Some(&(1000, 0)));
        assert_eq!(wp.len(), 12, "200ms / 16ms ≈ 12 waypoints");
        assert_eq!(step, Duration::from_millis(200 / 12));
        assert!(wp.windows(2).all(|w| w[1].0 >= w[0].0)); // monotonic x
    }

    #[test]
    fn schedule_min_waypoints_floor() {
        // Tiny duration still yields at least MIN_WAYPOINTS (10) for a long path.
        let (wp, _) = drag_schedule((0, 0), (1000, 0), 1);
        assert_eq!(wp.len(), 10);
    }

    #[test]
    fn schedule_short_drag_uses_all_points() {
        // 5px path has 6 points (< MIN_WAYPOINTS) -> use all 6, endpoints exact.
        let (wp, _) = drag_schedule((0, 0), (5, 0), 200);
        assert_eq!(wp, vec![(0, 0), (1, 0), (2, 0), (3, 0), (4, 0), (5, 0)]);
    }

    #[test]
    fn schedule_zero_length_single_point_no_delay() {
        let (wp, step) = drag_schedule((2, 2), (2, 2), 200);
        assert_eq!(wp, vec![(2, 2)]);
        assert_eq!(step, Duration::ZERO);
    }
}
