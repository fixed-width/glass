//! Wire protocol between the injected hook DLL (client) and the host store server. v2 carries
//! arbitrary clipboard formats as `(FormatKey, bytes)` items; registered formats are keyed by NAME
//! (the per-session 0xC000+ ids aren't portable across processes).

/// Bumped on any wire-format change; a mismatch is rejected (never guessed).
pub const VERSION: u8 = 2;

/// Per-item byte cap (32 MiB — large enough for images). Oversize is rejected, never truncated.
pub const MAX_ITEM_BYTES: usize = 32 << 20;
/// Aggregate cap across one SetAll/Items message.
pub const MAX_TOTAL_BYTES: usize = 64 << 20;
/// Cap on a registered-format NAME (UTF-8 bytes).
pub const MAX_NAME_BYTES: usize = 256;

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum FormatKey {
    Standard(u32),
    Named(String),
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Request {
    SetAll(Vec<(FormatKey, Vec<u8>)>),
    List,
    Get(FormatKey),
    GetAll,
    Empty,
    Seq,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Response {
    Formats(Vec<FormatKey>),
    Bytes(Option<Vec<u8>>),
    Items(Vec<(FormatKey, Vec<u8>)>),
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

struct Reader<'a> {
    b: &'a [u8],
    i: usize,
}
impl<'a> Reader<'a> {
    fn new(b: &'a [u8]) -> Self {
        Self { b, i: 0 }
    }
    fn u8(&mut self) -> Result<u8, ProtoError> {
        let v = *self.b.get(self.i).ok_or(ProtoError::Truncated)?;
        self.i += 1;
        Ok(v)
    }
    fn u32(&mut self) -> Result<u32, ProtoError> {
        let s = self
            .b
            .get(self.i..self.i + 4)
            .ok_or(ProtoError::Truncated)?;
        self.i += 4;
        Ok(u32::from_le_bytes([s[0], s[1], s[2], s[3]]))
    }
    fn u64(&mut self) -> Result<u64, ProtoError> {
        let s = self
            .b
            .get(self.i..self.i + 8)
            .ok_or(ProtoError::Truncated)?;
        self.i += 8;
        let mut a = [0u8; 8];
        a.copy_from_slice(s);
        Ok(u64::from_le_bytes(a))
    }
    fn bytes(&mut self, cap: usize) -> Result<Vec<u8>, ProtoError> {
        let n = self.u32()? as usize;
        if n > cap {
            return Err(ProtoError::TooLarge(n));
        }
        let s = self
            .b
            .get(self.i..self.i + n)
            .ok_or(ProtoError::Truncated)?;
        self.i += n;
        Ok(s.to_vec())
    }
    fn key(&mut self) -> Result<FormatKey, ProtoError> {
        match self.u8()? {
            0 => Ok(FormatKey::Standard(self.u32()?)),
            1 => {
                let b = self.bytes(MAX_NAME_BYTES)?;
                Ok(FormatKey::Named(
                    String::from_utf8(b).map_err(|_| ProtoError::Utf8)?,
                ))
            }
            t => Err(ProtoError::BadTag(t)),
        }
    }
    fn items(&mut self) -> Result<Vec<(FormatKey, Vec<u8>)>, ProtoError> {
        let count = self.u32()? as usize;
        if count > 4096 {
            return Err(ProtoError::TooLarge(count));
        }
        let mut out = Vec::new();
        let mut total = 0usize;
        for _ in 0..count {
            let k = self.key()?;
            let v = self.bytes(MAX_ITEM_BYTES)?;
            total = total.saturating_add(v.len());
            if total > MAX_TOTAL_BYTES {
                return Err(ProtoError::TooLarge(total));
            }
            out.push((k, v));
        }
        Ok(out)
    }
}

fn put_u32(o: &mut Vec<u8>, v: u32) {
    o.extend_from_slice(&v.to_le_bytes());
}
fn put_bytes(o: &mut Vec<u8>, b: &[u8]) {
    put_u32(o, b.len() as u32);
    o.extend_from_slice(b);
}
fn put_key(o: &mut Vec<u8>, k: &FormatKey) {
    match k {
        FormatKey::Standard(id) => {
            o.push(0);
            put_u32(o, *id);
        }
        FormatKey::Named(s) => {
            o.push(1);
            put_bytes(o, s.as_bytes());
        }
    }
}
fn put_items(o: &mut Vec<u8>, items: &[(FormatKey, Vec<u8>)]) {
    put_u32(o, items.len() as u32);
    for (k, v) in items {
        put_key(o, k);
        put_bytes(o, v);
    }
}

impl Request {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = vec![VERSION];
        match self {
            Request::SetAll(items) => {
                o.push(1);
                put_items(&mut o, items);
            }
            Request::List => o.push(2),
            Request::Get(k) => {
                o.push(3);
                put_key(&mut o, k);
            }
            Request::GetAll => o.push(4),
            Request::Empty => o.push(5),
            Request::Seq => o.push(6),
        }
        o
    }
    pub fn decode(b: &[u8]) -> Result<Request, ProtoError> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(ProtoError::Version(b[0]));
        }
        Ok(match r.u8()? {
            1 => Request::SetAll(r.items()?),
            2 => Request::List,
            3 => Request::Get(r.key()?),
            4 => Request::GetAll,
            5 => Request::Empty,
            6 => Request::Seq,
            t => return Err(ProtoError::BadTag(t)),
        })
    }
}

impl Response {
    pub fn encode(&self) -> Vec<u8> {
        let mut o = vec![VERSION];
        match self {
            Response::Formats(keys) => {
                o.push(1);
                put_u32(&mut o, keys.len() as u32);
                for k in keys {
                    put_key(&mut o, k);
                }
            }
            Response::Bytes(None) => o.push(2),
            Response::Bytes(Some(v)) => {
                o.push(3);
                put_bytes(&mut o, v);
            }
            Response::Items(items) => {
                o.push(4);
                put_items(&mut o, items);
            }
            Response::Ok => o.push(5),
            Response::Seq(n) => {
                o.push(6);
                o.extend_from_slice(&n.to_le_bytes());
            }
        }
        o
    }
    pub fn decode(b: &[u8]) -> Result<Response, ProtoError> {
        let mut r = Reader::new(b);
        if r.u8()? != VERSION {
            return Err(ProtoError::Version(b[0]));
        }
        Ok(match r.u8()? {
            1 => {
                let n = r.u32()? as usize;
                if n > 4096 {
                    return Err(ProtoError::TooLarge(n));
                }
                let mut keys = Vec::new();
                for _ in 0..n {
                    keys.push(r.key()?);
                }
                Response::Formats(keys)
            }
            2 => Response::Bytes(None),
            3 => Response::Bytes(Some(r.bytes(MAX_ITEM_BYTES)?)),
            4 => Response::Items(r.items()?),
            5 => Response::Ok,
            6 => Response::Seq(r.u64()?),
            t => return Err(ProtoError::BadTag(t)),
        })
    }
}

/// Length-prefix a message body for the pipe (4-byte LE length + body).
pub fn frame(body: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(4 + body.len());
    out.extend_from_slice(&(body.len() as u32).to_le_bytes());
    out.extend_from_slice(body);
    out
}

/// Parse one length-prefixed frame, returning (body, remaining). Errors if incomplete/oversize.
pub fn parse_frame(buf: &[u8]) -> Result<(Vec<u8>, &[u8]), ProtoError> {
    if buf.len() < 4 {
        return Err(ProtoError::Truncated);
    }
    let n = u32::from_le_bytes([buf[0], buf[1], buf[2], buf[3]]) as usize;
    if n > MAX_TOTAL_BYTES + 4096 {
        return Err(ProtoError::TooLarge(n));
    }
    let body = buf.get(4..4 + n).ok_or(ProtoError::Truncated)?;
    Ok((body.to_vec(), &buf[4 + n..]))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> Vec<(FormatKey, Vec<u8>)> {
        vec![
            (
                FormatKey::Standard(13),
                b"\x68\x00\x69\x00\x00\x00".to_vec(),
            ), // CF_UNICODETEXT "hi"
            (
                FormatKey::Named("HTML Format".into()),
                b"<b>hi</b>".to_vec(),
            ),
        ]
    }

    #[test]
    fn request_round_trips() {
        for r in [
            Request::List,
            Request::GetAll,
            Request::Empty,
            Request::Seq,
            Request::Get(FormatKey::Standard(13)),
            Request::Get(FormatKey::Named("PNG".into())),
            Request::SetAll(sample()),
        ] {
            assert_eq!(Request::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn response_round_trips() {
        for r in [
            Response::Ok,
            Response::Seq(42),
            Response::Bytes(None),
            Response::Bytes(Some(b"abc".to_vec())),
            Response::Formats(vec![
                FormatKey::Standard(13),
                FormatKey::Named("HTML Format".into()),
            ]),
            Response::Items(sample()),
        ] {
            assert_eq!(Response::decode(&r.encode()).unwrap(), r);
        }
    }

    #[test]
    fn rejects_version_skew_truncation_bad_tag() {
        let mut b = Request::List.encode();
        b[0] = VERSION ^ 0xFF;
        assert!(matches!(Request::decode(&b), Err(ProtoError::Version(_))));
        assert!(Request::decode(&[]).is_err());
        assert!(Request::decode(&[VERSION]).is_err());
        assert!(Request::decode(&[VERSION, 0x7F]).is_err());
    }

    #[test]
    fn rejects_oversize_item() {
        let mut b = vec![VERSION, 1 /*SetAll*/];
        b.extend_from_slice(&1u32.to_le_bytes()); // 1 item
        b.push(0); // FormatKey tag Standard
        b.extend_from_slice(&13u32.to_le_bytes());
        b.extend_from_slice(&((MAX_ITEM_BYTES + 1) as u32).to_le_bytes()); // bytes len too big
        assert!(matches!(Request::decode(&b), Err(ProtoError::TooLarge(_))));
    }

    #[test]
    fn rejects_oversize_item_count() {
        // count > 4096 is rejected BEFORE any per-item allocation (no pre-alloc amplification).
        let mut b = vec![VERSION, 1 /*SetAll*/];
        b.extend_from_slice(&5000u32.to_le_bytes()); // huge count, no item bytes follow
        assert!(matches!(Request::decode(&b), Err(ProtoError::TooLarge(_))));
        // same guard on the Formats response.
        let mut f = vec![VERSION, 1 /*Formats*/];
        f.extend_from_slice(&5000u32.to_le_bytes());
        assert!(matches!(Response::decode(&f), Err(ProtoError::TooLarge(_))));
    }

    #[test]
    fn rejects_oversize_name() {
        // a Named key length over MAX_NAME_BYTES is rejected before the name bytes are read.
        let mut b = vec![VERSION, 3 /*Get*/, 1 /*Named*/];
        b.extend_from_slice(&((MAX_NAME_BYTES + 1) as u32).to_le_bytes());
        assert!(matches!(Request::decode(&b), Err(ProtoError::TooLarge(_))));
    }

    #[test]
    #[cfg_attr(miri, ignore)] // ~96 MiB buffer — too slow under the Miri interpreter
    fn rejects_oversize_total() {
        // 3 items each just under MAX_ITEM_BYTES → aggregate exceeds MAX_TOTAL_BYTES.
        let chunk = MAX_ITEM_BYTES - 1;
        let mut b = vec![VERSION, 1 /*SetAll*/];
        b.extend_from_slice(&3u32.to_le_bytes());
        for _ in 0..3 {
            b.push(0); // Standard
            b.extend_from_slice(&1u32.to_le_bytes()); // id
            b.extend_from_slice(&(chunk as u32).to_le_bytes());
            b.resize(b.len() + chunk, 0u8);
        }
        assert!(matches!(Request::decode(&b), Err(ProtoError::TooLarge(_))));
    }

    #[test]
    fn frame_then_parse_round_trips() {
        let payload = Request::SetAll(sample()).encode();
        let framed = frame(&payload);
        let (got, rest) = parse_frame(&framed).unwrap();
        assert_eq!(got, payload);
        assert!(rest.is_empty());
    }
}
