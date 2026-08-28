//! `Glass` synchronization: wait-for-stable/element/region/log, scroll-to-element,
//! and the wait/scroll parameter and outcome types.
use super::*;
use crate::accessibility::ElementSelector;

/// How long to go on a signal's word alone before reading the tree anyway.
///
/// A signal reports the change classes it subscribed to, from the senders it resolved; anything
/// outside that would otherwise let a wait answer "not matched" without ever looking again — a
/// wrong result rather than a slow one. This bounds that to added latency, one re-read per second
/// no matter what the platform does or does not announce.
///
/// Wall-clock, and deliberately not a count of quiet intervals: a count scales with the caller's
/// `interval_ms` and can sit past the caller's whole timeout — ten at the 200ms
/// `glass_wait_for_element` default is two seconds, which put the ceiling out of reach of exactly
/// the short waits that could least afford one stale read.
const REREAD_AFTER: std::time::Duration = std::time::Duration::from_secs(1);

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
    pub description: Option<String>,
    pub role: Option<AxRole>,
    pub value: Option<String>,
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
    /// Which timeout ended the wait. `None` when the predicate was satisfied.
    #[doc(hidden)]
    pub timed_out_by: Option<crate::Whose>,
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
    pub description: Option<String>,
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
    /// Which timeout ended the sweep. `None` for a match, saturation, or the step cap.
    #[doc(hidden)]
    pub timed_out_by: Option<crate::Whose>,
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
    /// Wait until the screen stops changing.
    ///
    /// Not event-gated, and must not be: this waits on *pixels* settling, and an accessibility
    /// event says nothing about that — an animation emits none, and a tree change may move none.
    pub fn wait_stable(&mut self, params: &WaitStableParams) -> Result<WaitStableOutcome> {
        self.wait_stable_by(params, Deadline::UNBOUNDED)
    }

    /// [`Self::wait_stable`] bounded by a caller's shared deadline.
    pub fn wait_stable_by(
        &mut self,
        params: &WaitStableParams,
        caller: Deadline,
    ) -> Result<WaitStableOutcome> {
        if caller.has_passed() {
            return Err(GlassError::deadline_not_started("wait for stable"));
        }
        let started = std::time::Instant::now();
        let (effective_duration, whose) =
            caller.budget(std::time::Duration::from_millis(params.timeout_ms), started);
        let deadline = Deadline::at(started + effective_duration);
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
        // Built lazily on the first tick and sized from that frame, not the session's
        // cached geometry, which can belong to a different window or be stale after a
        // self-resize. `for_region` intersects `ignore` with `region` and translates into
        // region-local coordinates, since `capture` crops to `region` and the settle
        // comparison and the mask must agree on that space.
        let mut tracker: Option<StabilityTracker> = None;
        let mut looked = false;
        let outcome = crate::poll::poll_until_with_pause(
            params.interval_ms,
            effective_duration.as_millis() as u64,
            |d| {
                std::thread::sleep(deadline.remaining().unwrap_or(d).min(d));
                true
            },
            || {
                // Poll only the watched region (cheap) when one is set; else the full window.
                let capture_deadline = if !looked && params.timeout_ms == 0 {
                    caller
                } else {
                    deadline
                };
                looked = true;
                let frame = self.capture_by(window, region.as_ref(), capture_deadline)?;
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
            },
        )?;
        let tracker = tracker.expect("poll_until ticks at least once");
        let settled = outcome.value.is_some();
        // Return the full window: a fresh capture if we were polling a sub-region
        // (the genuinely-settled state), else the just-observed full frame.
        let frame = match region {
            Some(_) if outcome.value.is_none() && whose == crate::Whose::Callee => {
                self.capture_by(window, None, caller)?
            }
            Some(_) => self.capture_by(window, None, deadline)?,
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
        self.wait_for_element_by(params, Deadline::UNBOUNDED)
    }

    /// [`Self::wait_for_element`] bounded by a caller's shared deadline.
    pub fn wait_for_element_by(
        &mut self,
        params: &WaitElementParams,
        caller: Deadline,
    ) -> Result<WaitElementOutcome> {
        if caller.has_passed() {
            return Err(GlassError::deadline_not_started("wait for element"));
        }
        self.require_active()?; // fail fast; a11y_snapshot rechecks inside the loop
        let started = std::time::Instant::now();
        // Every read this wait makes carries when the wait stops: the tick is synchronous, so the
        // loop cannot take back a read a reader has started (glass#338).
        let (effective_duration, whose) =
            caller.budget(std::time::Duration::from_millis(params.timeout_ms), started);
        let deadline = Deadline::at(started + effective_duration);
        // Before the first walk, not after: a change landing in that gap is announced to nobody,
        // and the wait then burns its whole budget on a condition that already holds.
        //
        // An interval of 0 means "re-read as fast as you can", which never pauses — so there is
        // nothing for a signal to save, and subscribing would only cost a round-trip.
        let mut signal = (params.interval_ms > 0)
            .then(|| self.subscribe_a11y_changes(deadline))
            .flatten();
        // Subscribing spends the caller's budget, so the poll loop gets what is left. That
        // bounds the polling, not the call: a reader that does not honour `deadline` bounds its
        // own handshake in seconds, so a wait told to give up after 500ms can return later.
        let remaining = (effective_duration.as_millis() as u64)
            .saturating_sub(started.elapsed().as_millis() as u64);
        // Starts now rather than at the first read: the first tick follows immediately.
        let mut last_read = std::time::Instant::now();
        // Only a wait that never saw a tree reports why it saw none. A reader honouring
        // `deadline` gives up on the tick that ends the wait, so without this every unmatched wait
        // on such a backend failed instead of answering `{matched:false}` (glass#338).
        let mut unread: Option<GlassError> = None;
        let mut saw_a_tree = false;
        // The first read carries no deadline: `poll_until_with_pause` guarantees one tick, so a
        // wait must look once, and `timeout_ms: 0` ("check now") would otherwise error against a
        // healthy app nobody consulted. Reads after it carry the bound.
        let mut looked = false;
        let outcome = crate::poll::poll_until_with_pause(
            params.interval_ms,
            remaining,
            |d| {
                let paused_at = std::time::Instant::now();
                let pause_budget = deadline.remaining().unwrap_or(d).min(d);
                let read_now = match signal.as_mut() {
                    Some(s) => match s.wait(pause_budget) {
                        ChangeWait::Changed => true,
                        // Read anyway now and then — see `REREAD_AFTER`.
                        ChangeWait::Quiet => last_read.elapsed() >= REREAD_AFTER,
                        // See `ChangeWait`: an unusable signal must not read as a quiet one.
                        ChangeWait::Unusable => {
                            signal = None;
                            true
                        }
                    },
                    None => true,
                };
                if read_now {
                    last_read = std::time::Instant::now();
                }
                // One interval between reads, whatever woke us. A chatty app (spinner, clock,
                // progress bar) would otherwise drive back-to-back walks with no gap, hammering the
                // same bus the app is trying to serve — and a signal that returns early without
                // blocking, which nothing here can enforce, would spin this loop at full tilt.
                std::thread::sleep(pause_budget.saturating_sub(paused_at.elapsed()));
                read_now
            },
            || {
                // fresh snapshot; assigns ids, caches, pumps
                let bound = if !looked && caller == Deadline::UNBOUNDED && params.timeout_ms == 0 {
                    Deadline::UNBOUNDED
                } else {
                    deadline
                };
                looked = true;
                let tree = match self.a11y_resnapshot(bound) {
                    Ok(t) => {
                        saw_a_tree = true;
                        t
                    }
                    // Kept so a spent budget can report this instead of "not found" (glass#329).
                    Err(e @ GlassError::AccessibilityNotReady(_)) => {
                        unread = Some(e);
                        return Ok(None);
                    }
                    Err(e) => return Err(e),
                };
                Ok(
                    match tree.element_match_selector(
                        ElementSelector {
                            name: params.name.as_deref(),
                            description: params.description.as_deref(),
                            role: params.role,
                            value: params.value.as_deref(),
                            value_contains: params.value_contains.as_deref(),
                        },
                        params.condition,
                    ) {
                        ElementMatch::Satisfied(node) => Some(node.map(ElementInfo::from_node)),
                        ElementMatch::Pending => None,
                    },
                )
            },
        )?;
        if outcome.value.is_none()
            && !saw_a_tree
            && let Some(e) = unread
        {
            return Err(e);
        }
        let matched = outcome.value.is_some();
        Ok(WaitElementOutcome {
            matched,
            element: outcome.value.flatten(),
            // From before the subscription, not from the poll loop: the agent is told how long the
            // call took, and the subscribe is part of it.
            elapsed_ms: started.elapsed().as_millis() as u64,
            timed_out_by: (!matched).then_some(whose),
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
        self.scroll_to_element_by(params, Deadline::UNBOUNDED)
    }

    /// [`Self::scroll_to_element`] bounded by a caller's shared deadline.
    pub fn scroll_to_element_by(
        &mut self,
        params: &ScrollToElementParams,
        caller: Deadline,
    ) -> Result<ScrollToElementOutcome> {
        if caller.has_passed() {
            return Err(GlassError::deadline_not_started("scroll to element"));
        }
        self.require_active()?;
        let start = std::time::Instant::now();
        let (effective_duration, whose) =
            caller.budget(std::time::Duration::from_millis(params.timeout_ms), start);
        let deadline = Deadline::at(start + effective_duration);
        let geo = self.geometry()?;
        // Return a match once scrolling can't improve its visibility: it has an on-screen
        // clickable center, or its bounds are unknown (scrolling won't populate a
        // `bounds: None` a backend couldn't read).
        let ready = |info: &ElementInfo| match info.bounds {
            Some(b) => b.clamped_center(geo.width, geo.height).is_some(),
            None => true,
        };

        // One pre-sweep snapshot serves four jobs: early return if already visible,
        // direction inference, anchor derivation, and seeding the saturation outline.
        let first_deadline = if caller == Deadline::UNBOUNDED && params.timeout_ms == 0 {
            Deadline::UNBOUNDED
        } else {
            deadline
        };
        let Some((found0, mut prev_outline)) =
            self.snapshot_match_outline(params, first_deadline)?
        else {
            return Ok(ScrollToElementOutcome {
                matched: false,
                element: None,
                elapsed_ms: start.elapsed().as_millis() as u64,
                steps: 0,
                reversed: false,
                direction: params.direction.unwrap_or(ScrollDirection::Down),
                timed_out_by: Some(whose),
            });
        };
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
        let outcome = |matched, element, steps, reversed, timed_out_by| ScrollToElementOutcome {
            matched,
            element,
            elapsed_ms: start.elapsed().as_millis() as u64,
            steps,
            reversed,
            direction: primary,
            timed_out_by,
        };

        if let Some(info) = found0.filter(|i| ready(i)) {
            return Ok(outcome(true, Some(info), 0, false, None));
        }

        let (ax, ay) = params
            .anchor
            .unwrap_or_else(|| scroll_anchor(primary, found0_bounds, geo.width, geo.height));

        let mut steps: u32 = 0;
        for (i, dir) in [primary, primary.opposite()].into_iter().enumerate() {
            let reversed = i == 1;
            loop {
                if deadline.has_passed() {
                    return Ok(outcome(false, None, steps, reversed, Some(whose)));
                }
                if steps >= SCROLL_TO_MAX_STEPS {
                    return Ok(outcome(false, None, steps, reversed, None));
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
                let settle = std::time::Duration::from_millis(SCROLL_TO_SETTLE_MS);
                std::thread::sleep(deadline.remaining().unwrap_or(settle).min(settle));
                if deadline.has_passed() {
                    return Ok(outcome(false, None, steps, reversed, Some(whose)));
                }
                let Some((found, outline)) = self.snapshot_match_outline(params, deadline)? else {
                    return Ok(outcome(false, None, steps, reversed, Some(whose)));
                };
                if let Some(info) = found.filter(|i| ready(i)) {
                    return Ok(outcome(true, Some(info), steps, reversed, None));
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
        Ok(outcome(false, None, steps, true, None))
    }

    /// Snapshot the current view once; return the matched element (if the selector is
    /// satisfied) and the tree's outline. The snapshot is cached, so a returned
    /// element's id is usable with `click_element`. The outline is the end-of-scroll
    /// signal: unchanged across a scroll step ⇒ the container did not advance.
    fn snapshot_match_outline(
        &mut self,
        params: &ScrollToElementParams,
        deadline: Deadline,
    ) -> Result<Option<(Option<ElementInfo>, String)>> {
        let tree = match self.a11y_resnapshot(deadline) {
            Ok(tree) => tree,
            Err(GlassError::AccessibilityNotReady(_)) if deadline.has_passed() => return Ok(None),
            Err(error) => return Err(error),
        };
        let found = match tree.element_match_selector(
            ElementSelector {
                name: params.name.as_deref(),
                description: params.description.as_deref(),
                role: params.role,
                value: None,
                value_contains: params.value_contains.as_deref(),
            },
            ElementCondition::Appears,
        ) {
            ElementMatch::Satisfied(node) => node.map(ElementInfo::from_node),
            ElementMatch::Pending => None,
        };
        Ok(Some((found, tree.to_outline())))
    }

    /// Block until a watched region diverges from / converges to a reference.
    /// Compares in-memory each tick (no WebP encode). Text-only outcome; the last
    /// captured frame is returned for an optional image at the tool layer.
    /// If `baseline` is set and `region` is `None`, the baseline must match the
    /// current window size — a size change since it was saved returns `SizeMismatch`;
    /// crop to a stable `region` to avoid this. `ignore` excludes window-relative
    /// sub-rectangles from every comparison — pixels there never count toward
    /// `changed`/`matches` (see `WaitRegionParams::ignore`).
    /// Not event-gated, for the same reason as [`Glass::wait_stable`]: the subject is a captured
    /// region, not the accessibility tree.
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
        // Built once, sized from `reference` — the frame actually compared every tick — not
        // from the session's cached geometry, which can be stale or belong to a different
        // window. Every polled frame must match `reference`'s size (`SizeMismatch` otherwise),
        // so those dimensions are the comparison's real size, cropped or not.
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
    /// Not event-gated, and must not be: an app writes to stdout without emitting any accessibility
    /// event — a canvas app emits none at all — so a gated loop would miss lines for a whole
    /// timeout.
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
    fn callee_timeout_final_settle_capture_keeps_the_bounded_caller_deadline() {
        let deadlines = Arc::new(Mutex::new(Vec::new()));
        let caller = Deadline::from_millis(1_000);
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])])
            .with_capture_deadline_log(deadlines.clone())
            .with_capture_delay(Duration::from_millis(20));
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();

        let outcome = g
            .wait_stable_by(
                &WaitStableParams {
                    interval_ms: 0,
                    settle_frames: 2,
                    tolerance: 0,
                    timeout_ms: 10,
                    stability_region: Some(Region {
                        x: 0,
                        y: 0,
                        width: 2,
                        height: 2,
                    }),
                    ignore: Vec::new(),
                    window: None,
                },
                caller,
            )
            .unwrap();

        assert!(!outcome.settled);
        let deadlines = deadlines.lock().unwrap();
        assert_eq!(deadlines.len(), 2);
        assert_eq!(deadlines[1], caller);
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
        // `settled` alone is NOT the discriminator: `FakePlatform` repeats its last supplied
        // frame forever, so polling past the 3 scripted frames compares that repeat to itself
        // and settles trivially. Pinning the capture count to 3 rules that out.
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
        // The cached geometry is a deliberately stale 2x2 while the frames are 4x4 — a window
        // whose real size the cache doesn't reflect. The `ignore` rect at (3,3) is outside the
        // stale bounds but inside the real frame, so a mask sized from the cache would clamp
        // it away and the blink would never settle.
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
                description: None,
                role: Some(AxRole::Button),
                value: None,
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

    /// glass#338: on Android a `uiautomator dump` spending its own 20s budget answered a 10s
    /// wait, because `poll_until_with_pause` ticks synchronously and only the reader can stop one
    /// read early.
    ///
    /// This asserts over the reads *after* the first, which is deliberately unbounded — see
    /// [`a_wait_looks_once_however_little_time_it_was_given`].
    #[test]
    fn a_wait_tells_the_reader_when_it_stops_waiting() {
        let (mut g, seen) = glass_with_a11y_until_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();
        g.wait_for_element(&WaitElementParams {
            name: Some("Ghost".into()), // never matches, so the wait reads more than once
            description: None,
            role: None,
            value: None,
            value_contains: None,
            condition: ElementCondition::Appears,
            interval_ms: 5,
            timeout_ms: 400,
        })
        .unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen.len() > 1, "the wait read only once: {}", seen.len());
        let left = seen[1].expect("the read after the first is bounded by the wait's timeout");
        // A lower bound as well as an upper one: a deadline built from `interval_ms`, or from any
        // constant smaller than the timeout, passes an upper bound alone (glass#284).
        assert!(
            left > std::time::Duration::from_millis(200),
            "the second read's bound is not the wait's 400ms timeout: {left:?}"
        );
        assert!(
            left <= std::time::Duration::from_millis(400),
            "a deadline past the wait's own timeout bounds nothing: {left:?}"
        );
    }

    /// A wait that read the tree and did not find the element answers `{matched:false}`, which is
    /// what `glass_wait_for_element`'s schema and the tool description promise.
    ///
    /// The poll loop checks its budget *after* a tick, so the last read always starts at the
    /// deadline and a reader honouring it always gives up there.
    #[test]
    fn a_wait_that_read_the_tree_reports_no_match_even_when_its_last_read_ran_out_of_time() {
        let (mut g, seen) = glass_with_a11y_until_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Ghost".into()), // never in the tree
                description: None,
                role: None,
                value: None,
                value_contains: None,
                condition: ElementCondition::Appears,
                interval_ms: 10,
                timeout_ms: 120,
            })
            .expect("a wait that read the UI reports the element absent, it does not fail");

        assert!(!o.matched);
        assert!(
            seen.lock().unwrap().len() > 1,
            "the wait answered from one read, so it never reached the read that runs out of time"
        );
    }

    /// A wait looks once however little time it was given: `poll_until_with_pause` guarantees one
    /// tick, and `timeout_ms: 0` means "check now" — answering for a device nobody consulted is
    /// the one outcome worse than answering late.
    #[test]
    fn a_wait_looks_once_however_little_time_it_was_given() {
        let (mut g, seen) = glass_with_a11y_until_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                description: None,
                role: Some(AxRole::Button),
                value: None,
                value_contains: None,
                condition: ElementCondition::Appears,
                interval_ms: 0,
                timeout_ms: 0,
            })
            .expect("a wait given no time still looks once");

        assert!(o.matched, "the element is in the very first tree read");
        let seen = seen.lock().unwrap();
        assert_eq!(seen.len(), 1);
        assert_eq!(
            seen[0], None,
            "the one look a wait is guaranteed was handed a bound it could not meet"
        );
    }

    #[test]
    fn a_scroll_sweep_bounds_the_reader_by_its_own_budget() {
        let (mut g, ctx_log) = glass_with_a11y_ctx(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        g.scroll_to_element(&ScrollToElementParams {
            name: Some("Ghost".into()),
            description: None,
            role: None,
            value_contains: None,
            direction: Some(ScrollDirection::Down),
            anchor: None,
            step: SCROLL_TO_DEFAULT_STEP,
            timeout_ms: 20_000,
        })
        .unwrap();

        assert!(
            ctx_log
                .lock()
                .unwrap()
                .as_ref()
                .expect("the sweep read the tree")
                .deadline
                .remaining()
                .is_some(),
            "the sweep did not pass its effective deadline to the reader"
        );
    }

    #[test]
    fn standalone_scroll_snapshot_deadline_returns_a_soft_callee_timeout() {
        let (mut g, _) = glass_with_a11y_not_ready_at_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();

        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: 20,
            })
            .expect("an own snapshot deadline is a soft scroll timeout");

        assert!(!out.matched);
        assert_eq!(out.timed_out_by, Some(crate::Whose::Callee));
    }

    #[test]
    fn caller_scroll_snapshot_deadline_returns_a_soft_caller_timeout() {
        let (mut g, _) = glass_with_a11y_not_ready_at_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();

        let out = g
            .scroll_to_element_by(
                &ScrollToElementParams {
                    name: Some("Ghost".into()),
                    description: None,
                    role: None,
                    value_contains: None,
                    direction: Some(ScrollDirection::Down),
                    anchor: None,
                    step: SCROLL_TO_DEFAULT_STEP,
                    timeout_ms: 1_000,
                },
                Deadline::from_millis(20),
            )
            .expect("a caller snapshot deadline is a soft scroll timeout");

        assert!(!out.matched);
        assert_eq!(out.timed_out_by, Some(crate::Whose::Caller));
    }

    #[test]
    fn scroll_zero_timeout_first_snapshot_remains_unbounded() {
        let (mut g, seen) = glass_with_a11y_not_ready_at_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();

        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Save".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: 0,
            })
            .unwrap();

        assert!(out.matched);
        assert_eq!(*seen.lock().unwrap(), vec![Deadline::UNBOUNDED]);
    }

    #[test]
    fn scroll_predeadline_not_ready_still_propagates() {
        let (mut g, _) = glass_with_a11y_not_ready(FakePlatform::new(100, 100), 1);
        g.start(&spec()).unwrap();

        let error = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: 1_000,
            })
            .unwrap_err();

        assert!(matches!(error, GlassError::AccessibilityNotReady(_)));
    }

    #[test]
    fn scroll_accessibility_failure_still_propagates() {
        let mut g = glass_with_a11y_unavailable(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();

        let error = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: 1_000,
            })
            .unwrap_err();

        assert!(matches!(error, GlassError::AccessibilityUnavailable(_)));
    }

    #[test]
    fn a_wait_polls_through_an_app_that_has_not_published_its_tree_yet() {
        let (mut g, reads) = glass_with_a11y_not_ready(FakePlatform::new(100, 100), 3);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                description: None,
                role: Some(AxRole::Button),
                value: None,
                value_contains: None,
                condition: ElementCondition::Appears,
                interval_ms: 10,
                timeout_ms: 2_000,
            })
            .expect("a tree that appears inside the budget must be waited for");
        assert!(o.matched);
        assert!(
            reads.load(std::sync::atomic::Ordering::Relaxed) > 1,
            "the wait returned on its first read instead of polling"
        );
    }

    #[test]
    fn a_wait_that_never_saw_a_tree_says_so_rather_than_reporting_the_element_absent() {
        // `matched: false` would send the caller looking for a missing element; the app published
        // no tree at all, and the remedies for those two do not overlap.
        let (mut g, reads) = glass_with_a11y_not_ready(FakePlatform::new(100, 100), usize::MAX);
        g.start(&spec()).unwrap();
        let e = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                description: None,
                role: Some(AxRole::Button),
                value: None,
                value_contains: None,
                condition: ElementCondition::Appears,
                interval_ms: 10,
                timeout_ms: 100,
            })
            .expect_err("a budget spent without ever seeing a tree is not a missing element");
        assert!(matches!(e, GlassError::AccessibilityNotReady(_)), "{e}");
        // Without this the test passes on a wait that abandoned its budget on the first read,
        // which is the defect, not the fix.
        assert!(
            reads.load(std::sync::atomic::Ordering::Relaxed) > 1,
            "the wait reported without ever polling"
        );
    }

    /// A condition the fixed fake tree never satisfies, so the wait runs its full budget.
    /// Walks driven by one 600ms wait at a 20ms interval under `signal`.
    ///
    /// `None` gives the control the two pacing tests compare against: a backend with no event
    /// stream re-walks every interval, so the count is what this machine manages now. A fixed
    /// number cannot — a runner whose 20ms sleeps land at 50ms fits a third as many in.
    ///
    /// 600ms rather than the 200ms the deadline needs: the comparison is between two runs, so it
    /// carries both their jitter, and +/-1 walk is 25% of a four-sample run but 3% of a thirty-
    /// sample one.
    fn walks_in_a_paced_wait(signal: Option<fn() -> Box<dyn ChangeSignal>>) -> usize {
        let (mut g, walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            signal,
        );
        g.start(&spec()).unwrap();
        let o = g.wait_for_element(&never_matches(20, 600)).unwrap();
        assert!(!o.matched);
        walks.load(Ordering::Relaxed)
    }

    fn never_matches(interval_ms: u64, timeout_ms: u64) -> WaitElementParams {
        WaitElementParams {
            name: Some("Save".into()),
            description: None,
            role: None,
            value: None,
            value_contains: None,
            condition: ElementCondition::Checked,
            interval_ms,
            timeout_ms,
        }
    }

    #[test]
    fn a_quiet_wait_walks_once_and_looks_again_at_the_deadline() {
        // The point of the change: told nothing changed, the wait must not re-read on the
        // interval. The second walk is the deadline read — see `poll_until_with_pause`.
        let (mut g, walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            Some(|| Box::new(NeverSignals) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let o = g.wait_for_element(&never_matches(20, 120)).unwrap();

        assert!(!o.matched);
        assert_eq!(
            walks.load(Ordering::Relaxed),
            2,
            "a quiet wait re-walked on the interval"
        );
    }

    #[test]
    fn caller_deadline_caps_an_element_wait_interval_and_signal_wait() {
        let (mut g, _walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            Some(|| Box::new(NeverSignals) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();
        let started = std::time::Instant::now();

        let outcome = g
            .wait_for_element_by(&never_matches(1_000, 5_000), Deadline::from_millis(30))
            .unwrap();

        assert_eq!(outcome.timed_out_by, Some(crate::Whose::Caller));
        assert!(
            started.elapsed() < Duration::from_millis(300),
            "a 30ms caller deadline was stretched to {:?} by the 1s interval",
            started.elapsed()
        );
    }

    #[test]
    fn a_signal_that_never_blocks_does_not_spin_the_loop() {
        // `wait` must block for the timeout it is handed, and nothing in the seam can enforce that,
        // so the loop paces itself: a signal that answers instantly would otherwise turn a 200ms
        // wait into 200ms of a pegged core.
        QUIET_WITHOUT_BLOCKING_CALLS.store(0, Ordering::Relaxed);
        let (mut g, _walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            Some(|| Box::new(QuietWithoutBlocking) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let _ = g.wait_for_element(&never_matches(20, 200)).unwrap();

        // Ten intervals fit in the budget; a spin runs to thousands.
        let calls = QUIET_WITHOUT_BLOCKING_CALLS.load(Ordering::Relaxed);
        assert!(
            calls <= 40,
            "the loop spun: {calls} pauses in ten intervals"
        );
    }

    #[test]
    fn a_change_wakes_the_wait_and_it_finds_the_element() {
        // The contract, which every other test here leaves untested: they all wait for something
        // that never matches, so they would pass on a wait that skipped every read forever.
        let (mut g, walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree(), fake_tree_enabled()],
            Some(|| Box::new(SignalsOnce(true)) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                description: None,
                role: None,
                value: None,
                value_contains: None,
                condition: ElementCondition::Enabled,
                // Shorter than the quiet ceiling (10 intervals): otherwise a wait that ignored the
                // change entirely would still match on the forced re-read, and this test would
                // pass without the wake it exists to prove.
                interval_ms: 20,
                timeout_ms: 120,
            })
            .unwrap();

        assert!(o.matched, "the change was announced but never read");
        assert_eq!(walks.load(Ordering::Relaxed), 2, "one read per state");
    }

    #[test]
    fn a_signal_that_never_stops_firing_stays_bounded_by_the_deadline() {
        let n = walks_in_a_paced_wait(Some(|| Box::new(AlwaysSignals) as Box<dyn ChangeSignal>));

        // A ceiling only. Waking early removes the interval's pacing, and a chatty app then drives
        // back-to-back walks against the bus it is trying to serve; the deadline caps a paced run
        // at thirty intervals on any machine, while losing the pacing runs to thousands.
        //
        // No lower bound: this signal answers instantly, so `d.saturating_sub(~0)` is `d` and the
        // pacing arithmetic cannot be wrong here — `a_change_arriving_mid_interval_still_paces_the_next_read`
        // tells the two apart.
        assert!(
            n <= 36,
            "a chatty app drove {n} walks in 600ms at a 20ms interval"
        );
    }

    #[test]
    fn a_long_quiet_wait_reads_anyway_now_and_then() {
        // 2.5s spans two `REREAD_AFTER` ceilings: two forced reads on top of the first and the
        // deadline's. The interval is far smaller than the ceiling on purpose — a count of
        // intervals would put it at 500ms here and read five times.
        let (mut g, walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            Some(|| Box::new(NeverSignals) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let o = g.wait_for_element(&never_matches(50, 2_500)).unwrap();

        assert!(!o.matched);
        let n = walks.load(Ordering::Relaxed);
        assert!(
            (3..=6).contains(&n),
            "a quiet 2.5s wait at a 50ms interval read {n} times; the ceiling is not firing"
        );
    }

    #[test]
    fn a_wait_too_short_to_reach_the_ceiling_still_sees_an_unannounced_change() {
        // The regression this fixes: a wait whose whole budget is shorter than `REREAD_AFTER`
        // never reaches the forced read, so before the deadline read it answered from the single
        // snapshot it took before the change happened — a wrong answer, for an element on screen.
        let (mut g, walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            // Absent on the first read, present on every read after it.
            vec![fake_tree_enabled(), fake_tree_checked()],
            Some(|| Box::new(NeverSignals) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        // 600ms at a 200ms interval: three intervals, and no ceiling inside the budget.
        let o = g.wait_for_element(&never_matches(200, 600)).unwrap();

        assert!(
            o.matched,
            "answered from one stale read after {} walks",
            walks.load(Ordering::Relaxed)
        );
    }

    #[test]
    fn the_ceiling_is_wall_clock_so_a_wide_interval_does_not_push_it_out() {
        // Ten quiet intervals at the 200ms `glass_wait_for_element` default is a two-second
        // ceiling; wall-clock, the same wait sees an unannounced change inside one.
        let (mut g, _walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled(), fake_tree_checked()],
            Some(|| Box::new(NeverSignals) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let o = g.wait_for_element(&never_matches(200, 5_000)).unwrap();

        assert!(o.matched);
        assert!(
            o.elapsed_ms < 1_500,
            "took {}ms; an interval-counted ceiling would land near 2000",
            o.elapsed_ms
        );
    }

    #[test]
    fn an_interval_of_zero_does_not_subscribe() {
        // With no pause there is nothing a signal could save, and subscribing is a round-trip of
        // its own — paid, on a real backend, out of the caller's budget.
        let (mut g, walks, subscribes) = glass_with_a11y_counted_subs(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            Some(|| Box::new(NeverSignals) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let o = g.wait_for_element(&never_matches(0, 30)).unwrap();

        assert!(!o.matched);
        assert_eq!(
            subscribes.load(Ordering::Relaxed),
            0,
            "subscribed for nothing"
        );
        assert!(
            walks.load(Ordering::Relaxed) > 1,
            "an interval of 0 must keep re-reading"
        );
    }

    #[test]
    fn a_change_arriving_mid_interval_still_paces_the_next_read() {
        // The pacing is "wake early, but no sooner than one interval after the last read". A signal
        // whose change lands halfway through is what tells the two arithmetics apart: sleeping the
        // *remainder* keeps one read per interval, sleeping the elapsed time again halves the rate.
        // Sampled either side of the subject, smallest wins. One control run before it would bias
        // the comparison on a runner that gets busier as it goes: the subject, running second,
        // would look slower than the pacing made it.
        let before = walks_in_a_paced_wait(None);
        let n = walks_in_a_paced_wait(Some(|| {
            Box::new(ChangesMidInterval) as Box<dyn ChangeSignal>
        }));
        let polled = before.min(walks_in_a_paced_wait(None));

        assert!(
            n <= 36,
            "a mid-interval change drove {n} walks in 600ms at a 20ms interval"
        );
        // Sleeping the elapsed time again rather than the remainder costs a third of the rate:
        // 10ms of signal wait, then a full 20ms instead of the 10ms remaining.
        assert!(
            n * 6 >= polled * 5,
            "a mid-interval change drove {n} walks where polling drove {polled} in the same wait"
        );
    }

    #[test]
    fn a_signal_that_stops_working_falls_back_to_polling() {
        // The failure that would be invisible: a dead subscription reports no changes, which is
        // indistinguishable from a quiet app unless it says so.
        let (mut g, walks) = glass_with_a11y_counted(
            FakePlatform::new(100, 100),
            vec![fake_tree_enabled()],
            Some(|| Box::new(DeadSignal) as Box<dyn ChangeSignal>),
        );
        g.start(&spec()).unwrap();

        let o = g.wait_for_element(&never_matches(20, 80)).unwrap();

        assert!(!o.matched);
        assert!(
            walks.load(Ordering::Relaxed) > 1,
            "a dead signal must degrade to polling, not to silence"
        );
    }

    #[test]
    fn a_backend_without_a_signal_polls_exactly_as_before() {
        // Every backend but one has no event stream, and two never can. Their waits must keep the
        // behaviour they had: re-walk each interval.
        let (mut g, walks) =
            glass_with_a11y_counted(FakePlatform::new(100, 100), vec![fake_tree_enabled()], None);
        g.start(&spec()).unwrap();

        let o = g.wait_for_element(&never_matches(10, 80)).unwrap();

        assert!(!o.matched);
        assert!(
            walks.load(Ordering::Relaxed) > 1,
            "a backend with no signal stopped polling"
        );
    }

    #[test]
    fn wait_for_element_times_out_soft() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Save".into()),
                description: None,
                role: None,
                value: None,
                value_contains: None,
                condition: ElementCondition::Checked, // never true in the fixed tree
                interval_ms: 0,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!o.matched);
        assert!(o.element.is_none());
        assert_eq!(o.timed_out_by, Some(crate::Whose::Callee));
    }

    #[test]
    fn wait_for_element_names_its_own_timeout_callee() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let out = g.wait_for_element(&never_matches(0, 0)).unwrap();
        assert_eq!(out.timed_out_by, Some(crate::Whose::Callee));
    }

    #[test]
    fn wait_for_element_names_the_sequence_timeout_caller() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let out = g
            .wait_for_element_by(&never_matches(5, 1_000), Deadline::from_millis(20))
            .unwrap();
        assert_eq!(out.timed_out_by, Some(crate::Whose::Caller));
    }

    #[test]
    fn a_caller_deadline_bounds_the_first_wait_for_element_read() {
        let (mut g, seen) = glass_with_a11y_until_deadline(FakePlatform::new(100, 100));
        g.start(&spec()).unwrap();
        g.wait_for_element_by(&never_matches(0, 1_000), Deadline::from_millis(20))
            .unwrap();
        assert!(
            seen.lock().unwrap()[0].is_some(),
            "a bounded sequence must not grant the first read an unbounded exception"
        );
    }

    #[test]
    fn scroll_saturation_is_not_reported_as_a_timeout() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert_eq!(out.timed_out_by, None);
    }

    #[test]
    fn scroll_to_element_passes_the_same_deadline_to_every_snapshot() {
        let (mut g, seen) = glass_with_a11y_deadline_log(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        g.scroll_to_element_by(
            &ScrollToElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: 2_000,
            },
            Deadline::from_millis(1_000),
        )
        .unwrap();

        let seen = seen.lock().unwrap();
        assert!(seen.len() > 1);
        assert!(seen.iter().all(|deadline| *deadline == seen[0]));
    }

    #[test]
    fn a_spent_caller_deadline_starts_no_settle_capture() {
        let captures = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![Frame::solid(4, 4, [0, 0, 0, 255])])
            .with_capture_log(captures.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let error = g
            .wait_stable_by(
                &WaitStableParams {
                    interval_ms: 0,
                    settle_frames: 2,
                    tolerance: 0,
                    timeout_ms: 1_000,
                    stability_region: None,
                    ignore: Vec::new(),
                    window: None,
                },
                Deadline::from_millis(0),
            )
            .unwrap_err();
        assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted));
        assert!(captures.lock().unwrap().is_empty());
    }

    #[test]
    fn sequence_deadline_during_settle_is_an_error_not_soft_settled_false() {
        let captures = Arc::new(Mutex::new(Vec::new()));
        let platform = FakePlatform::new(4, 4)
            .with_frames(vec![
                Frame::solid(4, 4, [0, 0, 0, 255]),
                Frame::solid(4, 4, [1, 1, 1, 255]),
            ])
            .with_capture_log(captures.clone());
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let error = g
            .wait_stable_by(
                &WaitStableParams {
                    interval_ms: 50,
                    settle_frames: 3,
                    tolerance: 0,
                    timeout_ms: 1_000,
                    stability_region: Some(Region {
                        x: 0,
                        y: 0,
                        width: 2,
                        height: 2,
                    }),
                    ignore: Vec::new(),
                    window: None,
                },
                Deadline::from_millis(20),
            )
            .unwrap_err();
        assert_eq!(error.bound(), Some(crate::BoundKind::NotStarted));
        assert_eq!(captures.lock().unwrap().len(), 1);
    }

    #[test]
    fn wait_for_element_disappears_is_matched_when_absent() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree_enabled());
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_element(&WaitElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value: None,
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
                description: None,
                role: None,
                value: None,
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
                description: None,
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
    fn scroll_to_element_reveals_an_unnamed_description_match_and_returns_its_id() {
        let before = fake_tree();
        let mut revealed = fake_tree();
        let field = &mut revealed.root.children[0];
        field.id = AxNodeId(35);
        field.role = AxRole::TextField;
        field.name = None;
        field.description = Some("Search settings".into());

        let (mut g, _) =
            glass_with_a11y_counted(FakePlatform::new(100, 100), vec![before, revealed], None);
        g.start(&spec()).unwrap();

        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: None,
                description: Some("Search settings".into()),
                role: Some(AxRole::TextField),
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();

        assert!(out.matched);
        assert_eq!(out.steps, 1);
        let element = out.element.expect("matched element");
        assert_eq!(element.id, AxNodeId(1));
        assert_eq!(element.description.as_deref(), Some("Search settings"));
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
                description: None,
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
        let tree = tree_with(100, 100, vec![ax_node(1, AxRole::Button, None, vec![])]);
        let mut g = glass_with_a11y(platform, tree);
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: None,
                description: None,
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
                description: None,
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

    /// `reversed` says whether the match came from the second, opposite sweep. Every existing
    /// case returns through the tail, which hardcodes `true`, so the flag is only observable
    /// from a return *inside* the loop — here, a match on the first direction.
    #[test]
    fn scroll_to_element_reports_not_reversed_when_found_on_the_first_sweep() {
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
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: SCROLL_TO_DEFAULT_TIMEOUT_MS,
            })
            .unwrap();
        assert!(out.matched);
        assert!(
            !out.reversed,
            "found on the primary sweep, so the opposite one never ran"
        );
    }

    /// Either bound ends the sweep on its own. A zero timeout is spent before the first step,
    /// so nothing is scrolled — which a conjunction of the two bounds would not honour, since
    /// the step count is still far below its cap.
    #[test]
    fn scroll_to_element_stops_on_the_timeout_alone() {
        let mut g = glass_with_a11y(FakePlatform::new(100, 100), fake_tree());
        g.start(&spec()).unwrap();
        let out = g
            .scroll_to_element(&ScrollToElementParams {
                name: Some("Ghost".into()),
                description: None,
                role: None,
                value_contains: None,
                direction: Some(ScrollDirection::Down),
                anchor: None,
                step: SCROLL_TO_DEFAULT_STEP,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!out.matched);
        assert_eq!(out.steps, 0, "the timeout is spent before the first scroll");
        assert!(!out.reversed, "it gave up on the primary sweep");
    }

    /// The anchor is clamped to the last addressable pixel, so an element whose centre lies
    /// past the window edge still yields a point inside it.
    #[test]
    fn scroll_anchor_clamps_to_the_last_pixel_inside_the_window() {
        let past = AxRect {
            x: 300,
            y: 300,
            width: 40,
            height: 40,
        };
        // Horizontal sweep anchors x at the window centre and y on the element's row.
        assert_eq!(
            scroll_anchor(ScrollDirection::Right, Some(past), 100, 100),
            (50, 99)
        );
        // Vertical sweep anchors y at the centre and x on the element's column.
        assert_eq!(
            scroll_anchor(ScrollDirection::Down, Some(past), 100, 100),
            (99, 50)
        );
        // Negative centres clamp up to zero on the same axis.
        let before = AxRect {
            x: -300,
            y: -300,
            width: 40,
            height: 40,
        };
        assert_eq!(
            scroll_anchor(ScrollDirection::Right, Some(before), 100, 100),
            (50, 0)
        );
        assert_eq!(
            scroll_anchor(ScrollDirection::Down, Some(before), 100, 100),
            (0, 50)
        );
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
                description: None,
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
                description: None,
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
                description: None,
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
                description: None,
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
                description: None,
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
        // Pixel (3,3) blinks every frame — a stand-in for a blinking caret or a clock — while
        // the rest of the 4x4 frame stays constant, so masking it leaves `until: Changes`
        // nothing to react to.
        //
        // `timeout_ms: 0` bounds the wait to one poll after the reference capture, so a
        // generous timeout outlasting the scripted frames into `FakePlatform`'s
        // repeat-forever fallback can't be what makes this pass.
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
        // (3,3) blinks and is INSIDE the watched region (2,2,2,2), so the cropped frames
        // differ every poll; only a window-relative rect translated into region-local space
        // masks it.
        //
        // Pinning the capture count to 2 (reference + one poll, via `timeout_ms: 0`) makes the
        // translation load-bearing: build the mask with no region and the rect lands outside
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
        // The baseline is saved while the corner is 10; the polled frame has it at 20,
        // otherwise identical. Unmasked, that difference keeps `until: Matches` from ever
        // being satisfied.
        //
        // Pinning the capture count to 2 (baseline save + one poll) rules out a generous
        // timeout matching by other means.
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

    /// The note means "this was already in the buffer *before* your call", and its advice —
    /// pass cursor:0 — is only right for such a line. One that arrives after the wait has taken
    /// its starting cursor sits at or past it, and must not be described that way. The empty
    /// batches put the line on the pump that happens after the poll gives up.
    #[test]
    fn wait_for_log_does_not_report_a_line_that_arrived_during_the_call() {
        let platform = FakePlatform::new(10, 10).with_log_batches(vec![
            vec![],
            vec![],
            vec![],
            vec![(Stream::Stdout, "arrived during the wait")],
        ]);
        let mut g = glass_with(platform);
        g.start(&spec()).unwrap();
        let o = g
            .wait_for_log(&WaitLogParams {
                contains: "arrived during the wait".into(),
                stream: None,
                cursor: None,
                interval_ms: 0,
                timeout_ms: 0,
            })
            .unwrap();
        assert!(!o.matched, "the poll gave up before the line landed");
        assert!(
            o.note.is_none(),
            "the line arrived during the call, so it was not buffered beforehand: {:?}",
            o.note
        );
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

    /// The overflow magnitudes decide which edge wins, and each is built from its own
    /// arithmetic. The existing tie-break is 2001 against 501, where a term being off by one
    /// changes nothing — these put the two candidates exactly one apart, so any single altered
    /// operand flips the answer. `max_by_key` keeps the *last* maximum, so a tie hands it to
    /// the later direction in the table.
    #[test]
    fn offscreen_direction_tie_breaks_on_a_single_pixel() {
        let at = |x: i32, y: i32| AxRect {
            x,
            y,
            width: 10,
            height: 10,
        };
        // Right 11 vs Down 10.
        assert_eq!(
            offscreen_direction(at(110, 109), 100, 100),
            Some(ScrollDirection::Right)
        );
        // Left 11 vs Up 10.
        assert_eq!(
            offscreen_direction(at(-20, -19), 100, 100),
            Some(ScrollDirection::Left)
        );
        // Right 11 vs Up 10.
        assert_eq!(
            offscreen_direction(at(110, -19), 100, 100),
            Some(ScrollDirection::Right)
        );
        // Left 11 vs Down 10.
        assert_eq!(
            offscreen_direction(at(-20, 109), 100, 100),
            Some(ScrollDirection::Left)
        );
        // And the other way round on each pair, so "always the first row" is wrong too.
        assert_eq!(
            offscreen_direction(at(109, 110), 100, 100),
            Some(ScrollDirection::Down)
        );
        assert_eq!(
            offscreen_direction(at(-19, -20), 100, 100),
            Some(ScrollDirection::Up)
        );

        // Exact ties. Because the last maximum wins, a later row keeps a tie — so a later row
        // losing one pixel is only visible from a tie, never from a margin it already leads by.
        assert_eq!(
            offscreen_direction(at(110, 110), 100, 100),
            Some(ScrollDirection::Down),
            "Right and Down both overflow by 11; the later row keeps the tie"
        );
        assert_eq!(
            offscreen_direction(at(110, -20), 100, 100),
            Some(ScrollDirection::Up),
            "Right and Up both overflow by 11; the later row keeps the tie"
        );
    }

    /// Each edge is half-open in its own direction: touching it is still on-screen, one past it
    /// is not.
    #[test]
    fn offscreen_direction_boundaries_are_exact() {
        let at = |x: i32, y: i32| AxRect {
            x,
            y,
            width: 10,
            height: 10,
        };
        // x == win_w is off; one less still intersects.
        assert_eq!(
            offscreen_direction(at(100, 50), 100, 100),
            Some(ScrollDirection::Right)
        );
        assert_eq!(offscreen_direction(at(99, 50), 100, 100), None);
        // x + w == 0 is off; one more still intersects.
        assert_eq!(
            offscreen_direction(at(-10, 50), 100, 100),
            Some(ScrollDirection::Left)
        );
        assert_eq!(offscreen_direction(at(-9, 50), 100, 100), None);
        // Same on the vertical axis, Up included.
        assert_eq!(
            offscreen_direction(at(50, 100), 100, 100),
            Some(ScrollDirection::Down)
        );
        assert_eq!(offscreen_direction(at(50, 99), 100, 100), None);
        assert_eq!(
            offscreen_direction(at(50, -10), 100, 100),
            Some(ScrollDirection::Up)
        );
        assert_eq!(offscreen_direction(at(50, -9), 100, 100), None);
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
