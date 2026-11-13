use super::*;

fn buffer_size(format: wl_shm::Format, width: u32, height: u32, stride: u32) -> Result<usize> {
    let bytes_per_pixel = match format {
        wl_shm::Format::Bgr888 | wl_shm::Format::Rgb888 => 3,
        wl_shm::Format::Xrgb8888
        | wl_shm::Format::Argb8888
        | wl_shm::Format::Xbgr8888
        | wl_shm::Format::Abgr8888 => 4,
        _ => {
            return Err(GlassError::CaptureFailed(
                "unsupported recording buffer format".into(),
            ));
        }
    };
    let size = u64::from(stride) * u64::from(height);
    if width == 0
        || height == 0
        || size > 256 * 1024 * 1024
        || u64::from(stride) < u64::from(width) * bytes_per_pixel
    {
        return Err(GlassError::CaptureFailed(
            "invalid recording buffer dimensions".into(),
        ));
    }
    Ok(size as usize)
}

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
        let size = buffer_size(format, w, h, stride)?;
        let mut pool = RawPool::new(size, &self.state.shm)
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn screencopy_dimensions_validate_padding_and_allocation_limits_before_mapping() {
        let format = wl_shm::Format::Xrgb8888;
        assert_eq!(buffer_size(format, 10, 20, 48).unwrap(), 960);
        assert_eq!(
            buffer_size(format, 1024, 65_536, 4096).unwrap(),
            256 * 1024 * 1024
        );
        for (format, stride) in [
            (wl_shm::Format::Bgr888, 30),
            (wl_shm::Format::Rgb888, 30),
            (wl_shm::Format::Xrgb8888, 40),
            (wl_shm::Format::Argb8888, 40),
            (wl_shm::Format::Xbgr8888, 40),
            (wl_shm::Format::Abgr8888, 40),
        ] {
            assert_eq!(buffer_size(format, 10, 1, stride).unwrap(), stride as usize);
            assert!(buffer_size(format, 10, 1, stride - 1).is_err());
        }
        assert!(buffer_size(wl_shm::Format::Rgb565, 1, 1, 4).is_err());
        for (width, height, stride) in [
            (0, 1, 4),
            (1, 0, 4),
            (1, 1, 0),
            (2, 1, 4),
            (2, 1, 6),
            (1024, 65_537, 4096),
            (u32::MAX, 1, 4),
            (1, u32::MAX, 4),
            (1, 1, u32::MAX),
        ] {
            assert!(
                buffer_size(format, width, height, stride).is_err(),
                "{width}x{height} stride {stride}"
            );
        }
    }
}
