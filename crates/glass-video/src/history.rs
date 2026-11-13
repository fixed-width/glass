use crate::movie::{MUX_RESERVE, Sample};
use glass_core::{GlassError, Result, RollingOptions, RollingWindow};
use std::collections::VecDeque;

const MAX_SAMPLES: usize = 1024;

struct Pending {
    window: RollingWindow,
    samples: Vec<Sample>,
    bytes: u64,
    full: bool,
}

pub(crate) struct History {
    options: RollingOptions,
    samples: VecDeque<Sample>,
    bytes: u64,
    keyframe_ms: u64,
    group_bytes: u64,
    pending: Option<Pending>,
}

fn error(reason: &str) -> GlassError {
    GlassError::Backend(reason.into())
}

impl History {
    pub(crate) fn new(options: RollingOptions) -> Self {
        Self {
            options,
            samples: VecDeque::new(),
            bytes: 0,
            keyframe_ms: 0,
            group_bytes: 0,
            pending: None,
        }
    }

    pub(crate) fn keyframe_due(&self, now: u64) -> bool {
        self.samples.is_empty()
            || now.saturating_sub(self.keyframe_ms) >= 1000
            || self.group_bytes >= self.options.max_bytes / 8
    }

    pub(crate) fn push(&mut self, sample: Sample) -> Result<()> {
        let size = sample.bytes.len() as u64;
        if size + MUX_RESERVE > self.options.max_bytes {
            return Err(error("an encoded frame exceeds the history byte allowance"));
        }
        let now = sample.timestamp;
        if let Some(pending) = &mut self.pending
            && !pending.full
        {
            if pending.bytes + size + MUX_RESERVE <= self.options.max_bytes
                && pending.samples.len() < MAX_SAMPLES
                && now.saturating_sub(pending.window.start_ms) < 60_000
            {
                pending.bytes += size;
                pending.samples.push(sample.clone());
            } else {
                pending.full = true;
                if now < pending.window.end_ms {
                    pending.window.end_ms = pending
                        .samples
                        .last()
                        .map_or(pending.window.start_ms + 1, |s| s.timestamp + 1);
                    pending.window.reason = Some("byte_or_duration_limit".into());
                    pending.window.truncated = true;
                }
            }
        }
        if sample.sync {
            self.keyframe_ms = now;
            self.group_bytes = 0;
        }
        self.group_bytes += size;
        if self.samples.is_empty() && !sample.sync {
            return Ok(());
        }
        self.bytes += size;
        self.samples.push_back(sample);
        let cutoff = now.saturating_sub(self.options.history_ms);
        loop {
            let next = self
                .samples
                .iter()
                .enumerate()
                .skip(1)
                .find(|(_, s)| s.sync)
                .map(|(i, _)| i);
            let Some(next) = next else {
                break;
            };
            if self.samples[next].timestamp > cutoff
                && self.bytes + MUX_RESERVE <= self.options.max_bytes
                && self.samples.len() <= MAX_SAMPLES
            {
                break;
            }
            for _ in 0..next {
                if let Some(old) = self.samples.pop_front() {
                    self.bytes -= old.bytes.len() as u64;
                }
            }
        }
        if self.bytes + MUX_RESERVE > self.options.max_bytes || self.samples.len() > MAX_SAMPLES {
            self.samples.clear();
            self.bytes = 0;
        }
        Ok(())
    }

    pub(crate) fn bytes(&self) -> u64 {
        self.bytes
    }
    pub(crate) fn pending_bytes(&self) -> u64 {
        self.pending.as_ref().map_or(0, |p| p.bytes)
    }
    pub(crate) fn latest_ms(&self) -> u64 {
        self.samples.back().map_or(0, |s| s.timestamp)
    }
    pub(crate) fn retained_ms(&self, _now: u64) -> u64 {
        self.samples
            .front()
            .map_or(0, |first| self.latest_ms().saturating_sub(first.timestamp))
    }
    pub(crate) fn pending(&self, now: u64) -> Option<RollingWindow> {
        self.pending.as_ref().map(|p| {
            let mut window = p.window.clone();
            window.ready = now >= window.end_ms;
            window.captured_until_ms = p
                .samples
                .last()
                .map_or(window.start_ms, |s| s.timestamp.min(window.end_ms));
            window.bytes = p
                .samples
                .iter()
                .take_while(|s| s.timestamp < window.end_ms)
                .map(|s| s.bytes.len() as u64)
                .sum();
            window
        })
    }

    pub(crate) fn mark(&mut self, id: &str, now: u64, tail_ms: u64) -> Result<RollingWindow> {
        if id.len() != 32 || !id.bytes().all(|b| b.is_ascii_hexdigit()) || tail_ms > 5000 {
            return Err(error("invalid buffer mark options"));
        }
        let cutoff = now.saturating_sub(self.options.history_ms);
        let start = self
            .samples
            .iter()
            .rev()
            .find(|s| s.sync && s.timestamp <= cutoff)
            .or_else(|| self.samples.front())
            .map(|s| s.timestamp)
            .ok_or_else(|| error("buffer is waiting for an encoded keyframe"))?;
        let end = (now + tail_ms).max(start + 1);
        if let Some(pending) = &mut self.pending {
            if pending.window.id != id {
                return Err(error("buffer clip identity mismatch"));
            }
            if start > pending.window.end_ms {
                return Err(error(
                    "pending clip does not overlap this mark; save it first",
                ));
            }
            if pending.full || end.saturating_sub(pending.window.start_ms) > 60_000 {
                return Err(error(
                    "pending clip reached its allowance; save it before another mark",
                ));
            }
            pending.window.end_ms = pending.window.end_ms.max(end);
            pending.window.marked_at_ms = now;
        } else {
            let samples: Vec<_> = self
                .samples
                .iter()
                .filter(|s| s.timestamp >= start)
                .cloned()
                .collect();
            let bytes = samples.iter().map(|s| s.bytes.len() as u64).sum();
            self.pending = Some(Pending {
                window: RollingWindow {
                    id: id.into(),
                    start_ms: start,
                    end_ms: end,
                    marked_at_ms: now,
                    captured_until_ms: now,
                    bytes,
                    ready: false,
                    truncated: now < self.options.history_ms || start > cutoff,
                    reason: None,
                },
                samples,
                bytes,
                full: false,
            });
        }
        self.pending(now)
            .ok_or_else(|| error("marked clip unavailable"))
    }

    pub(crate) fn fail(&mut self, reason: &str) {
        if let Some(pending) = &mut self.pending {
            let end = pending
                .samples
                .last()
                .map_or(pending.window.start_ms + 1, |s| s.timestamp + 1);
            pending.window.end_ms = pending.window.end_ms.min(end);
            pending.window.reason = Some(reason.into());
            pending.window.truncated = true;
            pending.full = true;
        }
    }

    pub(crate) fn take(
        &mut self,
        now: u64,
        stopping: bool,
    ) -> Result<(RollingWindow, Vec<Sample>)> {
        let ready = self
            .pending(now)
            .ok_or_else(|| error("no marked clip awaits saving"))?;
        if !stopping && !ready.ready {
            return Err(error("marked clip is still collecting its tail"));
        }
        let mut pending = self
            .pending
            .take()
            .ok_or_else(|| error("marked clip unavailable"))?;
        if stopping && now < pending.window.end_ms {
            pending.window.end_ms = now.max(pending.window.start_ms + 1);
            pending.window.truncated = true;
            pending.window.reason = Some("buffer_stopped_before_tail".into());
        }
        pending
            .samples
            .retain(|s| s.timestamp < pending.window.end_ms);
        pending.window.bytes = pending.samples.iter().map(|s| s.bytes.len() as u64).sum();
        pending.window.captured_until_ms = pending
            .samples
            .last()
            .map_or(pending.window.start_ms, |s| s.timestamp);
        pending.window.ready = true;
        Ok((pending.window, pending.samples))
    }

    pub(crate) fn discard(&mut self) {
        self.pending = None;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    fn history() -> History {
        History::new(RollingOptions {
            id: "a".repeat(32),
            history_ms: 1000,
            max_bytes: 8 * 1024 * 1024,
        })
    }
    fn sample(timestamp: u64, sync: bool) -> Sample {
        Sample {
            timestamp,
            sync,
            bytes: Bytes::from(vec![timestamp as u8; 100]),
        }
    }

    #[test]
    fn eviction_keeps_the_keyframe_preceding_the_requested_history() {
        let mut h = history();
        for ms in (0..=3000).step_by(100) {
            h.push(sample(ms, ms % 1000 == 0)).unwrap();
        }
        assert_eq!(h.samples.front().unwrap().timestamp, 2000);
        assert!(h.samples.front().unwrap().sync);
        assert_eq!(h.bytes(), 1100);
    }

    #[test]
    fn overlapping_marks_extend_one_clip_and_disjoint_marks_preserve_it() {
        let mut h = history();
        let id = "b".repeat(32);
        for ms in (0..=1000).step_by(100) {
            h.push(sample(ms, ms % 1000 == 0)).unwrap();
        }
        let first = h.mark(&id, 1000, 200).unwrap();
        for ms in (1100..=1500).step_by(100) {
            h.push(sample(ms, false)).unwrap();
        }
        let merged = h.mark(&id, 1500, 500).unwrap();
        assert_eq!(first.start_ms, merged.start_ms);
        assert_eq!(merged.end_ms, 2000);
        for ms in (1600..=5000).step_by(100) {
            h.push(sample(ms, ms % 1000 == 0)).unwrap();
        }
        assert!(h.mark(&id, 5000, 0).is_err());
        let (window, samples) = h.take(5000, false).unwrap();
        assert_eq!(window.end_ms, 2000);
        assert!(samples[0].sync);
        assert!(samples.iter().all(|s| s.timestamp < 2000));
    }

    #[test]
    fn byte_pressure_discards_complete_groups_and_never_exposes_a_delta_prefix() {
        let mut h = history();
        h.options.max_bytes = MUX_RESERVE + 350;
        for ms in (0..=1500).step_by(100) {
            h.push(sample(ms, ms % 300 == 0)).unwrap();
            assert!(h.bytes() <= 350);
            assert!(h.samples.front().is_none_or(|s| s.sync));
        }
    }

    #[test]
    fn a_mark_reaching_its_byte_limit_is_saveable_while_recent_history_keeps_rolling() {
        let mut h = history();
        h.options.max_bytes = MUX_RESERVE + 350;
        for ms in [0, 100, 200] {
            h.push(sample(ms, ms == 0)).unwrap();
        }
        let id = "b".repeat(32);
        h.mark(&id, 200, 5000).unwrap();
        h.push(sample(300, true)).unwrap();
        let pending = h.pending(300).unwrap();
        assert!(pending.ready && pending.truncated);
        assert!(pending.reason.is_some());
        assert!(h.mark(&id, 300, 0).is_err());
        assert_eq!(h.samples.front().unwrap().timestamp, 300);
        let (window, samples) = h.take(400, false).unwrap();
        assert_eq!(window.end_ms, 201);
        assert_eq!(samples.len(), 3);
        assert!(h.bytes() <= 350);
    }

    #[test]
    fn a_merge_beyond_the_duration_allowance_leaves_the_original_incident_unchanged() {
        let mut h = history();
        h.options.history_ms = 30_000;
        for ms in (0..=30_000).step_by(1000) {
            h.push(sample(ms, true)).unwrap();
        }
        let id = "b".repeat(32);
        let first = h.mark(&id, 30_000, 5000).unwrap();
        for ms in (31_000..=59_000).step_by(1000) {
            h.push(sample(ms, true)).unwrap();
        }
        assert!(h.mark(&id, 59_000, 5000).is_err());
        assert_eq!(h.pending(59_000).unwrap().end_ms, first.end_ms);
    }
}
