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
