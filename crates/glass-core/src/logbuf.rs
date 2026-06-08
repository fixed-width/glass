use std::collections::VecDeque;

/// Which stream a log line came from.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Stream {
    Stdout,
    Stderr,
}

/// A single captured log line. `seq` is a monotonic id used as a read cursor.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct LogLine {
    pub seq: u64,
    pub stream: Stream,
    pub text: String,
}

/// Bounded ring buffer of captured output with cursor-based incremental reads.
pub struct LogBuffer {
    capacity: usize,
    next_seq: u64,
    lines: VecDeque<LogLine>,
}

impl LogBuffer {
    pub fn new(capacity: usize) -> Self {
        Self { capacity: capacity.max(1), next_seq: 0, lines: VecDeque::new() }
    }

    /// Append a line, evicting the oldest if at capacity.
    pub fn push(&mut self, stream: Stream, text: impl Into<String>) {
        let line = LogLine { seq: self.next_seq, stream, text: text.into() };
        self.next_seq += 1;
        self.lines.push_back(line);
        while self.lines.len() > self.capacity {
            self.lines.pop_front();
        }
    }

    /// Read up to `max` lines with `seq >= cursor`, optionally filtered by
    /// stream and substring. Returns the matched lines and the next cursor to
    /// resume from (the seq of the first unreturned matching line, or one past
    /// the last line examined).
    ///
    /// `max` is clamped to at least 1 so that a caller looping on the returned
    /// cursor always makes progress (a `max` of 0 would otherwise return the
    /// same cursor forever).
    pub fn read(
        &self,
        cursor: u64,
        max: usize,
        stream: Option<Stream>,
        contains: Option<&str>,
    ) -> (Vec<LogLine>, u64) {
        let max = max.max(1);
        let mut out = Vec::new();
        let mut next_cursor = cursor;
        for line in self.lines.iter() {
            if line.seq < cursor {
                continue;
            }
            let matches = stream.is_none_or(|s| line.stream == s)
                && contains.is_none_or(|sub| line.text.contains(sub));
            if matches {
                if out.len() >= max {
                    return (out, line.seq);
                }
                out.push(line.clone());
            }
            next_cursor = line.seq + 1;
        }
        (out, next_cursor)
    }

    /// The cursor just past the last buffered line. Pass as `cursor` to a reader
    /// to receive only lines appended after this point.
    pub fn end_cursor(&self) -> u64 {
        self.next_seq
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn buf() -> LogBuffer {
        let mut b = LogBuffer::new(100);
        b.push(Stream::Stdout, "starting");
        b.push(Stream::Stderr, "a warning");
        b.push(Stream::Stdout, "click at 3,4");
        b
    }

    #[test]
    fn reads_all_then_nothing_new() {
        let b = buf();
        let (lines, cursor) = b.read(0, 100, None, None);
        assert_eq!(lines.len(), 3);
        assert_eq!(cursor, 3);
        let (lines2, cursor2) = b.read(cursor, 100, None, None);
        assert!(lines2.is_empty());
        assert_eq!(cursor2, 3);
    }

    #[test]
    fn filters_by_stream() {
        let b = buf();
        let (lines, _) = b.read(0, 100, Some(Stream::Stderr), None);
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "a warning");
    }

    #[test]
    fn filters_by_substring() {
        let b = buf();
        let (lines, _) = b.read(0, 100, None, Some("click"));
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].text, "click at 3,4");
    }

    #[test]
    fn respects_max_and_resumes() {
        let b = buf();
        let (first, cursor) = b.read(0, 2, None, None);
        assert_eq!(first.len(), 2);
        assert_eq!(cursor, 2);
        let (second, cursor2) = b.read(cursor, 2, None, None);
        assert_eq!(second.len(), 1);
        assert_eq!(cursor2, 3);
    }

    #[test]
    fn max_zero_is_clamped_so_cursor_always_advances() {
        // A naive caller that loops `(lines, cursor) = read(cursor, max, ..)`
        // must never get a cursor that fails to advance, or it spins forever.
        // `max` is clamped to >= 1 (matching the crate's other `.max(1)` clamps),
        // so even max=0 makes progress.
        let b = buf();
        let (lines, cursor) = b.read(0, 0, None, None);
        assert_eq!(lines.len(), 1);
        assert_eq!(cursor, 1);
    }

    #[test]
    fn evicts_oldest_at_capacity() {
        let mut b = LogBuffer::new(2);
        b.push(Stream::Stdout, "one");
        b.push(Stream::Stdout, "two");
        b.push(Stream::Stdout, "three");
        let (lines, cursor) = b.read(0, 100, None, None);
        assert_eq!(lines.iter().map(|l| l.text.as_str()).collect::<Vec<_>>(), vec!["two", "three"]);
        assert_eq!(lines[0].seq, 1); // "one" (seq 0) was evicted
        assert_eq!(cursor, 3);
    }

    #[test]
    fn end_cursor_is_one_past_the_last_line() {
        let b = buf(); // 3 lines, seq 0..2
        assert_eq!(b.end_cursor(), 3);
        let (lines, _) = b.read(b.end_cursor(), 100, None, None);
        assert!(lines.is_empty(), "reading from end_cursor yields only future lines");
    }
}
