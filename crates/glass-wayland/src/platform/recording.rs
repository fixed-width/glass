use super::*;

/// A separate read-only screencopy connection to the session's owned compositor.
pub(super) struct Capture {
    conn: Connection,
    queue: EventQueue<State>,
    state: State,
    manager: ZwlrScreencopyManagerV1,
    output: wl_output::WlOutput,
}

impl Capture {
    pub(super) fn connect(socket: &Path) -> Result<Self> {
        let deadline = Instant::now() + Duration::from_secs(2);
        let (conn, globals, mut queue) = connect_bounded::<State>(
            socket,
            deadline.saturating_duration_since(Instant::now()),
            "recording connection",
        )?;
        let qh = queue.handle();
        let mut state = State {
            registry: RegistryState::new(&globals),
            output: OutputState::new(&globals, &qh),
            shm: Shm::bind(&globals, &qh)
                .map_err(|e| GlassError::CaptureFailed(format!("recording shm: {e}")))?,
            capture: CaptureScratch::default(),
        };
        let manager = globals
            .bind(&qh, 3..=3, ())
            .map_err(|e| GlassError::CaptureFailed(format!("recording screencopy: {e}")))?;
        roundtrip_until(
            &conn,
            &mut queue,
            &mut state,
            deadline,
            "recording connection",
        )?;
        let output =
            state.output.outputs().next().ok_or_else(|| {
                GlassError::CaptureFailed("owned compositor has no output".into())
            })?;
        Ok(Self {
            conn,
            queue,
            state,
            manager,
            output,
        })
    }

    pub(super) fn frame(&mut self, deadline: Deadline) -> Result<Frame> {
        if deadline.has_passed() {
            return Err(GlassError::deadline_not_started("recording capture"));
        }
        self.state.capture = CaptureScratch::default();
        let qh = self.queue.handle();
        let frame = self.manager.capture_output(0, &self.output, &qh, ());
        let mut owned = CaptureObjects {
            frame,
            buffer: None,
        };
        let until = Instant::now() + deadline.remaining().unwrap_or(Duration::from_secs(2));
        let (format, w, h, stride) = wait_for(
            &self.conn,
            &mut self.queue,
            &mut self.state,
            until,
            "recording capture",
            |s| GlassError::CaptureFailed(s.capture.no_formats()),
            |s| s.capture.advertised(),
        )?;
        let size = u64::from(stride) * u64::from(h);
        if w == 0 || h == 0 || size > 256 * 1024 * 1024 {
            return Err(GlassError::CaptureFailed(
                "invalid recording buffer dimensions".into(),
            ));
        }
        let mut pool = RawPool::new(size as usize, &self.state.shm)
            .map_err(|e| GlassError::CaptureFailed(format!("recording shm: {e}")))?;
        let buffer = pool.create_buffer(0, w as i32, h as i32, stride as i32, format, (), &qh);
        owned.frame.copy(&buffer);
        owned.buffer = Some(buffer);
        wait_for(
            &self.conn,
            &mut self.queue,
            &mut self.state,
            until,
            "recording capture",
            |_| GlassError::CaptureFailed("recording screencopy deadline elapsed".into()),
            |s| s.capture.done.take(),
        )?;
        if deadline.has_passed() {
            return Err(GlassError::caller_deadline_elapsed("recording capture"));
        }
        let rgba = crate::pixels::to_rgba(pool.mmap(), format, w, h, stride)?;
        Frame::new(w, h, rgba)
    }
}
