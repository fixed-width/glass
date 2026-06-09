//! Wire protocol between the injected hook DLL (client) and the host store server.

/// Protocol version. Bumped on any wire-format change; a mismatch is rejected (never guessed).
pub const VERSION: u8 = 1;

/// Hard cap on a single text payload (1 MiB of UTF-8). Larger `Set`s are rejected by the
/// reader, not truncated — no silent loss.
pub const MAX_TEXT_BYTES: usize = 1 << 20;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    Get,
    Set(String),
    Empty,
    Seq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Text(Option<String>),
    Ok,
    Seq(u64),
}

#[derive(Debug, PartialEq, Eq)]
pub enum ProtoError {
    Version(u8),
    Truncated,
    BadTag(u8),
    TooLarge(usize),
    Utf8,
}

fn put_str(out: &mut Vec<u8>, s: &str) {
    let b = s.as_bytes();
    out.extend_from_slice(&(b.len() as u32).to_le_bytes());
    out.extend_from_slice(b);
}

fn take_str(buf: &[u8]) -> Result<String, ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if n > MAX_TEXT_BYTES {
        return Err(ProtoError::TooLarge(n));
    }
    let body = &buf[4..];
    if body.len() < n {
        return Err(ProtoError::Truncated);
    }
    String::from_utf8(body[..n].to_vec()).map_err(|_| ProtoError::Utf8)
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Request::Get => out.push(1),
            Request::Empty => out.push(2),
            Request::Set(s) => {
                out.push(3);
                put_str(&mut out, s);
            }
            Request::Seq => out.push(4),
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Request, ProtoError> {
        if bytes.len() < 2 {
            return Err(ProtoError::Truncated);
        }
        if bytes[0] != VERSION {
            return Err(ProtoError::Version(bytes[0]));
        }
        match bytes[1] {
            1 => Ok(Request::Get),
            2 => Ok(Request::Empty),
            3 => Ok(Request::Set(take_str(&bytes[2..])?)),
            4 => Ok(Request::Seq),
            t => Err(ProtoError::BadTag(t)),
        }
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut out = vec![VERSION];
        match self {
            Response::Text(None) => out.push(1),
            Response::Text(Some(s)) => {
                out.push(2);
                put_str(&mut out, s);
            }
            Response::Ok => out.push(3),
            Response::Seq(n) => {
                out.push(4);
                out.extend_from_slice(&n.to_le_bytes());
            }
        }
        out
    }

    pub fn decode(bytes: &[u8]) -> Result<Response, ProtoError> {
        if bytes.len() < 2 {
            return Err(ProtoError::Truncated);
        }
        if bytes[0] != VERSION {
            return Err(ProtoError::Version(bytes[0]));
        }
        match bytes[1] {
            1 => Ok(Response::Text(None)),
            2 => Ok(Response::Text(Some(take_str(&bytes[2..])?))),
            3 => Ok(Response::Ok),
            4 => {
                let b = bytes.get(2..10).ok_or(ProtoError::Truncated)?;
                let mut a = [0u8; 8];
                a.copy_from_slice(b);
                Ok(Response::Seq(u64::from_le_bytes(a)))
            }
            t => Err(ProtoError::BadTag(t)),
        }
    }
}

/// Length-prefix a message body for the pipe (4-byte LE length + body).
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Parse one length-prefixed frame, returning (body, remaining). Errors if incomplete.
pub fn parse_frame(buf: &[u8]) -> Result<(Vec<u8>, &[u8]), ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if n > MAX_TEXT_BYTES + 16 {
        return Err(ProtoError::TooLarge(n));
    }
    let body = buf.get(4..4 + n).ok_or(ProtoError::Truncated)?;
    Ok((body.to_vec(), &buf[4 + n..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn request_round_trips() {
        for r in [Request::Get, Request::Empty, Request::Seq, Request::Set("héllo 世界".into())] {
            let bytes = r.encode();
            assert_eq!(Request::decode(&bytes).unwrap(), r);
        }
    }

    #[test]
    fn response_round_trips() {
        for r in [
            Response::Text(None),
            Response::Text(Some("abc".into())),
            Response::Ok,
            Response::Seq(42),
        ] {
            let bytes = r.encode();
            assert_eq!(Response::decode(&bytes).unwrap(), r);
        }
    }

    #[test]
    fn rejects_version_skew() {
        let mut bytes = Request::Get.encode();
        bytes[0] = VERSION ^ 0xFF; // corrupt the version byte
        assert!(matches!(Request::decode(&bytes), Err(ProtoError::Version(_))));
    }

    #[test]
    fn rejects_truncated_and_bad_tag() {
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[VERSION]).is_err()); // version only, no tag
        assert!(Request::decode(&[VERSION, 0x7F]).is_err()); // unknown tag
        // truncated Set: version+tag+len says 5 but no bytes follow
        assert!(Request::decode(&[VERSION, 3, 5, 0, 0, 0]).is_err());
    }

    #[test]
    fn frame_then_parse_round_trips() {
        let payload = Request::Set("x".into()).encode();
        let framed = frame(&payload);
        let (got, rest) = parse_frame(&framed).unwrap();
        assert_eq!(got, payload);
        assert!(rest.is_empty());
    }
}
