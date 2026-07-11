//! `Glass` clipboard get/set.
use super::*;

impl Glass {
    pub fn get_clipboard(&mut self) -> Result<String> {
        self.active_mut()?.platform.get_clipboard()
    }

    pub fn set_clipboard(&mut self, text: &str) -> Result<()> {
        let t = std::time::Instant::now();
        let result = self.set_clipboard_inner(text);
        self.emit_audit(
            &crate::audit::Actuation::ClipboardSet { text },
            crate::audit::AuditOutcome::from_result(&result),
            t.elapsed(),
        );
        result
    }

    fn set_clipboard_inner(&mut self, text: &str) -> Result<()> {
        self.active_mut()?.platform.set_clipboard(text)
    }
}

#[cfg(test)]
mod tests {
    use crate::session::test_support::*;

    #[test]
    fn default_clipboard_is_unsupported() {
        // A Platform impl with no clipboard override returns Unsupported for both
        // get_clipboard and set_clipboard.
        let mut p = BareMinPlatform;
        let get_err = p.get_clipboard().unwrap_err();
        assert!(
            matches!(get_err, GlassError::Unsupported(_)),
            "get_clipboard: {get_err}"
        );
        let set_err = p.set_clipboard("hello").unwrap_err();
        assert!(
            matches!(set_err, GlassError::Unsupported(_)),
            "set_clipboard: {set_err}"
        );
    }

    #[test]
    fn clipboard_set_get_roundtrip() {
        // FakePlatform has an in-memory clipboard; Glass::set_clipboard/get_clipboard
        // are pass-throughs that require an active session.
        let mut g = glass_with(FakePlatform::new(10, 10));
        g.start(&spec()).unwrap();
        g.set_clipboard("hello glass").unwrap();
        assert_eq!(g.get_clipboard().unwrap(), "hello glass");
        // Overwrite with a new value.
        g.set_clipboard("updated").unwrap();
        assert_eq!(g.get_clipboard().unwrap(), "updated");
    }

    #[test]
    fn clipboard_requires_active_session() {
        let mut g = glass_with(FakePlatform::new(10, 10));
        // No session started — both ops should return NoActiveSession.
        assert!(matches!(
            g.get_clipboard().unwrap_err(),
            GlassError::NoActiveSession
        ));
        assert!(matches!(
            g.set_clipboard("x").unwrap_err(),
            GlassError::NoActiveSession
        ));
    }
}
