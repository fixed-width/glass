// The display-provisioning seam: where the target app renders for a session.
// `ExistingDesktop` uses the display the interactive session already presents — a real
// monitor, a dummy plug, or a virtual monitor from an indirect-display driver (e.g. mttvdd)
// on a headless box. See docs/running-on-windows.md.

use glass_core::Result;

/// Where the target app renders for a session.
pub(crate) trait DisplayProvider {
    /// Ensure a usable composited display exists; return its logical size in physical pixels.
    fn ensure(&mut self) -> Result<ProvisionedDisplay>;
}

pub(crate) struct ProvisionedDisplay {
    pub width: u32,
    pub height: u32,
}

mod existing;
pub(crate) use existing::ExistingDesktop;
