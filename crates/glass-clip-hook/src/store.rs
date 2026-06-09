//! Host-owned private clipboard store (pure; no Win32). Holds an ordered set of
//! `(FormatKey, bytes)` items (source priority order: most-descriptive first) + a sequence number.

use std::sync::{Arc, Mutex};

use crate::proto::{FormatKey, Request, Response};

#[cfg(any(windows, test))]
const CF_UNICODETEXT: u32 = 13;

#[derive(Default)]
struct ClipboardState {
    items: Vec<(FormatKey, Vec<u8>)>,
    seq: u64,
}

/// The single source of truth for a contained app's clipboard, shared (cloned `Arc`) between the
/// pipe server thread and the platform's `get/set_clipboard`. Pure — no Win32.
#[derive(Clone, Default)]
pub struct PrivateClipboard(Arc<Mutex<ClipboardState>>);

impl PrivateClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn set_all(&self, items: Vec<(FormatKey, Vec<u8>)>) {
        let mut g = self.0.lock().expect("clip mutex");
        g.items = items;
        g.seq = g.seq.wrapping_add(1);
    }

    pub fn list(&self) -> Vec<FormatKey> {
        self.0.lock().expect("clip mutex").items.iter().map(|(k, _)| k.clone()).collect()
    }

    pub fn get(&self, key: &FormatKey) -> Option<Vec<u8>> {
        let g = self.0.lock().expect("clip mutex");
        g.items.iter().find(|(k, _)| k == key).map(|(_, v)| v.clone())
    }

    pub fn get_all(&self) -> Vec<(FormatKey, Vec<u8>)> {
        self.0.lock().expect("clip mutex").items.clone()
    }

    pub fn empty(&self) {
        let mut g = self.0.lock().expect("clip mutex");
        g.items.clear();
        g.seq = g.seq.wrapping_add(1);
    }

    pub fn seq(&self) -> u64 {
        self.0.lock().expect("clip mutex").seq
    }

    /// Convenience for the agent's text path: write a single `CF_UNICODETEXT` item (UTF-16 + NUL).
    #[cfg(any(windows, test))]
    pub fn set_text(&self, text: &str) {
        let bytes: Vec<u8> = crate::text::string_to_utf16_nul(text)
            .iter()
            .flat_map(|u| u.to_le_bytes())
            .collect();
        self.set_all(vec![(FormatKey::Standard(CF_UNICODETEXT), bytes)]);
    }

    /// Convenience: read the `CF_UNICODETEXT` item as a `String` (NUL-terminated UTF-16), if present.
    #[cfg(any(windows, test))]
    pub fn get_text(&self) -> Option<String> {
        let bytes = self.get(&FormatKey::Standard(CF_UNICODETEXT))?;
        let units: Vec<u16> = bytes
            .chunks_exact(2)
            .map(|c| u16::from_le_bytes([c[0], c[1]]))
            .collect();
        Some(crate::text::utf16_nul_to_string(&units))
    }

    /// The single place the server maps protocol → state, so the hook DLL and `glass_clipboard_*`
    /// stay consistent.
    pub fn apply(&self, req: Request) -> Response {
        match req {
            Request::SetAll(items) => {
                self.set_all(items);
                Response::Ok
            }
            Request::List => Response::Formats(self.list()),
            Request::Get(k) => Response::Bytes(self.get(&k)),
            Request::GetAll => Response::Items(self.get_all()),
            Request::Empty => {
                self.empty();
                Response::Ok
            }
            Request::Seq => Response::Seq(self.seq()),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::proto::{FormatKey, Request, Response};

    fn items() -> Vec<(FormatKey, Vec<u8>)> {
        vec![
            (FormatKey::Standard(13), b"u".to_vec()),
            (FormatKey::Named("HTML Format".into()), b"<b/>".to_vec()),
        ]
    }

    #[test]
    fn set_all_get_list_seq() {
        let c = PrivateClipboard::new();
        assert!(c.list().is_empty());
        let s0 = c.seq();
        c.set_all(items());
        assert!(c.seq() > s0);
        assert_eq!(c.list(), vec![FormatKey::Standard(13), FormatKey::Named("HTML Format".into())]);
        assert_eq!(c.get(&FormatKey::Standard(13)), Some(b"u".to_vec()));
        assert_eq!(c.get(&FormatKey::Named("HTML Format".into())), Some(b"<b/>".to_vec()));
        assert_eq!(c.get(&FormatKey::Standard(999)), None);
        assert_eq!(c.get_all(), items());
    }

    #[test]
    fn empty_clears_and_bumps() {
        let c = PrivateClipboard::new();
        c.set_all(items());
        let s = c.seq();
        c.empty();
        assert!(c.list().is_empty());
        assert!(c.seq() > s);
    }

    #[test]
    fn apply_dispatches_v2() {
        let c = PrivateClipboard::new();
        assert_eq!(c.apply(Request::SetAll(items())), Response::Ok);
        assert_eq!(c.apply(Request::List), Response::Formats(vec![FormatKey::Standard(13), FormatKey::Named("HTML Format".into())]));
        assert_eq!(c.apply(Request::Get(FormatKey::Standard(13))), Response::Bytes(Some(b"u".to_vec())));
        assert_eq!(c.apply(Request::Get(FormatKey::Standard(1))), Response::Bytes(None));
        assert_eq!(c.apply(Request::GetAll), Response::Items(items()));
        assert!(matches!(c.apply(Request::Seq), Response::Seq(_)));
        assert_eq!(c.apply(Request::Empty), Response::Ok);
        assert_eq!(c.apply(Request::List), Response::Formats(vec![]));
    }

    #[test]
    fn clone_shares_state() {
        let a = PrivateClipboard::new();
        let b = a.clone();
        a.set_all(items());
        assert_eq!(b.list().len(), 2);
    }

    #[test]
    fn text_helpers_round_trip_via_cf_unicodetext() {
        let c = PrivateClipboard::new();
        assert_eq!(c.get_text(), None);
        c.set_text("héllo");
        assert_eq!(c.get_text().as_deref(), Some("héllo"));
        // set_text stores exactly CF_UNICODETEXT (id 13)
        assert_eq!(c.list(), vec![FormatKey::Standard(13)]);
    }
}
