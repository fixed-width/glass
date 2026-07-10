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
    /// and then quieted.
    pub saw_motion: bool,
    /// How long (ms) frames were observed before settling or timing out.
    pub observed_ms: u64,
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

/// A vertical scroll sweep direction.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ScrollDirection {
    Down,
    Up,
}

impl ScrollDirection {
    /// The other sweep direction.
    pub fn opposite(self) -> ScrollDirection {
        match self {
            ScrollDirection::Down => ScrollDirection::Up,
            ScrollDirection::Up => ScrollDirection::Down,
        }
    }
    /// Signed vertical wheel delta (notches): `Down` is positive (wheel-down),
    /// `Up` negative. Saturates a huge `step` to `i32::MAX` so an absurd caller
    /// value can't overflow (a plain `step as i32` would wrap, and `-(i32::MIN)`
    /// panics in debug) — real steps are single digits.
    pub fn dy(self, step: u32) -> i32 {
        let s = i32::try_from(step).unwrap_or(i32::MAX);
        match self {
            ScrollDirection::Down => s,
            ScrollDirection::Up => -s,
        }
    }
    /// Parse from a tool string (case-insensitive). `None` for unknown.
    pub fn from_name(s: &str) -> Option<ScrollDirection> {
        match s.to_ascii_lowercase().as_str() {
            "down" => Some(ScrollDirection::Down),
            "up" => Some(ScrollDirection::Up),
            _ => None,
        }
    }
}

/// Parameters for [`Glass::scroll_to_element`].
#[derive(Clone, Debug)]
pub struct ScrollToElementParams {
    pub name: Option<String>,
    pub role: Option<AxRole>,
    pub value_contains: Option<String>,
    /// Primary sweep direction; the search reverses to the other end if the
    /// target isn't found first.
    pub direction: ScrollDirection,
    /// Scroll anchor (window-relative). `None` → the active window's center.
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
        let mut tracker = StabilityTracker::new(params.settle_frames, params.tolerance);
        let region = params.stability_region;
        let window = params.window;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            // Poll only the watched region (cheap) when one is set; else the full window.
            let frame = self.capture(window, region.as_ref())?;
            let settled = tracker.observe(frame)?;
            Ok(if settled { Some(()) } else { None })
        })?;
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

    /// Scroll a container (at `anchor`, default the active window's center) until an
    /// element matching name/role/value realizes in the a11y tree, then return it —
    /// its id is from the final snapshot, so it is immediately `click_element`-able.
    /// For a virtualized list the target row is absent from the tree until scrolled
    /// into range; this checks the current view, sweeps the primary `direction` to
    /// its end, then reverses to cover the other end. End-of-scroll is detected from
    /// the accessibility tree: when a scroll step leaves the tree's outline unchanged,
    /// the container did not advance (immune to cosmetic repaints — a scroller's
    /// boundary shadow, a focus ring, a blinking caret — that a pixel-motion signal
    /// would misread as "still scrolling"). A target never realized after a full
    /// bidirectional sweep or `timeout_ms` yields a soft `{matched:false}` (not an
    /// error), like `wait_for_element`. The scroll actions are audited via the pointer
    /// path; there is no separate top-level audit entry.
    ///
    /// Limitations of the a11y-tree end-of-scroll signal: (1) a container holding a
    /// continuously-repainting a11y node — a live region, a clock, a progress bar —
    /// never leaves the tree "unchanged", so the sweep runs to `timeout_ms` in the
    /// primary direction and returns `{matched:false}` instead of reversing; pass the
    /// `direction` the target actually lies in to avoid the wasted sweep. (2) A very
    /// long list can exceed `timeout_ms` before a distant target scrolls into range —
    /// raise `timeout_ms`, or `step` to cover more per move.
    pub fn scroll_to_element(
        &mut self,
        params: &ScrollToElementParams,
    ) -> Result<ScrollToElementOutcome> {
        self.require_active()?;
        let start = std::time::Instant::now();
        let geo = self.geometry()?;
        let (ax, ay) = params
            .anchor
            .unwrap_or((geo.width as i32 / 2, geo.height as i32 / 2));

        // Snapshot the current view: return immediately if already realized, and seed
        // the outline the first scroll step is compared against.
        let (found, mut prev_outline) = self.snapshot_match_outline(params)?;
        if let Some(info) = found {
            return Ok(ScrollToElementOutcome {
                matched: true,
                element: Some(info),
                elapsed_ms: start.elapsed().as_millis() as u64,
                steps: 0,
                reversed: false,
            });
        }

        let mut steps: u32 = 0;
        for (i, dir) in [params.direction, params.direction.opposite()]
            .into_iter()
            .enumerate()
        {
            let reversed = i == 1;
            loop {
                if start.elapsed().as_millis() as u64 >= params.timeout_ms
                    || steps >= SCROLL_TO_MAX_STEPS
                {
                    return Ok(ScrollToElementOutcome {
                        matched: false,
                        element: None,
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        steps,
                        reversed,
                    });
                }
                self.pointer(&PointerEvent::Scroll {
                    x: ax,
                    y: ay,
                    dx: 0,
                    dy: dir.dy(params.step),
                    modifiers: vec![],
                })?;
                steps += 1;
                // Let the scrolled rows realize in the a11y tree before re-reading.
                std::thread::sleep(std::time::Duration::from_millis(SCROLL_TO_SETTLE_MS));
                let (found, outline) = self.snapshot_match_outline(params)?;
                if let Some(info) = found {
                    return Ok(ScrollToElementOutcome {
                        matched: true,
                        element: Some(info),
                        elapsed_ms: start.elapsed().as_millis() as u64,
                        steps,
                        reversed,
                    });
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
        Ok(ScrollToElementOutcome {
            matched: false,
            element: None,
            elapsed_ms: start.elapsed().as_millis() as u64,
            steps,
            reversed: true,
        })
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
    /// crop to a stable `region` to avoid this.
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
        let (perceptual, threshold, tolerance, until, region, window) = (
            params.perceptual,
            params.threshold,
            params.tolerance,
            params.until,
            params.region,
            params.window,
        );
        let mut last: Option<(f32, Option<BBox>, Frame)> = None;
        let outcome = crate::poll::poll_until(params.interval_ms, params.timeout_ms, || {
            let current = self.capture(window, region.as_ref())?;
            let d = if perceptual {
                diff_perceptual(&reference, &current, threshold)?
            } else {
                diff(&reference, &current, tolerance)?
            };
            let satisfied = region_satisfied(&d, until);
            last = Some((d.changed_pct, d.bbox, current));
            Ok(if satisfied { Some(()) } else { None })
        })?;
        let (changed_pct, bbox, frame) = last.expect("at least one poll ran");
        Ok(WaitRegionOutcome {
            matched: outcome.value.is_some(),
            changed_pct,
            bbox,
            frame,
            elapsed_ms: outcome.elapsed_ms,
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
