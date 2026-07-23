//! `Glass` synchronization: wait-for-stable/element/region/log, scroll-to-element,
//! and the wait/scroll parameter and outcome types.
use super::*;

/// Parameters for [`Glass::wait_stable`].
#[derive(Clone, Debug)]
pub struct WaitStableParams {
    pub interval_ms: u64,
    pub settle_frames: u32,
    pub tolerance: u8,
    pub timeout_ms: u64,
    /// When set, the settle decision compares only this sub-rectangle of each
    /// frame; the returned frame is still the full window.
    pub stability_region: Option<Region>,
    /// Window-relative sub-rectangles excluded from the settle comparison — pixels
    /// there never count as changed, so a perpetually animating region (a blinking
    /// caret, a clock) cannot prevent the stream from settling. When
    /// `stability_region` is set, each rect is intersected with it and translated
    /// into region-local coordinates, so `ignore` is always window-relative
    /// regardless of scoping. With `window` set, "window-relative" means relative
    /// to the watched window, not the active one.
    pub ignore: Vec<Region>,
    /// When set, watch this window's own region instead of the active window's —
    /// without changing which window is active.
    pub window: Option<WindowId>,
}

/// Outcome of a wait-until-stable: the final frame and whether it settled
/// before the timeout.
#[derive(Clone, Debug)]
pub struct WaitStableOutcome {
    pub frame: Frame,
    pub settled: bool,
    /// Whether any frame-to-frame change was seen while watching. `settled:true` with
    /// `saw_motion:false` over a short `observed_ms` is a *brief* quiet window — a slow
    /// animation can still hide under it, so use `wait_for_region {until:"changes"}` to
    /// positively assert motion. `settled:true` with `saw_motion:true` means it was moving
    /// and then quieted. Motion confined to an `ignore` rect does not count — it is masked
    /// out of the comparison, so it can never set this flag.
    pub saw_motion: bool,
    /// How long (ms) frames were observed before settling or timing out.
    pub observed_ms: u64,
    /// Pixels an `ignore` mask excluded from every settle comparison (counting
    /// overlaps once); 0 when no `ignore` rects were in effect. `settled:true`
    /// with `ignored_pixels` equal to the compared area means the mask covered
    /// everything, so nothing was actually compared — the same signal `glass_diff`
    /// surfaces.
    pub ignored_pixels: u64,
}

/// Parameters for [`Glass::wait_for_element`].
#[derive(Clone, Debug)]
pub struct WaitElementParams {
    pub name: Option<String>,
    pub role: Option<AxRole>,
    pub value_contains: Option<String>,
    pub condition: ElementCondition,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Outcome of [`Glass::wait_for_element`].
#[derive(Clone, Debug)]
pub struct WaitElementOutcome {
    pub matched: bool,
    /// The matched element (absent on timeout, and for a satisfied `disappears`).
    pub element: Option<ElementInfo>,
    /// Wall-clock milliseconds elapsed when the wait returned.
    pub elapsed_ms: u64,
}

/// Wheel notches per scroll step; chosen so a step realizes at most a few rows
/// (won't skip a virtualized row's realized band). Overridable per call.
pub const SCROLL_TO_DEFAULT_STEP: u32 = 3;
/// Overall wall-clock bound for a `scroll_to_element` sweep.
pub const SCROLL_TO_DEFAULT_TIMEOUT_MS: u64 = 20_000;
/// Hard cap on scroll steps issued across a full bidirectional sweep, independent
/// of `timeout_ms` — bounds the sweep even if the caller passes an enormous timeout.
const SCROLL_TO_MAX_STEPS: u32 = 500;
/// Milliseconds to let scrolled rows realize in the a11y tree before re-reading.
/// 250ms is the validated floor on the headless a11y bus: the tree is read once
/// per step (for both the match and the end-of-scroll comparison), so a settle
/// shorter than the toolkit's realize latency would read an unchanged tree and
/// misfire as premature saturation.
const SCROLL_TO_SETTLE_MS: u64 = 250;

/// A scroll sweep direction. `Down`/`Up` sweep vertically, `Left`/`Right`
/// horizontally. `Right`/`Down` reveal content to the right/below (a positive
/// wheel delta — see [`ScrollDirection::delta`]).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Down,
    Up,
    Left,
    Right,
}

impl ScrollDirection {
    /// The opposite sweep direction (`Down`↔`Up`, `Left`↔`Right`).
    pub fn opposite(self) -> ScrollDirection {
        match self {
            ScrollDirection::Down => ScrollDirection::Up,
            ScrollDirection::Up => ScrollDirection::Down,
            ScrollDirection::Left => ScrollDirection::Right,
            ScrollDirection::Right => ScrollDirection::Left,
        }
    }

    /// Signed `(dx, dy)` wheel delta (notches) for one step. `Right`/`Down` are
    /// positive (reveal content to the right/below), `Left`/`Up` negative. A huge
    /// `step` saturates to `i32::MAX` so an absurd caller value can't overflow
    /// (a plain `step as i32` would wrap, and `-(i32::MIN)` panics in debug) —
    /// real steps are single digits.
    pub fn delta(self, step: u32) -> (i32, i32) {
        let s = i32::try_from(step).unwrap_or(i32::MAX);
        match self {
            ScrollDirection::Down => (0, s),
            ScrollDirection::Up => (0, -s),
            ScrollDirection::Right => (s, 0),
            ScrollDirection::Left => (-s, 0),
        }
    }

    /// `true` for a horizontal sweep (`Left`/`Right`).
    pub fn is_horizontal(self) -> bool {
        matches!(self, ScrollDirection::Left | ScrollDirection::Right)
    }

    /// Parse from a tool string (case-insensitive). `None` for unknown.
    pub fn from_name(s: &str) -> Option<ScrollDirection> {
        match s.to_ascii_lowercase().as_str() {
            "down" => Some(ScrollDirection::Down),
            "up" => Some(ScrollDirection::Up),
            "left" => Some(ScrollDirection::Left),
            "right" => Some(ScrollDirection::Right),
            _ => None,
        }
    }

    /// The lowercase tool name (`"down"`/`"up"`/`"left"`/`"right"`), for output.
    pub fn as_str(self) -> &'static str {
        match self {
            ScrollDirection::Down => "down",
            ScrollDirection::Up => "up",
            ScrollDirection::Left => "left",
            ScrollDirection::Right => "right",
        }
    }
}

/// The direction to scroll to bring an off-screen element into view: whichever
/// window edge its bounds lie fully past. `None` when the bounds already
/// intersect the viewport (nothing to infer). Off two edges at once → the larger
/// overflow wins. Used when the caller omits `direction`.
fn offscreen_direction(b: AxRect, win_w: u32, win_h: u32) -> Option<ScrollDirection> {
    // Compute overflow magnitudes in `i64` so bounds near `i32::MAX` can't wrap; the
    // tie-break only needs relative magnitude, not the exact pixel distance.
    let (win_w, win_h) = (i64::from(win_w), i64::from(win_h));
    let (x, y) = (i64::from(b.x), i64::from(b.y));
    let (w, h) = (i64::from(b.width), i64::from(b.height));
    [
        (ScrollDirection::Right, x >= win_w, x - win_w + 1),
        (ScrollDirection::Left, x + w <= 0, -(x + w) + 1),
        (ScrollDirection::Down, y >= win_h, y - win_h + 1),
        (ScrollDirection::Up, y + h <= 0, -(y + h) + 1),
    ]
    .into_iter()
    .filter(|&(_, off, _)| off)
    .max_by_key(|&(_, _, mag)| mag)
    .map(|(dir, _, _)| dir)
}

/// Where to anchor the scroll swipe. An explicit anchor wins upstream; here, if
/// the target node's bounds are known, anchor on its *perpendicular* center so the
/// swipe lands on the container's band even when the target is off-screen along
/// the sweep axis (its off-axis coordinate is still on-screen); otherwise the
/// window center.
fn scroll_anchor(
    dir: ScrollDirection,
    bounds: Option<AxRect>,
    win_w: u32,
    win_h: u32,
) -> (i32, i32) {
    let (win_w, win_h) = (win_w as i32, win_h as i32);
    match bounds {
        Some(b) => {
            let cx = (b.x + b.width as i32 / 2).clamp(0, (win_w - 1).max(0));
            let cy = (b.y + b.height as i32 / 2).clamp(0, (win_h - 1).max(0));
            if dir.is_horizontal() {
                (win_w / 2, cy)
            } else {
                (cx, win_h / 2)
            }
        }
        None => (win_w / 2, win_h / 2),
    }
}

/// Parameters for [`Glass::scroll_to_element`].
#[derive(Clone, Debug)]
pub struct ScrollToElementParams {
    pub name: Option<String>,
    pub role: Option<AxRole>,
    pub value_contains: Option<String>,
    /// Sweep direction; `None` = infer from the target's off-screen bounds
    /// (falling back to `Down`→`Up` when the target isn't in the tree yet).
    pub direction: Option<ScrollDirection>,
    /// Scroll anchor (window-relative). `None` derives the anchor from the target's
    /// own row/column (via the private `scroll_anchor` helper), falling back to the
    /// active window's center only when the target's bounds are unknown.
    pub anchor: Option<(i32, i32)>,
    /// Wheel notches issued per scroll step.
    pub step: u32,
    /// Overall wall-clock bound.
    pub timeout_ms: u64,
}

/// Outcome of [`Glass::scroll_to_element`].
#[derive(Clone, Debug)]
pub struct ScrollToElementOutcome {
    pub matched: bool,
    /// The matched element (absent when `matched` is false). Its id is from the
    /// final snapshot, so it is usable with `click_element`.
    pub element: Option<ElementInfo>,
    pub elapsed_ms: u64,
    /// Total scroll steps issued across the sweep.
    pub steps: u32,
    /// Whether the sweep had reversed past the primary direction when it returned.
    pub reversed: bool,
    /// The resolved (possibly inferred) primary sweep direction.
    pub direction: ScrollDirection,
}

/// Parameters for [`Glass::wait_for_region`].
#[derive(Clone, Debug)]
pub struct WaitRegionParams {
    /// Saved baseline to compare against; `None` uses the frame at call start.
    pub baseline: Option<String>,
    /// Window-relative sub-rectangle to watch; `None` watches the whole window.
    pub region: Option<Region>,
    pub until: RegionUntil,
    /// `true` = perceptual diff (use `threshold`); `false` = exact (use `tolerance`).
    pub perceptual: bool,
    pub threshold: f32,
    pub tolerance: u8,
    pub interval_ms: u64,
    pub timeout_ms: u64,
    /// Window-relative sub-rectangles excluded from the comparison — pixels there
    /// never count toward `changed`/`matches`, so a perpetually animating area (a
    /// blinking caret, a clock) inside the watched region cannot itself satisfy
    /// `until: Changes`, nor block `until: Matches` from converging. When `region`
    /// is set, each rect is intersected with it and translated into region-local
    /// coordinates, so `ignore` is always window-relative regardless of scoping.
    /// With `window` set, "window-relative" means relative to the watched window,
    /// not the active one.
    pub ignore: Vec<Region>,
    /// When set, watch this window's own region instead of the active window's —
    /// without changing which window is active.
    pub window: Option<WindowId>,
}

/// Outcome of [`Glass::wait_for_region`]. `frame` is the last captured region
/// (window when no region), for the optional image at the MCP layer.
#[derive(Clone, Debug)]
pub struct WaitRegionOutcome {
    /// Whether the region condition held before the timeout.
    pub matched: bool,
    /// Percent of the watched region that differed from the reference at the last poll.
    pub changed_pct: f32,
    /// Bounding box of the changed area at the last poll (None if nothing changed).
    pub bbox: Option<BBox>,
    /// The last captured region frame (the watched window when no region) — source for the optional image at the tool layer.
    pub frame: Frame,
    /// Wall-clock milliseconds elapsed when the wait returned.
    pub elapsed_ms: u64,
    /// Pixels an `ignore` mask excluded from the last comparison (counting
    /// overlaps once); 0 when no `ignore` rects were in effect. Mirrors
    /// `glass_diff`'s `ignored_pixels`: when it equals the watched area nothing
    /// was actually compared, so `matched`/`changed_pct` describe an empty diff.
    pub ignored_pixels: u64,
}

/// Parameters for [`Glass::wait_for_log`].
#[derive(Clone, Debug)]
pub struct WaitLogParams {
    /// Substring to wait for (required by the tool layer to be non-empty).
    pub contains: String,
    pub stream: Option<Stream>,
    /// Start scanning from this cursor; `None` = the buffer's end at call start
    /// (so only newly-appended lines count).
    pub cursor: Option<u64>,
    pub interval_ms: u64,
    pub timeout_ms: u64,
}

/// Outcome of [`Glass::wait_for_log`].
#[derive(Clone, Debug)]
pub struct WaitLogOutcome {
    pub matched: bool,
    pub line: Option<LogLine>,
    /// Cursor to resume from: just past the matched line, or the buffer end on timeout.
    pub cursor: u64,
    pub elapsed_ms: u64,
    /// Set on a timeout when the substring was already in the buffer *before* this call's
    /// start cursor (the default-cursor footgun: a fast-boot line is otherwise skipped).
    /// Points the caller at `cursor:0` instead of failing silently.
    pub note: Option<String>,
}

impl Glass {
    pub fn wait_stable(&mut self, params: &WaitStableParams) -> Result<WaitStableOutcome> {
        let active = self.require_active()?;
        // The active window's cached geometry only bounds a stability_region when
        // watching the active window itself; a specific `window` is validated by
        // the backend against its own geometry instead (see `capture`).
        if params.window.is_none() {
            let geo = active.geometry.clone();
            if let Some(r) = &params.stability_region {
                r.check_fits(geo.width, geo.height)?;
            }
        }
        let region = params.stability_region;
        let window = params.window;
        // The mask is built lazily, on the first poll tick, sized from that
        // captured frame's own dimensions rather than the session's cached
        // geometry — which can belong to a different window than the one being
        // watched, or be stale if the watched window resized itself since the
        // cache was last refreshed. `for_region` intersects `ignore` with
        // `region` and translates into region-local coordinates (`capture`
        // crops to `region` when one is set, so the settle comparison and the
        // mask must agree on that same cropped space).
        let mut tracker: Option<StabilityTracker> = None;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            // Poll only the watched region (cheap) when one is set; else the full window.
            let frame = self.capture(window, region.as_ref())?;
            let t = match tracker {
                Some(ref mut t) => t,
                None => {
                    let mask =
                        mask_for(&params.ignore, region.as_ref(), frame.width, frame.height)?;
                    tracker.insert(StabilityTracker::with_mask(
                        params.settle_frames,
                        params.tolerance,
                        mask,
                    ))
                }
            };
            Ok(if t.observe(frame)? { Some(()) } else { None })
        })?;
        let tracker = tracker.expect("poll_until ticks at least once");
        let settled = outcome.value.is_some();
        // Return the full window: a fresh capture if we were polling a sub-region
        // (the genuinely-settled state), else the just-observed full frame.
        let frame = match region {
            Some(_) => self.capture(window, None)?,
            None => tracker.last().cloned().expect("a frame was just observed"),
        };
        Ok(WaitStableOutcome {
            frame,
            settled,
            saw_motion: tracker.saw_change(),
            observed_ms: outcome.elapsed_ms,
            ignored_pixels: tracker.ignored_count(),
        })
    }

    /// Block until a precise accessibility-element condition holds, re-snapshotting
    /// each tick. Text-only outcome. The final snapshot is cached (so the returned
    /// element id is immediately usable with `click_element`). Errors immediately if
    /// the backend has no accessibility reader (the first snapshot fails).
    pub fn wait_for_element(&mut self, params: &WaitElementParams) -> Result<WaitElementOutcome> {
        self.require_active()?; // fail fast; a11y_snapshot rechecks inside the loop
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let tree = self.a11y_snapshot()?; // fresh snapshot; assigns ids, caches, pumps
            Ok(
                match element_match(
                    &tree,
                    params.name.as_deref(),
                    params.role,
                    params.value_contains.as_deref(),
                    params.condition,
                ) {
                    ElementMatch::Satisfied(node) => Some(node.map(ElementInfo::from_node)),
                    ElementMatch::Pending => None,
                },
            )
        })?;
        Ok(WaitElementOutcome {
            matched: outcome.value.is_some(),
            element: outcome.value.flatten(),
            elapsed_ms: outcome.elapsed_ms,
        })
    }

    /// Scroll a container (at `anchor`, default derived from the target's own bounds
    /// — see the private `scroll_anchor` helper — else the active window's center)
    /// until an element matching name/role/value realizes in the a11y tree *and* is
    /// actually on-screen (its bounds intersect the viewport — see
    /// [`AxRect::clamped_center`]; a11y trees can report a node's bounds before it is
    /// scrolled into view), then return it — its id is from the final snapshot, so it
    /// is immediately `click_element`-able. A matched element whose bounds are unknown
    /// (`bounds: None`, from a backend that can't read geometry) is returned as-is:
    /// scrolling can't populate the bounds, so there is nothing to bring into view.
    /// `direction` picks the primary sweep axis explicitly; when omitted it is
    /// inferred from the target's current off-screen bounds (see the private
    /// `offscreen_direction` helper), falling back to `Down` when the target isn't in
    /// the tree yet. For a virtualized list the target row is absent from the tree until
    /// scrolled into range; this checks the current view, sweeps the primary
    /// direction to its end, then reverses to cover the other end. End-of-scroll is
    /// detected from the accessibility tree: when a scroll step leaves the tree's
    /// outline unchanged, the container did not advance (immune to cosmetic repaints
    /// — a scroller's boundary shadow, a focus ring, a blinking caret — that a
    /// pixel-motion signal would misread as "still scrolling"). A target never
    /// realized on-screen after a full bidirectional sweep or `timeout_ms` yields a
    /// soft `{matched:false}` (not an error), like `wait_for_element`. The scroll
    /// actions are audited via the pointer path; there is no separate top-level audit
    /// entry.
    ///
    /// Limitations of the a11y-tree end-of-scroll signal: (1) a container holding a
    /// continuously-repainting a11y node — a live region, a clock, a progress bar —
    /// never leaves the tree "unchanged", so the sweep runs to `timeout_ms` in the
    /// primary direction and returns `{matched:false}` instead of reversing; pass the
    /// `direction` the target actually lies in to avoid the wasted sweep. (2) A very
    /// long list can exceed `timeout_ms` before a distant target scrolls into range —
    /// raise `timeout_ms`, or `step` to cover more per move. (3) With `direction`
    /// omitted, inferring the axis needs the target's current bounds; when the target
    /// isn't in the a11y tree yet (a not-yet-realized virtualized item) there is
    /// nothing to infer from and the sweep defaults to vertical (`down`→`up`) — pass
    /// `direction` explicitly for a horizontal container whose target isn't realized
    /// yet.
    pub fn scroll_to_element(
        &mut self,
        params: &ScrollToElementParams,
    ) -> Result<ScrollToElementOutcome> {
        self.require_active()?;
        let start = std::time::Instant::now();
        let geo = self.geometry()?;
        // Return a match once scrolling can't improve its visibility: it has an
        // on-screen clickable center, or its bounds are unknown (a backend that can't
        // read an element's geometry keeps `bounds: None` — scrolling won't populate
        // them, and `click_element` reports that state honestly). Only a known
        // off-screen element (bounds present but no on-screen intersection) is worth
        // scrolling past.
        let ready = |info: &ElementInfo| match info.bounds {
            Some(b) => b.clamped_center(geo.width, geo.height).is_some(),
            None => true,
        };

        // One pre-sweep snapshot serves four jobs: early return if already visible,
        // direction inference, anchor derivation, and seeding the saturation outline.
        let (found0, mut prev_outline) = self.snapshot_match_outline(params)?;
        let found0_bounds = found0.as_ref().and_then(|i| i.bounds);

        // Resolve the primary sweep direction: explicit, else inferred from the
        // target's off-screen bounds, else the default vertical sweep.
        let primary = params.direction.unwrap_or_else(|| {
            found0_bounds
                .and_then(|b| offscreen_direction(b, geo.width, geo.height))
                .unwrap_or(ScrollDirection::Down)
        });

        // Every return shares this tail (elapsed_ms/direction); only the matched flag,
        // element, step count, and reversed flag vary.
        let outcome = |matched, element, steps, reversed| ScrollToElementOutcome {
            matched,
            element,
            elapsed_ms: start.elapsed().as_millis() as u64,
            steps,
            reversed,
            direction: primary,
        };

        if let Some(info) = found0.filter(|i| ready(i)) {
            return Ok(outcome(true, Some(info), 0, false));
        }

        let (ax, ay) = params
            .anchor
            .unwrap_or_else(|| scroll_anchor(primary, found0_bounds, geo.width, geo.height));

        let mut steps: u32 = 0;
        for (i, dir) in [primary, primary.opposite()].into_iter().enumerate() {
            let reversed = i == 1;
            loop {
                if start.elapsed().as_millis() as u64 >= params.timeout_ms
                    || steps >= SCROLL_TO_MAX_STEPS
                {
                    return Ok(outcome(false, None, steps, reversed));
                }
                let (dx, dy) = dir.delta(params.step);
                self.pointer(&PointerEvent::Scroll {
                    x: ax,
                    y: ay,
                    dx,
                    dy,
                    modifiers: vec![],
                })?;
                steps += 1;
                // Let the scrolled rows/columns realize in the a11y tree before re-reading.
                std::thread::sleep(std::time::Duration::from_millis(SCROLL_TO_SETTLE_MS));
                let (found, outline) = self.snapshot_match_outline(params)?;
                if let Some(info) = found.filter(|i| ready(i)) {
                    return Ok(outcome(true, Some(info), steps, reversed));
                }
                // No change in the a11y tree ⇒ the container did not advance ⇒ this
                // end is reached; sweep the opposite direction.
                let saturated = outline == prev_outline;
                prev_outline = outline;
                if saturated {
                    break;
                }
            }
        }
        Ok(outcome(false, None, steps, true))
    }

    /// Snapshot the current view once; return the matched element (if the selector is
    /// satisfied) and the tree's outline. The snapshot is cached, so a returned
    /// element's id is usable with `click_element`. The outline is the end-of-scroll
    /// signal: unchanged across a scroll step ⇒ the container did not advance.
    fn snapshot_match_outline(
        &mut self,
        params: &ScrollToElementParams,
    ) -> Result<(Option<ElementInfo>, String)> {
        let tree = self.a11y_snapshot()?;
        let found = match element_match(
            &tree,
            params.name.as_deref(),
            params.role,
            params.value_contains.as_deref(),
            ElementCondition::Appears,
        ) {
            ElementMatch::Satisfied(node) => node.map(ElementInfo::from_node),
            ElementMatch::Pending => None,
        };
        Ok((found, tree.to_outline()))
    }

    /// Block until a watched region diverges from / converges to a reference.
    /// Compares in-memory each tick (no WebP encode). Text-only outcome; the last
    /// captured frame is returned for an optional image at the tool layer.
    /// If `baseline` is set and `region` is `None`, the baseline must match the
    /// current window size — a size change since it was saved returns `SizeMismatch`;
    /// crop to a stable `region` to avoid this. `ignore` excludes window-relative
    /// sub-rectangles from every comparison — pixels there never count toward
    /// `changed`/`matches` (see `WaitRegionParams::ignore`).
    pub fn wait_for_region(&mut self, params: &WaitRegionParams) -> Result<WaitRegionOutcome> {
        let active = self.require_active()?;
        // As in `wait_stable`: the active window's cached geometry only bounds
        // `region` when watching the active window; a specific `window` is
        // validated by the backend against its own geometry instead.
        if params.window.is_none() {
            let geo = active.geometry.clone();
            if let Some(r) = &params.region {
                r.check_fits(geo.width, geo.height)?;
            }
        }
        // Reference: a saved baseline (cropped to the region) or the current frame.
        let reference: Frame = match &params.baseline {
            Some(name) => {
                let base = self.baselines.load(name)?;
                match &params.region {
                    Some(r) => base.crop(r)?,
                    None => base,
                }
            }
            None => self.capture(params.window, params.region.as_ref())?,
        };
        // The mask is built once, sized from `reference` — the frame that will
        // actually be compared every tick — not from the session's cached window
        // geometry, which can be stale or belong to a different window (the same
        // trap `wait_stable` hit: see its mask-build comment). Every polled
        // `current` frame is required to match `reference`'s size (the masked
        // diff functions error otherwise via `SizeMismatch`), so `reference`'s own
        // dimensions are exactly the comparison's real size, cropped or not.
        let mask = mask_for(
            &params.ignore,
            params.region.as_ref(),
            reference.width,
            reference.height,
        )?;
        let (perceptual, threshold, tolerance, until, region, window) = (
            params.perceptual,
            params.threshold,
            params.tolerance,
            params.until,
            params.region,
            params.window,
        );
        let mut last: Option<(f32, Option<BBox>, u64, Frame)> = None;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let current = self.capture(window, region.as_ref())?;
            let d = if perceptual {
                diff_perceptual_with_mask(&reference, &current, threshold, &mask)?
            } else {
                diff_with_mask(&reference, &current, tolerance, &mask)?
            };
            let satisfied = region_satisfied(&d, until);
            last = Some((d.changed_pct, d.bbox, d.ignored_pixels, current));
            Ok(if satisfied { Some(()) } else { None })
        })?;
        let (changed_pct, bbox, ignored_pixels, frame) = last.expect("at least one poll ran");
        Ok(WaitRegionOutcome {
            matched: outcome.value.is_some(),
            changed_pct,
            bbox,
            frame,
            elapsed_ms: outcome.elapsed_ms,
            ignored_pixels,
        })
    }

    /// Block until a log line matching `contains` (and optional stream) appears,
    /// scanning from `cursor` (default: the buffer end at call start, so only new
    /// lines count). Returns the matched line and a resume cursor; on timeout
    /// returns `{matched:false}` with the current end cursor.
    pub fn wait_for_log(&mut self, params: &WaitLogParams) -> Result<WaitLogOutcome> {
        let start_cursor = {
            let s = self.active_mut()?;
            s.pump();
            params.cursor.unwrap_or_else(|| s.logs.end_cursor())
        };
        let (contains, stream) = (params.contains.clone(), params.stream);
        let mut scan_cursor = start_cursor;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let s = self.active_mut()?;
            s.pump();
            let (lines, next) = s.logs.read(scan_cursor, 1, stream, Some(&contains));
            scan_cursor = next; // advance past already-examined lines so we don't re-scan
            Ok(lines.into_iter().next())
        })?;
        let s = self.active_mut()?;
        s.pump();
        let end = s.logs.end_cursor();
        Ok(match outcome.value {
            Some(line) => WaitLogOutcome {
                cursor: line.seq + 1,
                line: Some(line),
                matched: true,
                elapsed_ms: outcome.elapsed_ms,
                note: None,
            },
            None => {
                // The default cursor is the buffer end at call start, so a line emitted
                // *before* this call (e.g. a fast-boot "ready") is skipped and we time out.
                // If the substring is already in the buffer before our start cursor, say so
                // rather than failing silently — point the caller at cursor:0.
                let note = if params.cursor.is_none() {
                    let (earlier, _) = s.logs.read(0, 1, stream, Some(&contains));
                    earlier
                        .into_iter()
                        .next()
                        .filter(|l| l.seq < start_cursor)
                        .map(|l| {
                            format!(
                                "{contains:?} was already in the log at seq {} (before this call); \
                                 pass cursor:0 to match already-buffered lines",
                                l.seq
                            )
                        })
                } else {
                    None
                };
                WaitLogOutcome {
                    matched: false,
                    line: None,
                    cursor: end,
                    elapsed_ms: outcome.elapsed_ms,
                    note,
                }
            }
        })
    }
}

#[cfg(test)]
mod tests {
    use super::{offscreen_direction, scroll_anchor};
    use crate::session::test_support::*;

    #[test]
    fn wait_stable_settles_on_repeated_frame() {
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        // a, b, then b repeats forever (FakePlatform repeats the last frame).
        let platform = FakePlatform::new(2, 2).with_frames(vec![a, b.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(outcome.frame, b);
    }

    #[test]
    fn wait_stable_times_out_when_never_settling() {
        // Two alternating frames that never repeat -> never stable.
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [1, 1, 1, 255]);
        let mut frames = Vec::new();
        for _ in 0..50 {
            frames.push(a.clone());
            frames.push(b.clone());
        }
        let platform = FakePlatform::new(2, 2).with_frames(frames);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 5,
                tolerance: 0,
                timeout_ms: 0, // give up after the first non-settling capture
                stability_region: None,
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(!outcome.settled);
    }

    #[test]
    fn wait_stable_settles_using_only_the_stability_region() {
        // The 2x2 top-left region is constant black; only pixel (3,3) changes,
        // so the FULL frames all differ. Settling can only happen if the settle
        // decision looks at the region alone — and the returned frame is full.
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![f0, f1, f2.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(Region {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                }),
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(
            outcome.settled,
            "constant region should settle despite the changing corner"
        );
        assert_eq!(
            outcome.frame, f2,
            "wait_stable returns the FULL frame, not the cropped region"
        );
    }

    #[test]
    fn wait_stable_settles_using_ignore_to_mask_a_blinking_pixel() {
        // Pixel (3,3) blinks every frame — a stand-in for a blinking caret or a
        // clock — while the rest of the 4x4 frame stays constant black. Masking
        // it lets the (otherwise-constant) frame settle on the scripted frames.
        //
        // `settled` alone is NOT the discriminator: without the mask the stream
        // still settles, just *late*. `FakePlatform` repeats its last supplied
        // frame forever once exhausted, so once polling outlasts the 3 scripted
        // frames it compares that repeated final frame to itself and "settles"
        // trivially — proving nothing about `ignore`. Pinning the capture count to
        // exactly 3 (the frames actually supplied) rules that out: settling within
        // them can only happen if the blink was masked from the very first
        // comparison.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1, f2.clone()])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: vec![Region {
                    x: 3,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
                window: None,
            })
            .unwrap();
        assert!(
            outcome.settled,
            "the blinking pixel is masked, so the stream is stable"
        );
        assert_eq!(outcome.frame, f2);
        assert_eq!(
            log.lock().unwrap().len(),
            3,
            "must settle on the 3 supplied frames, not by outlasting them into FakePlatform's repeat"
        );
    }

    #[test]
    fn wait_stable_reports_ignored_pixels_masked_out_of_the_settle_comparison() {
        // A single ignore rect covering the whole 4x4 frame leaves nothing to
        // compare, so the stream settles trivially — and the outcome must surface
        // the full masked count so an agent can tell it compared nothing, rather
        // than reading a hollow `settled: true` (the gap `glass_diff` never had).
        let a = Frame::solid(4, 4, [0, 0, 0, 255]);
        let b = Frame::solid(4, 4, [255, 255, 255, 255]);
        let platform = FakePlatform::new(4, 4).with_frames(vec![a, b]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: vec![Region {
                    x: 0,
                    y: 0,
                    width: 4,
                    height: 4,
                }],
                window: None,
            })
            .unwrap();
        assert_eq!(
            outcome.ignored_pixels, 16,
            "the mask covers the whole 4x4 frame, so every pixel was excluded"
        );
    }

    #[test]
    fn wait_stable_masks_by_captured_frame_size_not_stale_cached_geometry() {
        // The cached window geometry is a deliberately stale/smaller 2x2 —
        // `FakePlatform::new(2, 2)` — while `with_frames` serves the same 4x4
        // blinking frames as the sibling test above. This models watching a
        // window whose real size the session's geometry cache doesn't reflect
        // (a different window, or a self-resize since the cache was last
        // refreshed). The `ignore` rect at (3,3) falls outside the stale 2x2
        // bounds but inside the actual 4x4 frame: if the mask were ever sized
        // from the cached geometry instead of the captured frame, (3,3) would be
        // clamped away, the blink would go unmasked, and the frames would never
        // settle within the timeout.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(2, 2)
            .with_frames(vec![f0, f1, f2.clone()])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: vec![Region {
                    x: 3,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
                window: None,
            })
            .unwrap();
        assert!(
            outcome.settled,
            "the mask must be sized from the captured 4x4 frame, not the stale 2x2 cached geometry"
        );
        assert_eq!(outcome.frame, f2);
        assert_eq!(
            log.lock().unwrap().len(),
            3,
            "must settle on the 3 supplied frames, not by outlasting them into FakePlatform's repeat"
        );
    }

    #[test]
    fn wait_stable_ignore_is_window_relative_under_a_stability_region() {
        // (3,3) blinks and is INSIDE the watched region, so the cropped frames
        // differ every poll; only a window-relative rect translated into
        // region-local space masks it.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1, f2])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(Region {
                    x: 2,
                    y: 2,
                    width: 2,
                    height: 2,
                }),
                ignore: vec![Region {
                    x: 3,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(
            log.lock().unwrap().len(),
            4,
            "3 region polls + 1 final full capture"
        );
    }

    #[test]
    fn wait_stable_polls_only_the_region_and_captures_full_once() {
        // Region constant, corner changing -> settles on the region; the returned
        // frame is a full capture, and every poll captured ONLY the region.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let f2 = frame_4x4_corner([30, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1, f2])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let region = Region {
            x: 0,
            y: 0,
            width: 2,
            height: 2,
        };
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(region),
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(
            (outcome.frame.width, outcome.frame.height),
            (4, 4),
            "returns the full window"
        );
        let calls = log.lock().unwrap();
        let (last, polls) = calls.split_last().expect("at least one capture");
        assert!(
            polls.iter().all(|c| *c == Some(region)),
            "polls capture only the region: {polls:?}"
        );
        assert_eq!(*last, None, "final capture is the full window");
    }

    #[test]
    fn wait_stable_without_region_captures_full_each_poll() {
        let log = Arc::new(Mutex::new(Vec::new()));
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform = FakePlatform::new(2, 2)
            .with_frames(vec![a, b])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(outcome.settled);
        let calls = log.lock().unwrap();
        assert!(
            calls.iter().all(|c| c.is_none()),
            "no-region captures are full: {calls:?}"
        );
    }

    #[test]
    fn wait_stable_rejects_out_of_bounds_stability_region() {
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let err = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: Some(Region {
                    x: 0,
                    y: 0,
                    width: 99,
                    height: 1,
                }),
                ignore: Vec::new(),
                window: None,
            })
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn wait_stable_rejects_zero_area_ignore_rect() {
        // `IgnoreMask` validates this directly, but the mask is now built lazily
        // inside the poll closure — pin that the error still propagates out of
        // `wait_stable` itself, so a future change that swallowed it in there
        // (e.g. treating a build failure as "not yet stable") would be caught.
        let platform =
            FakePlatform::new(4, 4).with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let err = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: vec![Region {
                    x: 0,
                    y: 0,
                    width: 0,
                    height: 1,
                }],
                window: None,
            })
            .unwrap_err();
        assert!(matches!(err, GlassError::InvalidRegion(_)));
    }

    #[test]
    fn wait_stable_with_window_id_uses_capture_window_and_leaves_active_untouched() {
        // Window B is constant, so it settles immediately; watching it must go
        // through capture_window (never capture_frame), and must not disturb the
        // active window (A).
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 100,
                y: 0,
                width: 4,
                height: 4,
            },
            active: false,
        };
        let frame_b = Frame::solid(4, 4, [3, 3, 3, 255]);
        let capture_log = Arc::new(Mutex::new(Vec::new()));
        let capture_window_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_windows(vec![a.clone(), b])
            .with_capture_log(capture_log.clone())
            .with_capture_window_log(capture_window_log.clone())
            .with_window_frame(WindowId(2), frame_b.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.select_window(WindowId(1)).unwrap(); // A is active

        let outcome = g
            .wait_stable(&WaitStableParams {
                interval_ms: 0,
                settle_frames: 2,
                tolerance: 0,
                timeout_ms: 1000,
                stability_region: None,
                ignore: Vec::new(),
                window: Some(WindowId(2)),
            })
            .unwrap();
        assert!(outcome.settled);
        assert_eq!(outcome.frame, frame_b);
        assert_eq!(
            g.geometry().unwrap(),
            a.geometry,
            "active window is still A after watching B"
        );
        assert!(
            capture_log.lock().unwrap().is_empty(),
            "watching a specific window must not go through capture_frame"
        );
        assert!(!capture_window_log.lock().unwrap().is_empty());
    }

    #[test]
    fn wait_for_element_matches_state_and_returns_node() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                role: Some(AxRole::Button),
                value_contains: None,
                condition: ElementCondition::Enabled,
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        let e = o.element.expect("matched element");
        assert_eq!(e.id, AxNodeId(1));
        assert_eq!(e.name.as_deref(), Some("Save"));
    }

    #[test]
    fn wait_for_element_times_out_soft() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                role: None,
                value_contains: None,
                condition: ElementCondition::Checked, // never true in the fixed tree
                interval_ms: 0,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!o.matched);
        assert!(o.element.is_none());
    }

    #[test]
    fn wait_for_element_disappears_is_matched_when_absent() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                condition: ElementCondition::Disappears,
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        assert!(o.element.is_none());
    }

    #[test]
    fn wait_for_element_errors_when_a11y_unsupported() {
        let mut g = glass_with(FakePlatform::new(40, 30)); // no accessibility reader
        g.start(&spec()).unwrap();
        let err = g
            .wait_for_element(&WaitElementParams {
                name: Some("x".into()),
                role: None,
                value_contains: None,
                condition: ElementCondition::Appears,
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap_err();
        assert!(matches!(err, GlassError::AxUnsupported));
    }

    #[test]
    fn scroll_to_element_returns_already_visible_without_scrolling() {
        // The target is present in the current view → return it immediately, steps=0,
        // and no scroll is issued.
        let platform = FakePlatform::new(100, 100);
        let mut g = glass_with_a11y(platform, fake_tree()); // fake_tree has Button "Save"
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Save".into()),
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert_eq!(out.steps, 0);
        assert!(!out.reversed);
        assert_eq!(out.element.unwrap().name.as_deref(), Some("Save"));
        assert_eq!(out.direction, ScrollDirection::Down);
    }

    #[test]
    fn scroll_to_element_absent_sweeps_both_ends_then_reports_unmatched() {
        // The target never appears and the a11y tree's outline never changes (the
        // fixture tree is fixed), so each direction saturates after one step. The
        // sweep must terminate (not hang), reversed, matched:false.
        let platform = FakePlatform::new(100, 100);
        let mut g = glass_with_a11y(platform, fake_tree()); // no node named "Ghost"
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(!out.matched);
        assert!(out.element.is_none());
        assert!(out.reversed, "must have reversed to sweep the other end");
        // One saturating step per direction: no motion breaks each sweep immediately.
        assert_eq!(out.steps, 2);
        assert_eq!(out.direction, ScrollDirection::Down);
    }

    #[test]
    fn scroll_to_element_bounds_unknown_returns_without_scrolling() {
        // A matched element whose backend can't read its geometry keeps `bounds:
        // None`. Scrolling can never populate the bounds, so the match must return
        // immediately (steps == 0) and issue no scroll — not sweep to the cap and
        // report a misleading `matched:false`.
        let scrolls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let platform = FakePlatform::new(100, 100).with_scroll_log(scrolls.clone());
        // A Button with no bounds (a backend that can't read geometry keeps it None).
        let tree = tree_with(100, 100, vec![ax_node(1, AxRole::Button, None, vec![])]);
        let mut g = glass_with_a11y(platform, tree);
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: None,
                role: Some(AxRole::Button),
                value_contains: None,
                direction: None,
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert_eq!(out.steps, 0);
        assert!(out.element.unwrap().bounds.is_none());
        assert!(
            scrolls.lock().unwrap().is_empty(),
            "a bounds-unknown match must not trigger any scroll"
        );
    }

    #[test]
    fn scroll_to_element_realizes_mid_sweep_with_unknown_bounds() {
        // Unlike `scroll_to_element_bounds_unknown_returns_without_scrolling` (the
        // pre-sweep early return), here the target is absent from the first
        // snapshot — forcing a scroll — and only realizes, bounds-unknown, on the
        // second. The in-loop `ready` check must accept it and stop (steps >= 1),
        // not keep sweeping to the cap because it can never see an on-screen center.
        let absent = tree_with(100, 100, vec![]);
        let realized = tree_with(
            100,
            100,
            vec![AxNode {
                name: Some("Ghost".into()),
                ..ax_node(1, AxRole::Button, None, vec![])
            }],
        );
        let mut g = glass_with_a11y_seq(FakePlatform::new(100, 100), vec![absent, realized]);
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert!(out.steps >= 1, "target realized only after scrolling");
        assert!(out.element.unwrap().bounds.is_none());
    }

    #[test]
    fn scroll_to_element_absent_with_omitted_direction_defaults_to_down() {
        // Omitted direction + a target never in the tree: inference has nothing to go
        // on, so the sweep falls back to the vertical down→up axis and reports it.
        let platform = FakePlatform::new(100, 100);
        let mut g = glass_with_a11y(platform, fake_tree()); // no node named "Ghost"
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                direction: None,
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(!out.matched);
        assert_eq!(out.direction, ScrollDirection::Down);
    }

    #[test]
    fn scroll_to_element_absent_horizontal_sweeps_both_ends_then_reports_unmatched() {
        // The horizontal mirror of the vertical absent sweep: the target never
        // appears and the outline never changes, so each end saturates after one
        // step and the sweep terminates reversed, matched:false.
        let platform = FakePlatform::new(100, 100);
        let mut g = glass_with_a11y(platform, fake_tree());
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Right),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(!out.matched);
        assert!(out.reversed, "must have reversed to sweep the other end");
        assert_eq!(out.steps, 2);
        assert_eq!(out.direction, ScrollDirection::Right);
    }

    // A horizontal toolbar (thin band at y≈250) whose "ZoomIn" button is at
    // `zoomin_x`, off the right edge until scrolled into the 1206-wide viewport.
    fn toolbar_tree(zoomin_x: i32) -> AxTree {
        tree_with(
            1206,
            2622,
            vec![
                named_node(
                    1,
                    AxRole::Button,
                    "Red",
                    AxRect {
                        x: 24,
                        y: 226,
                        width: 90,
                        height: 61,
                    },
                ),
                named_node(
                    2,
                    AxRole::Button,
                    "ZoomIn",
                    AxRect {
                        x: zoomin_x,
                        y: 226,
                        width: 164,
                        height: 61,
                    },
                ),
            ],
        )
    }

    #[test]
    fn scroll_to_element_horizontal_returns_only_when_on_screen() {
        // Snapshot 0: ZoomIn off the right edge (x=1600). 1: still off (x=1300).
        // 2: on-screen (x=1000). Require-visible must skip 0 and 1, return at 2.
        let trees = vec![toolbar_tree(1600), toolbar_tree(1300), toolbar_tree(1000)];
        let mut g = glass_with_a11y_seq(FakePlatform::new(1206, 2622), trees);
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("ZoomIn".into()),
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Right),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert!(
            out.steps >= 1,
            "must have scrolled past the off-screen snapshots"
        );
        let b = out.element.unwrap().bounds.unwrap();
        assert!(
            b.clamped_center(1206, 2622).is_some(),
            "returned element is on-screen"
        );
    }

    #[test]
    fn scroll_to_element_infers_right_and_anchors_on_the_row() {
        let scrolls = std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let platform = FakePlatform::new(1206, 2622).with_scroll_log(scrolls.clone());
        // Off right, then on-screen. No `direction` → must infer Right.
        let trees = vec![toolbar_tree(1600), toolbar_tree(1000)];
        let mut g = glass_with_a11y_seq(platform, trees);
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("ZoomIn".into()),
                role: None,
                value_contains: None,
                direction: None,
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert_eq!(
            out.direction,
            ScrollDirection::Right,
            "inferred from off-right bounds"
        );
        // Anchor landed on the toolbar row (y≈226+61/2=256), positive dx (reveal right).
        let logged = scrolls.lock().unwrap();
        let first = logged.first().expect("at least one scroll issued");
        match first {
            PointerEvent::Scroll {
                x: _, y, dx, dy, ..
            } => {
                assert_eq!(*y, 256, "anchored on the ZoomIn row, not the window center");
                assert!(
                    *dx > 0 && *dy == 0,
                    "horizontal, revealing content to the right"
                );
            }
            other => panic!("expected a Scroll, got {other:?}"),
        }
    }

    #[test]
    fn scroll_to_element_infers_down_when_target_below() {
        // A single vertical-list item below the fold, then on-screen. No direction.
        let below = tree_with(
            1206,
            2622,
            vec![named_node(
                1,
                AxRole::Button,
                "Deep",
                AxRect {
                    x: 100,
                    y: 3000,
                    width: 200,
                    height: 60,
                },
            )],
        );
        let on = tree_with(
            1206,
            2622,
            vec![named_node(
                1,
                AxRole::Button,
                "Deep",
                AxRect {
                    x: 100,
                    y: 1200,
                    width: 200,
                    height: 60,
                },
            )],
        );
        let mut g = glass_with_a11y_seq(FakePlatform::new(1206, 2622), vec![below, on]);
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Deep".into()),
                role: None,
                value_contains: None,
                direction: None,
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert_eq!(out.direction, ScrollDirection::Down);
        assert!(
            out.steps >= 1,
            "must have scrolled past the off-screen snapshot"
        );
        let b = out.element.unwrap().bounds.unwrap();
        assert!(
            b.clamped_center(1206, 2622).is_some(),
            "returned element is on-screen"
        );
    }

    #[test]
    fn wait_for_region_changes_matches_on_divergence() {
        // Reference captured at start = black; next frame = white -> "changes".
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform = FakePlatform::new(2, 2).with_frames(vec![a, b]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(o.matched);
        assert!(o.changed_pct > 0.0);
    }

    #[test]
    fn wait_for_region_changes_times_out_when_static() {
        // One frame, repeated -> reference == every poll -> never changes.
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let platform = FakePlatform::new(2, 2).with_frames(vec![a]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 0,
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(!o.matched);
    }

    #[test]
    fn wait_for_region_matches_converges_to_baseline() {
        // save baseline from black; then poll white, then black -> "matches" on black.
        let black = Frame::solid(2, 2, [0, 0, 0, 255]);
        let white = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform =
            FakePlatform::new(2, 2).with_frames(vec![black.clone(), white, black.clone()]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("b").unwrap(); // consumes frame #1 (black)
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: Some("b".into()),
                region: None,
                until: RegionUntil::Matches,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                ignore: Vec::new(),
                window: None,
            })
            .unwrap();
        assert!(o.matched);
        assert_eq!(o.changed_pct, 0.0);
    }

    #[test]
    fn wait_for_region_ignore_masks_a_changing_rect_so_changes_never_matches() {
        // Pixel (3,3) blinks every frame — a stand-in for a blinking caret or a
        // clock — while the rest of the 4x4 frame stays constant. Masking it means
        // `until: Changes` has nothing left to react to: the corner is the only
        // pixel that ever differs, and it is excluded from the comparison.
        //
        // `timeout_ms: 0` bounds the wait to exactly one poll after the reference
        // capture (see `poll_until`), so a generous timeout letting `FakePlatform`
        // outlast its scripted frames into its repeat-forever fallback can't be
        // what makes this pass — the outcome is decided by that single real
        // comparison. Pinning the capture count to 2 (reference + one poll) makes
        // that explicit.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 0,
                ignore: vec![Region {
                    x: 3,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
                window: None,
            })
            .unwrap();
        assert!(
            !o.matched,
            "the only real difference (the corner) is masked, so nothing should register as a change"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "reference capture + exactly one poll, not outlasted into FakePlatform's repeat"
        );
    }

    #[test]
    fn wait_for_region_ignore_is_window_relative_under_a_region() {
        // (3,3) blinks and is INSIDE the watched region (2,2,2,2), so the cropped
        // frames differ every poll; only a window-relative rect translated into
        // region-local space masks it. The region-scoped path was the last one
        // left unexercised for `ignore`; the siblings are
        // `wait_stable_ignore_is_window_relative_under_a_stability_region` and
        // `baseline_ignore_is_window_relative_under_a_region`.
        //
        // Pinning the capture count to 2 (reference + exactly one poll, via
        // `timeout_ms: 0`) makes the translation load-bearing: this can only be
        // `!matched` if the window-relative rect was translated into region-local
        // space and masked the blink on that single real comparison. Drop the
        // translation (build the mask with no region) and the rect lands outside
        // the 2x2 crop, the blink registers, and this flips to `matched`.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: Some(Region {
                    x: 2,
                    y: 2,
                    width: 2,
                    height: 2,
                }),
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 0,
                ignore: vec![Region {
                    x: 3,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
                window: None,
            })
            .unwrap();
        assert!(
            !o.matched,
            "the window-relative rect must translate into region-local space and mask the blink"
        );
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "reference capture + exactly one poll — pins that the translated mask suppressed the only change on the first real comparison"
        );
    }

    #[test]
    fn wait_for_region_ignore_lets_matches_converge_despite_a_changing_rect() {
        // The baseline is saved while the corner is 10; the polled stream then
        // serves a frame with the corner at 20 — otherwise identical. Without
        // masking, that real corner difference would keep `until: Matches` from
        // ever being satisfied; masking it lets the (otherwise-constant) rest of
        // the frame converge on the very first poll.
        //
        // Pinning the capture count to 2 (the baseline save + one poll) rules out
        // a generous timeout eventually matching by other means: it can only
        // happen if the corner was masked from that first real comparison.
        let log = Arc::new(Mutex::new(Vec::new()));
        let f0 = frame_4x4_corner([10, 0, 0, 255]);
        let f1 = frame_4x4_corner([20, 0, 0, 255]);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![f0, f1])
            .with_capture_log(log.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.save_baseline("b").unwrap(); // consumes f0
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: Some("b".into()),
                region: None,
                until: RegionUntil::Matches,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                ignore: vec![Region {
                    x: 3,
                    y: 3,
                    width: 1,
                    height: 1,
                }],
                window: None,
            })
            .unwrap();
        assert!(
            o.matched,
            "the corner is masked, so the rest of the frame matches the baseline immediately"
        );
        assert_eq!(o.changed_pct, 0.0);
        assert_eq!(
            log.lock().unwrap().len(),
            2,
            "baseline save + exactly one poll — matched on the first real comparison"
        );
    }

    #[test]
    fn wait_for_region_reports_ignored_pixels_from_the_last_diff() {
        // The whole 2x2 area is masked, so `until: Changes` never sees a change and
        // the wait times out — but the outcome must still carry the masked count
        // from the final diff, giving the agent the same signal `glass_diff` does.
        let a = Frame::solid(2, 2, [0, 0, 0, 255]);
        let b = Frame::solid(2, 2, [255, 255, 255, 255]);
        let platform = FakePlatform::new(2, 2).with_frames(vec![a, b]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Changes,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 0,
                ignore: vec![Region {
                    x: 0,
                    y: 0,
                    width: 2,
                    height: 2,
                }],
                window: None,
            })
            .unwrap();
        assert_eq!(
            o.ignored_pixels, 4,
            "the mask covers the whole 2x2 area, so every pixel was excluded from the diff"
        );
    }

    #[test]
    fn wait_for_region_with_window_id_uses_capture_window_and_leaves_active_untouched() {
        // Window B is constant, so it matches its own initial capture immediately;
        // watching it must go through capture_window (never capture_frame), and
        // must not disturb the active window (A).
        let a = WindowInfo {
            id: WindowId(1),
            title: Some("A".into()),
            class: None,
            geometry: WindowGeometry {
                x: 0,
                y: 0,
                width: 4,
                height: 4,
            },
            active: true,
        };
        let b = WindowInfo {
            id: WindowId(2),
            title: Some("B".into()),
            class: None,
            geometry: WindowGeometry {
                x: 100,
                y: 0,
                width: 4,
                height: 4,
            },
            active: false,
        };
        let frame_b = Frame::solid(4, 4, [5, 5, 5, 255]);
        let capture_log = Arc::new(Mutex::new(Vec::new()));
        let capture_window_log = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_windows(vec![a.clone(), b])
            .with_capture_log(capture_log.clone())
            .with_capture_window_log(capture_window_log.clone())
            .with_window_frame(WindowId(2), frame_b);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        g.select_window(WindowId(1)).unwrap(); // A is active

        let o = g
            .wait_for_region(&WaitRegionParams {
                baseline: None,
                region: None,
                until: RegionUntil::Matches,
                perceptual: false,
                threshold: 0.1,
                tolerance: 0,
                interval_ms: 0,
                timeout_ms: 1000,
                ignore: Vec::new(),
                window: Some(WindowId(2)),
            })
            .unwrap();
        assert!(o.matched);
        assert_eq!(o.changed_pct, 0.0);
        assert_eq!(
            g.geometry().unwrap(),
            a.geometry,
            "active window is still A after watching B"
        );
        assert!(
            capture_log.lock().unwrap().is_empty(),
            "watching a specific window must not go through capture_frame"
        );
        assert!(
            capture_window_log.lock().unwrap().len() >= 2,
            "reference capture + at least one poll"
        );
    }

    #[test]
    fn wait_for_log_matches_existing_from_cursor_zero() {
        let platform =
            FakePlatform::new(10, 10).with_logs(vec![(Stream::Stdout, "export complete")]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "complete".into(),
                stream: None,
                cursor: Some(0), // scan from the beginning
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        let line = o.line.expect("matched line");
        assert_eq!(line.text, "export complete");
        assert_eq!(o.cursor, line.seq + 1);
    }

    #[test]
    fn wait_for_log_default_cursor_skips_old_lines_and_times_out() {
        // The line already in the buffer is "old" (before the default start cursor),
        // so a default-cursor wait does not match it.
        let platform = FakePlatform::new(10, 10).with_logs(vec![(Stream::Stdout, "old line")]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "old line".into(),
                stream: None,
                cursor: None, // default = end-at-start
                interval_ms: 0,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!o.matched);
        assert!(o.line.is_none());
        // Footgun guard: the line WAS in the buffer (seq 0) before the default start
        // cursor, so the timeout must say so and point at cursor:0 — not fail silently.
        let note = o
            .note
            .expect("timeout note when the substring was already buffered");
        assert!(
            note.contains("cursor:0"),
            "note should point at cursor:0, got: {note}"
        );
        assert!(
            note.contains("seq 0"),
            "note should cite the buffered seq, got: {note}"
        );
    }

    #[test]
    fn wait_for_log_match_cursor_resumes_after_matched_line() {
        // Two lines; match the FIRST -> resume cursor is just after it (1), not the end (2).
        let platform = FakePlatform::new(10, 10).with_logs(vec![
            (Stream::Stdout, "first hit"),
            (Stream::Stdout, "second"),
        ]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "first".into(),
                stream: None,
                cursor: Some(0),
                interval_ms: 0,
                timeout_ms: 1000,
            })
            .unwrap();
        assert!(o.matched);
        assert_eq!(o.line.unwrap().seq, 0);
        assert_eq!(
            o.cursor, 1,
            "resume cursor is just after the matched line, not the buffer end"
        );
    }

    #[test]
    fn scroll_direction_delta_opposite_names() {
        use ScrollDirection::*;
        assert_eq!(Down.opposite(), Up);
        assert_eq!(Up.opposite(), Down);
        assert_eq!(Left.opposite(), Right);
        assert_eq!(Right.opposite(), Left);

        // Right/Down are positive; Left/Up negative. (dx, dy).
        assert_eq!(Down.delta(3), (0, 3));
        assert_eq!(Up.delta(3), (0, -3));
        assert_eq!(Right.delta(3), (3, 0));
        assert_eq!(Left.delta(3), (-3, 0));
        // An absurd step saturates instead of overflowing/panicking.
        assert_eq!(Right.delta(u32::MAX), (i32::MAX, 0));
        assert_eq!(Left.delta(u32::MAX), (-i32::MAX, 0));

        assert!(Left.is_horizontal() && Right.is_horizontal());
        assert!(!Down.is_horizontal() && !Up.is_horizontal());

        assert_eq!(ScrollDirection::from_name("DOWN"), Some(Down));
        assert_eq!(ScrollDirection::from_name("up"), Some(Up));
        assert_eq!(ScrollDirection::from_name("left"), Some(Left));
        assert_eq!(ScrollDirection::from_name("Right"), Some(Right));
        assert_eq!(ScrollDirection::from_name("sideways"), None);

        assert_eq!(Down.as_str(), "down");
        assert_eq!(Up.as_str(), "up");
        assert_eq!(Left.as_str(), "left");
        assert_eq!(Right.as_str(), "right");
    }

    #[test]
    fn offscreen_direction_picks_the_edge() {
        // Fully past the right edge (x >= win_w).
        let r = AxRect {
            x: 1300,
            y: 250,
            width: 100,
            height: 60,
        };
        assert_eq!(
            offscreen_direction(r, 1206, 2622),
            Some(ScrollDirection::Right)
        );
        // Fully past the left edge (x + w <= 0).
        let l = AxRect {
            x: -300,
            y: 250,
            width: 100,
            height: 60,
        };
        assert_eq!(
            offscreen_direction(l, 1206, 2622),
            Some(ScrollDirection::Left)
        );
        // Past the bottom edge.
        let d = AxRect {
            x: 100,
            y: 3000,
            width: 100,
            height: 60,
        };
        assert_eq!(
            offscreen_direction(d, 1206, 2622),
            Some(ScrollDirection::Down)
        );
        // Intersects the viewport → nothing to infer.
        let on = AxRect {
            x: 100,
            y: 100,
            width: 100,
            height: 60,
        };
        assert_eq!(offscreen_direction(on, 1206, 2622), None);
        // Off two edges at once → larger overflow wins (right ~2001 vs down ~501).
        let both = AxRect {
            x: 3206,
            y: 3122,
            width: 10,
            height: 10,
        };
        assert_eq!(
            offscreen_direction(both, 1206, 2622),
            Some(ScrollDirection::Right)
        );
    }

    #[test]
    fn scroll_anchor_lands_on_the_container_band() {
        // Horizontal sweep: anchor x = window center, y = the element's row center.
        let h = AxRect {
            x: 2000,
            y: 250,
            width: 100,
            height: 60,
        };
        assert_eq!(
            scroll_anchor(ScrollDirection::Right, Some(h), 1206, 2622),
            (603, 280)
        );
        // Vertical sweep: anchor x = the element's column center, y = window center.
        let v = AxRect {
            x: 300,
            y: 2000,
            width: 100,
            height: 60,
        };
        assert_eq!(
            scroll_anchor(ScrollDirection::Down, Some(v), 1206, 2622),
            (350, 1311)
        );
        // No bounds → window center.
        assert_eq!(
            scroll_anchor(ScrollDirection::Down, None, 1206, 2622),
            (603, 1311)
        );
    }
}
