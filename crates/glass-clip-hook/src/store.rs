//! Host-owned private clipboard store (pure; no Win32).

use std::sync::{Arc, Mutex};

use crate::proto::{Request, Response};

#[derive(Default)]
struct ClipboardState {
    text: Option<String>,
    seq: u64,
}

/// The single source of truth for a contained app's clipboard, shared (cloned `Arc`) between
/// the pipe server thread and the platform's `get/set_clipboard`. Pure — no Win32.
#[derive(Clone, Default)]
pub struct PrivateClipboard(Arc<Mutex<ClipboardState>>);

impl PrivateClipboard {
    pub fn new() -> Self {
        Self::default()
    }

    pub fn get(&self) -> Option<String> {
        self.0.lock().expect("clip mutex").text.clone()
    }

    pub fn set(&self, text: String) {
        let mut g = self.0.lock().expect("clip mutex");
        g.text = Some(text);
        g.seq = g.seq.wrapping_add(1);
    }

    pub fn empty(&self) {
        let mut g = self.0.lock().expect("clip mutex");
        g.text = None;
        g.seq = g.seq.wrapping_add(1);
    }

    pub fn seq(&self) -> u64 {
        self.0.lock().expect("clip mutex").seq
    }

    /// Apply a wire request, returning the wire response. The single place the server maps
    /// protocol → state, so the hook DLL and `glass_clipboard_*` stay consistent.
    pub fn apply(&self, req: Request) -> Response {
        match req {
            Request::Get => Response::Text(self.get()),
            Request::Set(s) => {
                self.set(s);
                Response::Ok
            }
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

    #[test]
    fn set_get_round_trip_and_seq_bumps() {
        let c = PrivateClipboard::new();
        assert_eq!(c.get(), None);
        let s0 = c.seq();
        c.set("hi".into());
        assert_eq!(c.get(), Some("hi".into()));
        assert!(c.seq() > s0);
    }

    #[test]
    fn empty_clears_and_bumps() {
        let c = PrivateClipboard::new();
        c.set("hi".into());
        let s1 = c.seq();
        c.empty();
        assert_eq!(c.get(), None);
        assert!(c.seq() > s1);
    }

    #[test]
    fn apply_request_maps_to_responses() {
        let c = PrivateClipboard::new();
        assert_eq!(c.apply(super::super::proto::Request::Set("z".into())), super::super::proto::Response::Ok);
        assert_eq!(c.apply(super::super::proto::Request::Get), super::super::proto::Response::Text(Some("z".into())));
        assert_eq!(c.apply(super::super::proto::Request::Empty), super::super::proto::Response::Ok);
        assert_eq!(c.apply(super::super::proto::Request::Get), super::super::proto::Response::Text(None));
        assert!(matches!(c.apply(super::super::proto::Request::Seq), super::super::proto::Response::Seq(_)));
    }

    #[test]
    fn clone_shares_state() {
        let a = PrivateClipboard::new();
        let b = a.clone();
        a.set("shared".into());
        assert_eq!(b.get(), Some("shared".into())); // same Arc
    }
}
