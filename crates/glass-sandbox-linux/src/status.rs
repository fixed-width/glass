use std::os::fd::{AsRawFd, OwnedFd, RawFd};

const MAX_PENDING: usize = 64 * 1024;

/// Pipe whose writer is inherited by Bubblewrap and whose reader stays in glass.
pub struct BwrapStatusPipe {
    reader: OwnedFd,
    writer: OwnedFd,
}

/// Nonblocking parser for Bubblewrap JSON status lines.
pub struct BwrapStatusReader {
    reader: OwnedFd,
    pending: Vec<u8>,
    eof: bool,
    child_pid: Option<u32>,
}

impl BwrapStatusPipe {
    pub fn new() -> std::io::Result<Self> {
        let (reader, writer) = rustix::pipe::pipe_with(
            rustix::pipe::PipeFlags::NONBLOCK | rustix::pipe::PipeFlags::CLOEXEC,
        )?;
        rustix::io::fcntl_setfd(&writer, rustix::io::FdFlags::empty())?;
        Ok(Self { reader, writer })
    }

    pub fn writer_fd(&self) -> RawFd {
        self.writer.as_raw_fd()
    }

    pub fn into_reader(self) -> BwrapStatusReader {
        drop(self.writer);
        BwrapStatusReader {
            reader: self.reader,
            pending: Vec::new(),
            eof: false,
            child_pid: None,
        }
    }
}

impl BwrapStatusReader {
    pub fn poll_child_pid(&mut self) -> std::io::Result<Option<u32>> {
        if self.child_pid.is_some() || self.eof {
            return Ok(self.child_pid);
        }
        let mut chunk = [0_u8; 4096];
        loop {
            match rustix::io::read(&self.reader, &mut chunk) {
                Ok(0) => {
                    self.eof = true;
                    break;
                }
                Ok(bytes) => {
                    if self.pending.len().saturating_add(bytes) > MAX_PENDING {
                        return Err(std::io::Error::new(
                            std::io::ErrorKind::InvalidData,
                            "bubblewrap status exceeded 64 KiB",
                        ));
                    }
                    self.pending.extend_from_slice(&chunk[..bytes]);
                    self.parse_complete_lines();
                    if self.child_pid.is_some() {
                        break;
                    }
                }
                Err(rustix::io::Errno::AGAIN) => break,
                Err(error) => return Err(error.into()),
            }
        }
        if self.child_pid.is_none() {
            self.parse_complete_lines();
        }
        Ok(self.child_pid)
    }

    fn parse_complete_lines(&mut self) {
        while let Some(newline) = self.pending.iter().position(|byte| *byte == b'\n') {
            let line = self.pending.drain(..=newline).collect::<Vec<_>>();
            let Ok(value) = serde_json::from_slice::<serde_json::Value>(&line) else {
                continue;
            };
            let Some(pid) = value.get("child-pid").and_then(serde_json::Value::as_u64) else {
                continue;
            };
            if let Ok(pid) = u32::try_from(pid)
                && pid > 0
            {
                self.child_pid = Some(pid);
                break;
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsFd;

    fn reader_and_writer() -> (BwrapStatusReader, OwnedFd) {
        let pipe = BwrapStatusPipe::new().unwrap();
        let BwrapStatusPipe { reader, writer } = pipe;
        (
            BwrapStatusReader {
                reader,
                pending: Vec::new(),
                eof: false,
                child_pid: None,
            },
            writer,
        )
    }

    fn write_all(fd: &OwnedFd, mut bytes: &[u8]) {
        while !bytes.is_empty() {
            let written = rustix::io::write(fd, bytes).unwrap();
            bytes = &bytes[written..];
        }
    }

    #[test]
    fn split_line_is_parsed_only_after_newline_arrives() {
        let (mut reader, writer) = reader_and_writer();
        write_all(&writer, br#"{"child-pid":4"#);
        assert_eq!(reader.poll_child_pid().unwrap(), None);
        write_all(&writer, b"2}\n");
        assert_eq!(reader.poll_child_pid().unwrap(), Some(42));
    }

    #[test]
    fn parser_ignores_unknown_malformed_and_invalid_pid_lines() {
        let (mut reader, writer) = reader_and_writer();
        write_all(
            &writer,
            b"{\"version\":1}\nnot-json\n{\"child-pid\":0}\n{\"child-pid\":4294967296}\n{\"child-pid\":7}\n",
        );
        assert_eq!(reader.poll_child_pid().unwrap(), Some(7));
    }

    #[test]
    fn first_valid_pid_is_stable() {
        let (mut reader, writer) = reader_and_writer();
        write_all(&writer, b"{\"child-pid\":7}\n{\"child-pid\":8}\n");
        assert_eq!(reader.poll_child_pid().unwrap(), Some(7));
        assert_eq!(reader.poll_child_pid().unwrap(), Some(7));
    }

    #[test]
    fn eof_partial_line_is_never_accepted_and_repoll_is_deterministic() {
        let (mut reader, writer) = reader_and_writer();
        write_all(&writer, b"{\"child-pid\":9}");
        drop(writer);
        assert_eq!(reader.poll_child_pid().unwrap(), None);
        assert_eq!(reader.poll_child_pid().unwrap(), None);
    }

    #[test]
    fn pending_status_over_64_kib_is_rejected() {
        let (mut reader, writer) = reader_and_writer();
        let chunk = vec![b'x'; MAX_PENDING];
        write_all(&writer, &chunk[..8192]);
        assert_eq!(reader.poll_child_pid().unwrap(), None);
        for part in chunk[8192..].chunks(8192) {
            write_all(&writer, part);
            let _ = reader.poll_child_pid();
        }
        write_all(&writer, b"x");
        assert_eq!(
            reader.poll_child_pid().unwrap_err().kind(),
            std::io::ErrorKind::InvalidData
        );
    }

    #[test]
    fn reader_is_cloexec_and_writer_is_inheritable_while_owned() {
        let pipe = BwrapStatusPipe::new().unwrap();
        let reader_flags = rustix::io::fcntl_getfd(pipe.reader.as_fd()).unwrap();
        let writer_flags = rustix::io::fcntl_getfd(pipe.writer.as_fd()).unwrap();
        assert!(reader_flags.contains(rustix::io::FdFlags::CLOEXEC));
        assert!(!writer_flags.contains(rustix::io::FdFlags::CLOEXEC));
    }

    #[test]
    fn into_reader_closes_parent_writer_and_drops_close_all_descriptors() {
        let pipe = BwrapStatusPipe::new().unwrap();
        let reader_fd = pipe.reader.as_raw_fd();
        let writer_fd = pipe.writer.as_raw_fd();
        let reader_path = format!("/proc/self/fd/{reader_fd}");
        let writer_path = format!("/proc/self/fd/{writer_fd}");
        let reader_target = std::fs::read_link(&reader_path).unwrap();
        let writer_target = std::fs::read_link(&writer_path).unwrap();
        let reader = pipe.into_reader();
        assert_ne!(std::fs::read_link(&writer_path).ok(), Some(writer_target));
        assert_eq!(std::fs::read_link(&reader_path).unwrap(), reader_target);
        drop(reader);
        assert_ne!(std::fs::read_link(&reader_path).ok(), Some(reader_target));
    }

    #[test]
    fn dropping_pipe_closes_both_descriptors() {
        let pipe = BwrapStatusPipe::new().unwrap();
        let reader_fd = pipe.reader.as_raw_fd();
        let writer_fd = pipe.writer.as_raw_fd();
        let reader_path = format!("/proc/self/fd/{reader_fd}");
        let writer_path = format!("/proc/self/fd/{writer_fd}");
        let reader_target = std::fs::read_link(&reader_path).unwrap();
        let writer_target = std::fs::read_link(&writer_path).unwrap();
        drop(pipe);
        assert_ne!(std::fs::read_link(&reader_path).ok(), Some(reader_target));
        assert_ne!(std::fs::read_link(&writer_path).ok(), Some(writer_target));
    }
}
